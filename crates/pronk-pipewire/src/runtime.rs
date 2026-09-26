mod buffers;
mod format;

use std::cell::{Cell, RefCell};
use std::num::{NonZeroU32, NonZeroU64};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

use self::buffers::{add_buffer, process_returned_buffers, publish_frame, remove_buffer};
#[cfg(test)]
use self::buffers::{configure_spa_data, fill_frame, fill_optional_frame_metadata};
use self::format::{classify_format_change, format_parameter, negotiate_buffers};
#[cfg(test)]
use self::format::{pixel_format, storage_matches};
use crate::model::BufferTracker;
use crate::node_registration::NodeRegistration;
use crate::policy_gate::{
    PolicyGate, PolicyMarkerChange, PRIVATE_NODE_POLICY_VERSION, PRIVATE_NODE_PROPERTY,
};
use crate::policy_registry_sync::PolicyRegistrySync;
use crate::remote_monitor::RemoteDisconnectMonitor;
use crate::{
    PipeWireBufferTransport, PipeWireRemote, VideoBuffer, VideoFrame, VideoNodeIdentity,
    VideoSourceConfig, VideoSourceEvent, VideoSourceRuntimeError,
};
#[cfg(test)]
use spa::param::video::{VideoFlags, VideoFormat, VideoInfoRaw};

const EVENT_QUEUE_CAPACITY: usize = 128;
// At a 120 Hz driving graph this gives PipeWire about one second to
// coalesce an activation edge before treating the client connection as lost.
const MAX_CONSECUTIVE_TRIGGER_FAILURES: u32 = 125;

type StartupSender = oneshot::Sender<Result<VideoNodeIdentity, VideoSourceRuntimeError>>;
type StartupSlot = Arc<Mutex<Option<StartupSender>>>;

pub(crate) enum Command {
    Publish {
        frame: VideoFrame,
        _permit: OwnedSemaphorePermit,
        reply: oneshot::Sender<Result<(), VideoSourceRuntimeError>>,
    },
    TriggerProcess {
        reply: oneshot::Sender<()>,
    },
    Shutdown,
}

pub(crate) struct RuntimeHandle {
    pub commands: pw::channel::Sender<Command>,
    pub startup_cancel: pw::channel::Sender<()>,
    pub events: mpsc::Receiver<VideoSourceEvent>,
    pub startup: oneshot::Receiver<Result<VideoNodeIdentity, VideoSourceRuntimeError>>,
    pub thread: JoinHandle<()>,
}

pub(crate) fn spawn(
    config: VideoSourceConfig,
    buffers: Vec<VideoBuffer>,
    remote: PipeWireRemote,
) -> Result<RuntimeHandle, std::io::Error> {
    let (commands, command_receiver) = pw::channel::channel();
    let (startup_cancel, startup_cancel_receiver) = pw::channel::channel();
    let (events_tx, events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let (startup_tx, startup) = oneshot::channel();
    let startup_slot = Arc::new(Mutex::new(Some(startup_tx)));
    let supervisor_startup = startup_slot.clone();
    let supervisor_events = events_tx.clone();
    let thread = std::thread::Builder::new()
        .name("pronk-pipewire".to_string())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run(
                    config,
                    buffers,
                    remote,
                    command_receiver,
                    startup_cancel_receiver,
                    events_tx,
                    startup_slot,
                )
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    send_startup(&supervisor_startup, Err(error.clone()));
                    report_terminal_error(&supervisor_events, error);
                }
                Err(_) => {
                    let error = VideoSourceRuntimeError::ThreadPanicked;
                    send_startup(&supervisor_startup, Err(error.clone()));
                    report_terminal_error(&supervisor_events, error);
                }
            }
        })?;

    Ok(RuntimeHandle {
        commands,
        startup_cancel,
        events,
        startup,
        thread,
    })
}

fn report_terminal_error(events: &mpsc::Sender<VideoSourceEvent>, error: VideoSourceRuntimeError) {
    // Shutdown joins this thread while retaining the receiver. A full queue
    // still reports termination through channel closure after the thread exits.
    let _ = events.try_send(VideoSourceEvent::Failed(error));
}

struct RuntimeBuffer {
    descriptor: VideoBuffer,
    binding: Option<BufferBinding>,
}

#[derive(Clone, Copy)]
struct BufferBinding {
    raw: NonNull<pw::sys::pw_buffer>,
    transport: PipeWireBufferTransport,
}

struct ThreadState {
    config: VideoSourceConfig,
    buffers: Vec<RuntimeBuffer>,
    tracker: BufferTracker,
    events: mpsc::Sender<VideoSourceEvent>,
    startup: StartupSlot,
    node: NodeRegistration<VideoNodeIdentity>,
    failed: bool,
    shutting_down: bool,
}

enum FormatChange<'a> {
    Cleared,
    Negotiated(&'a Pod),
}

impl ThreadState {
    fn new(
        config: VideoSourceConfig,
        buffers: Vec<VideoBuffer>,
        events: mpsc::Sender<VideoSourceEvent>,
        startup: StartupSlot,
    ) -> Self {
        let tracker = BufferTracker::new(&buffers);
        Self {
            config,
            buffers: buffers
                .into_iter()
                .map(|descriptor| RuntimeBuffer {
                    descriptor,
                    binding: None,
                })
                .collect(),
            tracker,
            events,
            startup,
            node: NodeRegistration::AwaitingBoth,
            failed: false,
            shutting_down: false,
        }
    }

    fn buffer(&self, buffer_id: NonZeroU32) -> Option<&RuntimeBuffer> {
        self.buffers
            .iter()
            .find(|buffer| buffer.descriptor.id == buffer_id)
    }

    fn buffer_mut(&mut self, buffer_id: NonZeroU32) -> Option<&mut RuntimeBuffer> {
        self.buffers
            .iter_mut()
            .find(|buffer| buffer.descriptor.id == buffer_id)
    }

    fn buffer_id_for_raw(&self, raw: *mut pw::sys::pw_buffer) -> Option<NonZeroU32> {
        self.buffers.iter().find_map(|buffer| {
            (buffer.binding.map(|binding| binding.raw.as_ptr()) == Some(raw))
                .then_some(buffer.descriptor.id)
        })
    }

    fn emit(&self, event: VideoSourceEvent) -> Result<(), VideoSourceRuntimeError> {
        self.events.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => VideoSourceRuntimeError::EventQueueFull,
            mpsc::error::TrySendError::Closed(_) => {
                VideoSourceRuntimeError::Stream("PipeWire event receiver was closed".to_string())
            }
        })
    }

    fn observe_node(
        &mut self,
        object_id: u32,
        node_name: &str,
        object_serial: &str,
    ) -> Result<(), VideoSourceRuntimeError> {
        if node_name != self.config.node_name {
            return Ok(());
        }
        let object_id = NonZeroU32::new(object_id).ok_or_else(|| {
            VideoSourceRuntimeError::PipeWire("source node has zero object ID".to_string())
        })?;
        let object_serial = object_serial
            .parse::<u64>()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                VideoSourceRuntimeError::PipeWire(
                    "source node has invalid object.serial".to_string(),
                )
            })?;
        let identity = VideoNodeIdentity {
            node_name: self.config.node_name.clone(),
            object_id,
            object_serial,
            media_generation: self.config.media_generation,
        };
        if let Some(identity) = self.node.observe_registry(identity).map_err(|_| {
            VideoSourceRuntimeError::PipeWire(
                "registry node identity differs from stream node ID".to_string(),
            )
        })? {
            send_startup(&self.startup, Ok(identity));
        }
        Ok(())
    }

    fn observe_stream_node(&mut self, object_id: u32) -> Result<(), VideoSourceRuntimeError> {
        let stream_id = NonZeroU32::new(object_id)
            .filter(|_| object_id != pw::constants::ID_ANY)
            .ok_or_else(|| {
                VideoSourceRuntimeError::PipeWire("source stream has no node ID".to_string())
            })?;
        if let Some(identity) = self.node.observe_stream(stream_id).map_err(|_| {
            VideoSourceRuntimeError::PipeWire(
                "registry node identity differs from stream node ID".to_string(),
            )
        })? {
            send_startup(&self.startup, Ok(identity));
        }
        Ok(())
    }

    fn fail(&mut self, error: VideoSourceRuntimeError) {
        if self.failed || self.shutting_down {
            return;
        }
        self.failed = true;
        send_startup(&self.startup, Err(error.clone()));
        let _ = self.events.try_send(VideoSourceEvent::Failed(error));
    }
}

fn send_startup(startup: &StartupSlot, result: Result<VideoNodeIdentity, VideoSourceRuntimeError>) {
    if let Some(sender) = startup.lock().expect("startup mutex poisoned").take() {
        let _ = sender.send(result);
    }
}

/// Supplement pipewire-rs 0.10's local listener builder, which exposes the
/// command callback internally but has no public builder method for it.
struct RequestProcessListener {
    hook: Box<spa::sys::spa_hook>,
    _events: Box<pw::sys::pw_stream_events>,
    _sender: Box<pw::channel::Sender<()>>,
}

impl RequestProcessListener {
    fn new(stream: &pw::stream::Stream, sender: pw::channel::Sender<()>) -> Self {
        unsafe extern "C" fn on_command(
            data: *mut std::ffi::c_void,
            command: *const spa::sys::spa_command,
        ) {
            if data.is_null() || command.is_null() {
                return;
            }
            // SAFETY: `data` points to the boxed sender retained by the
            // listener, and PipeWire invokes callbacks only while the hook is
            // registered. `command` was checked non-null and is callback-local.
            let (sender, command_id) = unsafe {
                (
                    &*data.cast::<pw::channel::Sender<()>>(),
                    spa::sys::spa_node_command_id(command.cast_mut()),
                )
            };
            if command_id == spa::sys::SPA_NODE_COMMAND_RequestProcess {
                let _ = sender.send(());
            }
        }

        // SAFETY: Both C listener layouts are plain zero-initializable
        // registration records; all optional callbacks start as null.
        let mut hook = Box::new(unsafe { std::mem::zeroed::<spa::sys::spa_hook>() });
        // SAFETY: See the layout justification above.
        let mut events = Box::new(unsafe { std::mem::zeroed::<pw::sys::pw_stream_events>() });
        events.version = pw::sys::PW_VERSION_STREAM_EVENTS;
        events.command = Some(on_command);
        let mut sender = Box::new(sender);
        // SAFETY: The hook, event table, and callback data are heap allocated
        // and retained without moving until this listener unregisters.
        unsafe {
            pw::sys::pw_stream_add_listener(
                stream.as_raw_ptr(),
                hook.as_mut(),
                events.as_ref(),
                sender.as_mut() as *mut _ as *mut std::ffi::c_void,
            );
        }
        Self {
            hook,
            _events: events,
            _sender: sender,
        }
    }
}

impl Drop for RequestProcessListener {
    fn drop(&mut self) {
        spa::utils::hook::remove(*self.hook);
    }
}

fn fail(
    state: &Rc<RefCell<ThreadState>>,
    mainloop: &pw::main_loop::MainLoopRc,
    error: VideoSourceRuntimeError,
) {
    state.borrow_mut().fail(error);
    mainloop.quit();
}

fn run(
    config: VideoSourceConfig,
    buffers: Vec<VideoBuffer>,
    remote: PipeWireRemote,
    command_receiver: pw::channel::Receiver<Command>,
    startup_cancel_receiver: pw::channel::Receiver<()>,
    events: mpsc::Sender<VideoSourceEvent>,
    startup: StartupSlot,
) -> Result<(), VideoSourceRuntimeError> {
    let requires_policy = matches!(&remote, PipeWireRemote::Connected(_));
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| pipewire_error("create main loop", error))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| pipewire_error("create context", error))?;
    let (remote_for_monitor, core) = match remote {
        PipeWireRemote::Connected(fd) => {
            let monitor = fd.try_clone().map_err(|error| {
                VideoSourceRuntimeError::PipeWire(format!(
                    "duplicate classified PipeWire remote: {error}"
                ))
            })?;
            (Some(monitor), context.connect_fd_rc(fd, None))
        }
        PipeWireRemote::AmbientDevelopment => (None, context.connect_rc(None)),
    };
    let core = core.map_err(|error| pipewire_error("connect core", error))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| pipewire_error("get registry", error))?;
    let state = Rc::new(RefCell::new(ThreadState::new(
        config, buffers, events, startup,
    )));
    let policy_gate = Rc::new(RefCell::new(PolicyGate::new(requires_policy)));
    let initial_sync = Rc::new(Cell::new(PolicyRegistrySync::Unrequested));

    // Startup has protocol work to do before the normal command receiver can
    // safely capture a constructed stream. Keep cancellation independently
    // attached from the beginning so a bounded Tokio startup timeout can
    // always stop and join this foreign-loop thread, including while a
    // classified connection is waiting for WirePlumber authorization.
    let state_for_startup_cancel = state.clone();
    let mainloop_for_startup_cancel = mainloop.clone();
    let _startup_cancel = startup_cancel_receiver.attach(mainloop.loop_(), move |()| {
        state_for_startup_cancel.borrow_mut().shutting_down = true;
        mainloop_for_startup_cancel.quit();
    });

    let (remote_closed_sender, remote_closed_receiver) = pw::channel::channel();
    let state_for_remote_closed = state.clone();
    let mainloop_for_remote_closed = mainloop.clone();
    let _remote_closed = remote_closed_receiver.attach(mainloop.loop_(), move |()| {
        if !state_for_remote_closed.borrow().shutting_down {
            fail(
                &state_for_remote_closed,
                &mainloop_for_remote_closed,
                VideoSourceRuntimeError::PipeWire(
                    "classified PipeWire remote disconnected".to_string(),
                ),
            );
        }
    });
    let _remote_monitor = remote_for_monitor
        .map(|remote| {
            let sender = remote_closed_sender.clone();
            RemoteDisconnectMonitor::spawn(remote, move || {
                let _ = sender.send(());
            })
        })
        .transpose()
        .map_err(|error| {
            VideoSourceRuntimeError::PipeWire(format!(
                "monitor classified PipeWire remote: {error}"
            ))
        })?;

    let state_for_core = state.clone();
    let mainloop_for_core = mainloop.clone();
    let sync_for_core = initial_sync.clone();
    let mainloop_for_sync = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if sync_for_core.get().matches_done(id, seq.seq()) {
                sync_for_core.set(PolicyRegistrySync::Complete);
                mainloop_for_sync.quit();
            }
        })
        .error(move |_id, _seq, code, message| {
            if code < 0 && !state_for_core.borrow().shutting_down {
                fail(
                    &state_for_core,
                    &mainloop_for_core,
                    VideoSourceRuntimeError::Core {
                        code,
                        message: message.to_string(),
                    },
                );
            }
        })
        .register();

    let state_for_global = state.clone();
    let mainloop_for_global = mainloop.clone();
    let gate_for_global = policy_gate.clone();
    let state_for_remove = state.clone();
    let mainloop_for_remove = mainloop.clone();
    let gate_for_remove = policy_gate.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pw::types::ObjectType::Metadata {
                let name = global
                    .props
                    .as_ref()
                    .and_then(|props| props.get("metadata.name"));
                gate_for_global
                    .borrow_mut()
                    .observe_metadata(global.id, name);
                return;
            }
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props.as_ref() else {
                return;
            };
            let Some(node_name) = props.get(*pw::keys::NODE_NAME) else {
                return;
            };
            let Some(object_serial) = props.get(*pw::keys::OBJECT_SERIAL) else {
                return;
            };
            let result = {
                state_for_global
                    .borrow_mut()
                    .observe_node(global.id, node_name, object_serial)
            };
            if let Err(error) = result {
                fail(&state_for_global, &mainloop_for_global, error);
            }
        })
        .global_remove(move |id| {
            let policy_lost =
                gate_for_remove.borrow_mut().remove_object(id) == PolicyMarkerChange::Lost;
            if policy_lost && !state_for_remove.borrow().shutting_down {
                fail(
                    &state_for_remove,
                    &mainloop_for_remove,
                    VideoSourceRuntimeError::PolicyUnavailable,
                );
                return;
            }
            let removed = state_for_remove
                .borrow()
                .node
                .identity()
                .is_some_and(|identity| identity.object_id.get() == id);
            if removed && !state_for_remove.borrow().shutting_down {
                fail(
                    &state_for_remove,
                    &mainloop_for_remove,
                    VideoSourceRuntimeError::NodeRemoved,
                );
            }
        })
        .register();

    // A classified producer is not allowed to publish until the complete
    // versioned WirePlumber policy has advertised its live marker. The sync is
    // a registry barrier, so absence after this point is authoritative rather
    // than a startup-order race.
    let sync = core
        .sync(0)
        .map_err(|error| pipewire_error("synchronize policy registry", error))?;
    initial_sync.set(PolicyRegistrySync::Awaiting(sync.seq()));
    mainloop.run();
    if state.borrow().failed || state.borrow().shutting_down {
        return Ok(());
    }
    if !initial_sync.get().is_complete() {
        return Err(VideoSourceRuntimeError::PipeWire(
            "policy registry synchronization stopped unexpectedly".to_string(),
        ));
    }
    if !policy_gate.borrow().is_open() {
        return Err(VideoSourceRuntimeError::PolicyUnavailable);
    }

    let properties = source_properties(&state.borrow().config);
    let stream =
        pw::stream::StreamRc::new(core.clone(), &state.borrow().config.node_name, properties)
            .map_err(|error| pipewire_error("create source stream", error))?;
    let (process_kick_sender, process_kick_receiver) = pw::channel::channel();
    // `trigger_process` may synchronously invoke the process callback. Keep
    // its retry state independent of the callback-owned runtime state so no
    // RefCell borrow crosses that FFI call.
    let consecutive_trigger_failures = Rc::new(Cell::new(0));

    let state_for_state = state.clone();
    let mainloop_for_state = mainloop.clone();
    let process_kick_for_state = process_kick_sender.clone();
    let process_kick_for_command = process_kick_sender.clone();
    let state_for_param = state.clone();
    let mainloop_for_param = mainloop.clone();
    let state_for_add = state.clone();
    let mainloop_for_add = mainloop.clone();
    let state_for_remove_buffer = state.clone();
    let mainloop_for_remove_buffer = mainloop.clone();
    let state_for_process = state.clone();
    let mainloop_for_process = mainloop.clone();
    let _stream_listener = stream
        .add_local_listener::<()>()
        .state_changed(move |stream, _, _old, new| match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                let result = {
                    state_for_state
                        .borrow_mut()
                        .observe_stream_node(stream.node_id())
                };
                if let Err(error) = result {
                    fail(&state_for_state, &mainloop_for_state, error);
                    return;
                }
                if new == pw::stream::StreamState::Streaming {
                    // Triggering may synchronously call `process`; defer it so
                    // it cannot reenter this listener callback.
                    let _ = process_kick_for_state.send(());
                }
            }
            pw::stream::StreamState::Error(message) => fail(
                &state_for_state,
                &mainloop_for_state,
                VideoSourceRuntimeError::Stream(message),
            ),
            pw::stream::StreamState::Unconnected if !state_for_state.borrow().shutting_down => {
                fail(
                    &state_for_state,
                    &mainloop_for_state,
                    VideoSourceRuntimeError::Stream("source stream disconnected".to_string()),
                );
            }
            _ => {}
        })
        .param_changed(move |stream, _, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            // PipeWire clears the negotiated Format with a null parameter when
            // the exact consumer disconnects. The source node and its caller-
            // owned pool deliberately survive that interval so a replacement
            // backend can negotiate the same format. A present but incompatible
            // format remains terminal.
            let param = match classify_format_change(param) {
                FormatChange::Cleared => {
                    tracing::debug!("PipeWire consumer cleared the negotiated video format");
                    return;
                }
                FormatChange::Negotiated(param) => param,
            };
            let result = negotiate_buffers(stream, &state_for_param.borrow(), param);
            if let Err(error) = result {
                fail(&state_for_param, &mainloop_for_param, error);
            }
        })
        .add_buffer(move |_stream, _, raw| {
            let result = {
                let mut state = state_for_add.borrow_mut();
                add_buffer(&mut state, raw)
            };
            if let Err(error) = result {
                fail(&state_for_add, &mainloop_for_add, error);
            }
        })
        .remove_buffer(move |_stream, _, raw| {
            let result = {
                let mut state = state_for_remove_buffer.borrow_mut();
                remove_buffer(&mut state, raw)
            };
            if let Err(error) = result {
                fail(&state_for_remove_buffer, &mainloop_for_remove_buffer, error);
            }
        })
        .process(move |stream, _| {
            let result = {
                let mut state = state_for_process.borrow_mut();
                process_returned_buffers(stream, &mut state)
            };
            if let Err(error) = result {
                fail(&state_for_process, &mainloop_for_process, error);
            }
        })
        .register()
        .map_err(|error| pipewire_error("register source stream listener", error))?;
    // Consumers may explicitly ask this application-driven source to schedule
    // another graph cycle. Consumers that only queue returned buffers are
    // covered by the video actor's bounded process deadline.
    let _request_process_listener = RequestProcessListener::new(&stream, process_kick_for_command);

    let stream_for_process_kick = stream.clone();
    let state_for_process_kick = state.clone();
    let mainloop_for_process_kick = mainloop.clone();
    let trigger_failures_for_process_kick = consecutive_trigger_failures.clone();
    let _process_kick = process_kick_receiver.attach(mainloop.loop_(), move |()| {
        if state_for_process_kick.borrow().shutting_down {
            return;
        }
        let result = trigger_graph(&stream_for_process_kick, &trigger_failures_for_process_kick);
        if let Err(error) = result {
            fail(&state_for_process_kick, &mainloop_for_process_kick, error);
        }
    });

    let state_for_commands = state.clone();
    let mainloop_for_commands = mainloop.clone();
    let stream_for_commands = stream.clone();
    let trigger_failures_for_commands = consecutive_trigger_failures.clone();
    let _commands = command_receiver.attach(mainloop.loop_(), move |command| match command {
        Command::Publish {
            frame,
            _permit,
            reply,
        } => {
            let result = {
                let mut state = state_for_commands.borrow_mut();
                publish_frame(&stream_for_commands, &mut state, frame)
            };
            // A driving stream may synchronously invoke `process` here. Keep
            // the RefCell borrow above tightly scoped so that callback can
            // observe the buffer return without a reentrant borrow panic.
            let trigger_failure = if result.is_ok() {
                trigger_graph(&stream_for_commands, &trigger_failures_for_commands).err()
            } else {
                None
            };
            let failed = result.as_ref().err().cloned().or(trigger_failure);
            let _ = reply.send(result);
            if let Some(error) = failed {
                fail(&state_for_commands, &mainloop_for_commands, error);
            }
        }
        Command::TriggerProcess { reply } => {
            let result = trigger_graph(&stream_for_commands, &trigger_failures_for_commands);
            let _ = reply.send(());
            if let Err(error) = result {
                fail(&state_for_commands, &mainloop_for_commands, error);
            }
        }
        Command::Shutdown => {
            state_for_commands.borrow_mut().shutting_down = true;
            mainloop_for_commands.quit();
        }
    });

    let format = format_parameter(
        state.borrow().config.frame_rate,
        state.borrow().buffers[0].descriptor.layout,
    )?;
    let mut params = [Pod::from_bytes(&format).ok_or_else(|| {
        VideoSourceRuntimeError::PipeWire("serialize PipeWire format pod".to_string())
    })?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER
                | pw::stream::StreamFlags::ALLOC_BUFFERS
                | pw::stream::StreamFlags::EXCLUSIVE,
            &mut params,
        )
        .map_err(|error| pipewire_error("connect source stream", error))?;

    mainloop.run();

    let state = state.borrow_mut();
    if !state.failed {
        if !state.shutting_down {
            return Err(VideoSourceRuntimeError::Stream(
                "PipeWire loop stopped unexpectedly".to_string(),
            ));
        }
        send_startup(
            &state.startup,
            Err(VideoSourceRuntimeError::Stream(
                "PipeWire source stopped before publishing".to_string(),
            )),
        );
        let _ = state.emit(VideoSourceEvent::Stopped);
    }
    Ok(())
}

fn trigger_graph(
    stream: &pw::stream::Stream,
    consecutive_failures: &Cell<u32>,
) -> Result<(), VideoSourceRuntimeError> {
    // A trigger is an edge, not a transaction. PipeWire may reject one while
    // another graph iteration owns it (notably EIO from the activation-state
    // compare/exchange). A working graph clears that condition promptly. If
    // it persists, the source has lost progress, such as after a PipeWire
    // daemon restart whose old client connection has not emitted a core error.
    match stream.trigger_process() {
        Ok(()) => {
            consecutive_failures.set(0);
            Ok(())
        }
        Err(error) => {
            let failures = consecutive_failures.get().saturating_add(1);
            consecutive_failures.set(failures);
            if failures >= MAX_CONSECUTIVE_TRIGGER_FAILURES {
                return Err(VideoSourceRuntimeError::PipeWire(format!(
                    "PipeWire graph trigger failed {} consecutive times: {error}",
                    failures
                )));
            }
            tracing::trace!(
                %error,
                failures,
                "PipeWire graph trigger was coalesced"
            );
            Ok(())
        }
    }
}

fn source_properties(config: &VideoSourceConfig) -> pw::properties::PropertiesBox {
    let connector_id = config.connector_id.to_string();
    let output_index = config.output_index.to_string();
    let media_generation = config.media_generation.to_string();
    properties! {
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => config.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => config.node_description.as_str(),
        *pw::keys::NODE_EXCLUSIVE => "true",
        "node.reliable" => "true",
        *pw::keys::NODE_VIRTUAL => "true",
        "device.api" => "castkms",
        PRIVATE_NODE_PROPERTY => PRIVATE_NODE_POLICY_VERSION,
        "api.pronk.session-id" => config.session_id.as_str(),
        "api.pronk.device-instance" => config.device_instance.as_str(),
        "api.pronk.connector-id" => connector_id,
        "api.pronk.output-index" => output_index,
        "api.pronk.media-generation" => media_generation
    }
}

fn pipewire_error(operation: &'static str, error: pw::Error) -> VideoSourceRuntimeError {
    VideoSourceRuntimeError::PipeWire(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_error_does_not_wait_for_a_full_event_queue() {
        let (events, _receiver) = mpsc::channel(1);
        events.try_send(VideoSourceEvent::Stopped).unwrap();
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            report_terminal_error(&events, VideoSourceRuntimeError::ThreadPanicked);
            let _ = done.send(());
        });
        wait.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn video_identity_does_not_require_a_kernel_grant_number() {
        let config = VideoSourceConfig {
            node_name: "pronk.video.test".into(),
            node_description: "Capture test".into(),
            session_id: "session-test".into(),
            device_instance: "device-test".into(),
            connector_id: NonZeroU32::new(1).unwrap(),
            output_index: 0,
            media_generation: NonZeroU64::new(2).unwrap(),
            frame_rate: crate::VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
        };
        let properties = source_properties(&config);
        assert_eq!(properties.get("api.pronk.grant-id"), None);
        assert_eq!(properties.get("api.pronk.session-id"), Some("session-test"));
        assert_eq!(properties.get("api.pronk.media-generation"), Some("2"));
        assert_eq!(
            properties.get(PRIVATE_NODE_PROPERTY),
            Some(PRIVATE_NODE_POLICY_VERSION)
        );
    }

    #[test]
    fn consumers_may_omit_optional_frame_metadata() {
        let mut buffer = spa::sys::spa_buffer {
            n_metas: 0,
            n_datas: 0,
            metas: std::ptr::null_mut(),
            datas: std::ptr::null_mut(),
        };
        let frame = VideoFrame {
            buffer_id: NonZeroU32::new(1).unwrap(),
            sequence: 2,
            pts_ns: 3,
            damage: crate::VideoDamage {
                x: 0,
                y: 0,
                width: NonZeroU32::new(4).unwrap(),
                height: NonZeroU32::new(5).unwrap(),
            },
            discontinuity: false,
            acquire_point: None,
        };

        unsafe { fill_optional_frame_metadata(&mut buffer, frame) }.unwrap();
    }

    #[test]
    fn linear_cpu_format_does_not_force_dmabuf_caps_downstream() {
        let layout = crate::VideoBufferLayout {
            format: crate::VideoPixelFormat::Xrgb8888,
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            pitch: NonZeroU32::new(7680).unwrap(),
            size: NonZeroU64::new(8_294_400).unwrap(),
            storage: crate::VideoBufferStorage::MappableLinear,
        };
        let bytes = format_parameter(
            crate::VideoFrameRate::integer(NonZeroU32::new(60).unwrap()),
            layout,
        )
        .unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut info = VideoInfoRaw::new();
        info.parse(pod).unwrap();

        assert_eq!(info.format(), VideoFormat::BGRx);
        assert_eq!(info.size().width, 1920);
        assert_eq!(info.size().height, 1080);
        assert!(!info.flags().contains(VideoFlags::MODIFIER));
    }

    #[test]
    fn format_preserves_a_fractional_frame_rate() {
        let layout = crate::VideoBufferLayout {
            format: crate::VideoPixelFormat::Xrgb8888,
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            pitch: NonZeroU32::new(7680).unwrap(),
            size: NonZeroU64::new(8_294_400).unwrap(),
            storage: crate::VideoBufferStorage::MappableLinear,
        };
        let frame_rate = crate::VideoFrameRate::new(
            NonZeroU32::new(30_000).unwrap(),
            NonZeroU32::new(1_001).unwrap(),
        );
        let bytes = format_parameter(frame_rate, layout).unwrap();
        let mut info = VideoInfoRaw::new();
        info.parse(Pod::from_bytes(&bytes).unwrap()).unwrap();

        assert_eq!(info.framerate().num, 30_000);
        assert_eq!(info.framerate().denom, 1_001);
    }

    #[test]
    fn explicit_modifiers_round_trip_without_accepting_an_implicit_layout() {
        for modifier in [0, 0x0100_0000_0000_0009, 0x8100_0000_0000_0009] {
            let storage = crate::VideoBufferStorage::DrmModifier {
                modifier,
                offset: 4096,
            };
            let layout = crate::VideoBufferLayout {
                format: crate::VideoPixelFormat::Xrgb8888,
                width: NonZeroU32::new(16).unwrap(),
                height: NonZeroU32::new(8).unwrap(),
                pitch: NonZeroU32::new(64).unwrap(),
                size: NonZeroU64::new(8192).unwrap(),
                storage,
            };
            let bytes = format_parameter(
                crate::VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
                layout,
            )
            .unwrap();
            let mut info = VideoInfoRaw::new();
            info.parse(Pod::from_bytes(&bytes).unwrap()).unwrap();
            assert!(storage_matches(&info, storage));
            assert!(!storage_matches(
                &info,
                crate::VideoBufferStorage::MappableLinear
            ));
            assert!(!storage_matches(
                &info,
                crate::VideoBufferStorage::DrmModifier {
                    modifier: modifier ^ 1,
                    offset: 4096,
                }
            ));

            let bytes = format_parameter(
                crate::VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
                crate::VideoBufferLayout {
                    storage: crate::VideoBufferStorage::MappableLinear,
                    ..layout
                },
            )
            .unwrap();
            info.parse(Pod::from_bytes(&bytes).unwrap()).unwrap();
            assert!(!storage_matches(&info, storage));
        }
    }

    #[test]
    fn native_data_preserves_plane_offsets_on_every_publication() {
        for storage in [
            crate::VideoBufferStorage::MappableLinear,
            crate::VideoBufferStorage::DrmModifier {
                modifier: 0,
                offset: 4096,
            },
            crate::VideoBufferStorage::DrmModifier {
                modifier: 0x0100_0000_0000_0009,
                offset: 4096,
            },
        ] {
            let descriptor = VideoBuffer {
                id: NonZeroU32::new(1).unwrap(),
                dma_buf: std::fs::File::open("/dev/null").unwrap().into(),
                layout: crate::VideoBufferLayout {
                    format: crate::VideoPixelFormat::Xrgb8888,
                    width: NonZeroU32::new(16).unwrap(),
                    height: NonZeroU32::new(8).unwrap(),
                    pitch: NonZeroU32::new(64).unwrap(),
                    size: NonZeroU64::new(8192).unwrap(),
                    storage,
                },
                timelines: None,
            };
            // Backing descriptors are not touched by these metadata-only helpers.
            let mut chunk: spa::sys::spa_chunk = unsafe { std::mem::zeroed() };
            let mut data: spa::sys::spa_data = unsafe { std::mem::zeroed() };
            data.chunk = &mut chunk;
            let mut spa: spa::sys::spa_buffer = unsafe { std::mem::zeroed() };
            spa.n_datas = 1;
            spa.datas = &mut data;
            unsafe {
                configure_spa_data(
                    &mut spa,
                    &descriptor,
                    PipeWireBufferTransport::ReadyBeforePublish,
                )
            }
            .unwrap();
            assert_eq!(chunk.offset, storage.offset());
            assert_eq!(chunk.size, 8192 - storage.offset());
            assert_eq!(data.maxsize, 8192);
            assert_eq!(data.mapoffset, 0);
            assert_eq!(
                data.flags & spa::sys::SPA_DATA_FLAG_MAPPABLE != 0,
                storage == crate::VideoBufferStorage::MappableLinear
            );
            assert_eq!(
                data.flags & spa::sys::SPA_DATA_FLAG_READWRITE,
                spa::sys::SPA_DATA_FLAG_READABLE
            );

            let mut raw: pw::sys::pw_buffer = unsafe { std::mem::zeroed() };
            raw.buffer = &mut spa;
            let frame = VideoFrame {
                buffer_id: descriptor.id,
                sequence: 1,
                pts_ns: 1,
                damage: crate::VideoDamage {
                    x: 0,
                    y: 0,
                    width: descriptor.layout.width,
                    height: descriptor.layout.height,
                },
                discontinuity: false,
                acquire_point: None,
            };
            chunk.offset = 0;
            chunk.size = 0;
            unsafe {
                fill_frame(
                    &mut raw,
                    &descriptor,
                    PipeWireBufferTransport::ReadyBeforePublish,
                    frame,
                )
            }
            .unwrap();
            assert_eq!(chunk.offset, storage.offset());
            assert_eq!(chunk.size, 8192 - storage.offset());
        }
    }

    #[test]
    fn a_disconnected_consumer_clears_format_without_poisoning_the_source() {
        assert!(matches!(
            classify_format_change(None),
            FormatChange::Cleared
        ));

        let layout = crate::VideoBufferLayout {
            format: crate::VideoPixelFormat::Xrgb8888,
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            pitch: NonZeroU32::new(7680).unwrap(),
            size: NonZeroU64::new(8_294_400).unwrap(),
            storage: crate::VideoBufferStorage::MappableLinear,
        };
        let bytes = format_parameter(
            crate::VideoFrameRate::integer(NonZeroU32::new(60).unwrap()),
            layout,
        )
        .unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        assert!(matches!(
            classify_format_change(Some(pod)),
            FormatChange::Negotiated(_)
        ));
    }

    #[test]
    fn packed_alpha_format_is_preserved_with_each_storage_profile() {
        for storage in [
            crate::VideoBufferStorage::MappableLinear,
            crate::VideoBufferStorage::DrmModifier {
                modifier: 0,
                offset: 0,
            },
            crate::VideoBufferStorage::DrmModifier {
                modifier: 0x0100_0000_0000_0009,
                offset: 0,
            },
        ] {
            for format in [
                crate::VideoPixelFormat::Xrgb8888,
                crate::VideoPixelFormat::Argb8888,
            ] {
                let layout = crate::VideoBufferLayout {
                    format,
                    width: NonZeroU32::new(16).unwrap(),
                    height: NonZeroU32::new(8).unwrap(),
                    pitch: NonZeroU32::new(64).unwrap(),
                    size: NonZeroU64::new(512).unwrap(),
                    storage,
                };
                let bytes = format_parameter(
                    crate::VideoFrameRate::integer(NonZeroU32::new(30).unwrap()),
                    layout,
                )
                .unwrap();
                let mut info = VideoInfoRaw::new();
                info.parse(Pod::from_bytes(&bytes).unwrap()).unwrap();
                assert_eq!(info.format(), pixel_format(format));
                assert!(storage_matches(&info, storage));
            }
        }
    }
}
