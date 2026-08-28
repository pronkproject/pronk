use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::AsRawFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFlags, VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

use crate::model::{BufferReturn, BufferTracker};
use crate::policy_gate::{
    PolicyGate, PolicyMarkerChange, PRIVATE_NODE_POLICY_VERSION, PRIVATE_NODE_PROPERTY,
};
use crate::{
    PipeWireBufferTransport, PipeWireRemote, VideoBuffer, VideoFrame, VideoNodeIdentity,
    VideoSourceConfig, VideoSourceEvent, VideoSourceRuntimeError,
};

const EVENT_QUEUE_CAPACITY: usize = 128;
const CORE_OBJECT_ID: u32 = 0;

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
                    let _ = supervisor_events.blocking_send(VideoSourceEvent::Failed(error));
                }
                Err(_) => {
                    let error = VideoSourceRuntimeError::ThreadPanicked;
                    send_startup(&supervisor_startup, Err(error.clone()));
                    let _ = supervisor_events.blocking_send(VideoSourceEvent::Failed(error));
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

struct RuntimeBuffer {
    descriptor: VideoBuffer,
    pipewire_buffer: Option<NonNull<pw::sys::pw_buffer>>,
    transport: Option<PipeWireBufferTransport>,
}

struct ThreadState {
    config: VideoSourceConfig,
    buffers: Vec<RuntimeBuffer>,
    tracker: BufferTracker,
    events: mpsc::Sender<VideoSourceEvent>,
    startup: StartupSlot,
    identity: Option<VideoNodeIdentity>,
    stream_node_id: Option<NonZeroU32>,
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
                    pipewire_buffer: None,
                    transport: None,
                })
                .collect(),
            tracker,
            events,
            startup,
            identity: None,
            stream_node_id: None,
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
            (buffer.pipewire_buffer.map(NonNull::as_ptr) == Some(raw))
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
        self.identity = Some(VideoNodeIdentity {
            node_name: self.config.node_name.clone(),
            object_id,
            object_serial,
            media_generation: self.config.media_generation,
        });
        self.maybe_complete_startup()
    }

    fn observe_stream_node(&mut self, object_id: u32) -> Result<(), VideoSourceRuntimeError> {
        self.stream_node_id = NonZeroU32::new(object_id);
        if self.stream_node_id.is_none() || object_id == pw::constants::ID_ANY {
            return Err(VideoSourceRuntimeError::PipeWire(
                "source stream has no node ID".to_string(),
            ));
        }
        self.maybe_complete_startup()
    }

    fn maybe_complete_startup(&mut self) -> Result<(), VideoSourceRuntimeError> {
        let (Some(identity), Some(stream_node_id)) = (&self.identity, self.stream_node_id) else {
            return Ok(());
        };
        if identity.object_id != stream_node_id {
            return Err(VideoSourceRuntimeError::PipeWire(
                "registry node identity differs from stream node ID".to_string(),
            ));
        }
        send_startup(&self.startup, Ok(identity.clone()));
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
    let core = match remote {
        PipeWireRemote::Connected(fd) => context.connect_fd_rc(fd, None),
        PipeWireRemote::AmbientDevelopment => context.connect_rc(None),
    }
    .map_err(|error| pipewire_error("connect core", error))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| pipewire_error("get registry", error))?;
    let state = Rc::new(RefCell::new(ThreadState::new(
        config, buffers, events, startup,
    )));
    let policy_gate = Rc::new(RefCell::new(PolicyGate::new(requires_policy)));
    let initial_sync_seq = Rc::new(Cell::new(None));
    let initial_sync_complete = Rc::new(Cell::new(false));

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

    let state_for_core = state.clone();
    let mainloop_for_core = mainloop.clone();
    let sync_seq_for_core = initial_sync_seq.clone();
    let sync_complete_for_core = initial_sync_complete.clone();
    let mainloop_for_sync = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == CORE_OBJECT_ID && sync_seq_for_core.get() == Some(seq.seq()) {
                sync_complete_for_core.set(true);
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
                .identity
                .as_ref()
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
    initial_sync_seq.set(Some(sync.seq()));
    mainloop.run();
    if state.borrow().failed || state.borrow().shutting_down {
        return Ok(());
    }
    if !initial_sync_complete.get() {
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
    let _process_kick = process_kick_receiver.attach(mainloop.loop_(), move |()| {
        if state_for_process_kick.borrow().shutting_down {
            return;
        }
        trigger_graph(&stream_for_process_kick);
    });

    let state_for_commands = state.clone();
    let mainloop_for_commands = mainloop.clone();
    let stream_for_commands = stream.clone();
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
            if result.is_ok() {
                trigger_graph(&stream_for_commands);
            }
            let failed = result.as_ref().err().cloned();
            let _ = reply.send(result);
            if let Some(error) = failed {
                fail(&state_for_commands, &mainloop_for_commands, error);
            }
        }
        Command::TriggerProcess { reply } => {
            trigger_graph(&stream_for_commands);
            let _ = reply.send(());
        }
        Command::Shutdown => {
            state_for_commands.borrow_mut().shutting_down = true;
            mainloop_for_commands.quit();
        }
    });

    let format = format_parameter(
        state.borrow().config.refresh_hz,
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

fn trigger_graph(stream: &pw::stream::Stream) {
    // A trigger is an edge, not a transaction. PipeWire returns an error when
    // another graph iteration already owns that edge (notably EIO from the
    // activation-state compare/exchange), while the queued buffer remains
    // valid and the pending iteration can consume it. PipeWire's own driving
    // stream examples consequently treat this call as best effort. Actual
    // stream/core failures arrive through their state listeners above.
    if let Err(error) = stream.trigger_process() {
        tracing::trace!(%error, "PipeWire graph trigger was coalesced");
    }
}

fn source_properties(config: &VideoSourceConfig) -> pw::properties::PropertiesBox {
    let connector_id = config.connector_id.to_string();
    let output_index = config.output_index.to_string();
    let grant_id = config.grant_id.to_string();
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
        "api.pronk.grant-id" => grant_id,
        "api.pronk.media-generation" => media_generation
    }
}

fn format_parameter(
    refresh_hz: NonZeroU32,
    layout: crate::VideoBufferLayout,
) -> Result<Vec<u8>, VideoSourceRuntimeError> {
    if layout.modifier != 0 {
        return Err(VideoSourceRuntimeError::UnsupportedFormat);
    }
    // This producer allocates linear, CPU-mappable DMA-BUFs. Omitting the DRM
    // modifier lets consumers that copy out of the DMA-BUF expose ordinary
    // system-memory raw caps downstream; a modifier would require GStreamer to
    // preserve the memory:DMABuf feature all the way through the graph.
    let object = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: layout.width.get(),
                height: layout.height.get(),
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction {
                num: refresh_hz.get(),
                denom: 1,
            }
        ),
    );
    serialize_value(&Value::Object(object))
}

fn classify_format_change(param: Option<&Pod>) -> FormatChange<'_> {
    match param {
        Some(param) => FormatChange::Negotiated(param),
        None => FormatChange::Cleared,
    }
}

fn negotiate_buffers(
    stream: &pw::stream::Stream,
    state: &ThreadState,
    format: &Pod,
) -> Result<(), VideoSourceRuntimeError> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(format)
        .map_err(|_| VideoSourceRuntimeError::UnsupportedFormat)?;
    let mut info = VideoInfoRaw::new();
    info.parse(format)
        .map_err(|_| VideoSourceRuntimeError::UnsupportedFormat)?;
    let layout = state.buffers[0].descriptor.layout;
    if media_type != MediaType::Video
        || media_subtype != MediaSubtype::Raw
        || info.format() != VideoFormat::BGRx
        || info.flags().contains(VideoFlags::MODIFIER)
        || info.size().width != layout.width.get()
        || info.size().height != layout.height.get()
        || info.framerate().num != state.config.refresh_hz.get()
        || info.framerate().denom != 1
    {
        return Err(VideoSourceRuntimeError::UnsupportedFormat);
    }

    let values = buffer_parameters(state)?;
    let mut pods = values
        .iter()
        .map(|value| {
            Pod::from_bytes(value).ok_or_else(|| {
                VideoSourceRuntimeError::PipeWire("serialize PipeWire buffer parameter".to_string())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    stream
        .update_params(&mut pods)
        .map_err(|error| pipewire_error("update source buffer parameters", error))
}

fn buffer_parameters(state: &ThreadState) -> Result<Vec<Vec<u8>>, VideoSourceRuntimeError> {
    let layout = state.buffers[0].descriptor.layout;
    let count = i32::try_from(state.buffers.len()).expect("buffer count is bounded by 64");
    let size = i32::try_from(layout.size.get()).expect("validated layout fits i32-sized frame");
    let stride = i32::try_from(layout.pitch.get()).expect("validated pitch fits i32");
    let dma_buf_flag = 1i32
        .checked_shl(spa::sys::SPA_DATA_DmaBuf)
        .ok_or_else(|| VideoSourceRuntimeError::PipeWire("invalid DMA-BUF type".to_string()))?;
    let sync_meta_flag = 1i32
        .checked_shl(spa::sys::SPA_META_SyncTimeline)
        .ok_or_else(|| VideoSourceRuntimeError::PipeWire("invalid sync meta type".to_string()))?;
    let buffer_count = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: count,
            min: 2,
            max: count,
        },
    )));
    let data_type = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Flags {
            default: dma_buf_flag,
            flags: vec![dma_buf_flag],
        },
    )));
    let mut values = Vec::new();

    if state.buffers[0].descriptor.timelines.is_some() {
        let mut meta_type = Property::new(
            spa::sys::SPA_PARAM_BUFFERS_metaType,
            Value::Int(sync_meta_flag),
        );
        meta_type.flags = PropertyFlags::MANDATORY;
        values.push(Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
            id: spa::sys::SPA_PARAM_Buffers,
            properties: vec![
                Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, buffer_count.clone()),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(3)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, data_type.clone()),
                meta_type,
            ],
        }));
    }
    values.push(Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, buffer_count),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, data_type),
        ],
    }));
    values.push(meta_parameter(
        spa::sys::SPA_META_Header,
        std::mem::size_of::<spa::sys::spa_meta_header>(),
    ));
    values.push(meta_parameter(
        spa::sys::SPA_META_VideoDamage,
        std::mem::size_of::<spa::sys::spa_meta_region>(),
    ));
    if state.buffers[0].descriptor.timelines.is_some() {
        values.push(meta_parameter(
            spa::sys::SPA_META_SyncTimeline,
            std::mem::size_of::<spa::sys::spa_meta_sync_timeline>(),
        ));
    }

    values
        .iter()
        .map(serialize_value)
        .collect::<Result<Vec<_>, _>>()
}

fn meta_parameter(meta_type: u32, size: usize) -> Value {
    Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(Id(meta_type))),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(i32::try_from(size).expect("SPA metadata size fits i32")),
            ),
        ],
    })
}

fn serialize_value(value: &Value) -> Result<Vec<u8>, VideoSourceRuntimeError> {
    PodSerializer::serialize(Cursor::new(Vec::new()), value)
        .map(|(bytes, _)| bytes.into_inner())
        .map_err(|error| VideoSourceRuntimeError::PipeWire(format!("serialize SPA pod: {error:?}")))
}

fn add_buffer(
    state: &mut ThreadState,
    raw: *mut pw::sys::pw_buffer,
) -> Result<(), VideoSourceRuntimeError> {
    let raw = NonNull::new(raw).ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
        "null pw_buffer",
    ))?;
    let buffer_id = state
        .tracker
        .next_unbound()
        .ok_or(VideoSourceRuntimeError::TooManyPipeWireBuffers)?;
    let runtime = state
        .buffer_mut(buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
    let spa_buffer = unsafe { raw.as_ref().buffer };
    let spa_buffer = NonNull::new(spa_buffer).ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    let n_datas = unsafe { spa_buffer.as_ref().n_datas };
    let has_sync_meta = unsafe { sync_meta(spa_buffer.as_ptr()) }.is_some();
    let transport = match (
        n_datas,
        runtime.descriptor.timelines.as_ref(),
        has_sync_meta,
    ) {
        (1, _, _) => PipeWireBufferTransport::Waited,
        (3, Some(_), true) => PipeWireBufferTransport::SyncTimeline,
        _ => {
            return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "invalid data/sync-timeline layout",
            ));
        }
    };

    unsafe { configure_spa_data(spa_buffer.as_ptr(), &runtime.descriptor, transport)? };
    runtime.pipewire_buffer = Some(raw);
    runtime.transport = Some(transport);
    state.tracker.bind(buffer_id, transport)
}

fn remove_buffer(
    state: &mut ThreadState,
    raw: *mut pw::sys::pw_buffer,
) -> Result<(), VideoSourceRuntimeError> {
    let buffer_id =
        state
            .buffer_id_for_raw(raw)
            .ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "remove names unknown pw_buffer",
            ))?;
    state.tracker.unbind(buffer_id)?;
    let runtime = state
        .buffer_mut(buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
    runtime.pipewire_buffer = None;
    runtime.transport = None;
    Ok(())
}

unsafe fn configure_spa_data(
    spa_buffer: *mut spa::sys::spa_buffer,
    descriptor: &VideoBuffer,
    transport: PipeWireBufferTransport,
) -> Result<(), VideoSourceRuntimeError> {
    let buffer = unsafe { spa_buffer.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    if buffer.datas.is_null() || buffer.n_datas == 0 {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing data planes",
        ));
    }
    let datas = unsafe { std::slice::from_raw_parts_mut(buffer.datas, buffer.n_datas as usize) };
    if datas[0].chunk.is_null() {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing DMA-BUF chunk",
        ));
    }
    datas[0].type_ = spa::sys::SPA_DATA_DmaBuf;
    datas[0].flags = spa::sys::SPA_DATA_FLAG_READABLE | spa::sys::SPA_DATA_FLAG_MAPPABLE;
    datas[0].fd = descriptor.dma_buf.as_raw_fd() as i64;
    datas[0].mapoffset = 0;
    datas[0].maxsize = descriptor.layout.size.get() as u32;
    datas[0].data = std::ptr::null_mut();
    let chunk = unsafe { &mut *datas[0].chunk };
    chunk.offset = 0;
    chunk.size = descriptor.layout.size.get() as u32;
    chunk.stride = descriptor.layout.pitch.get() as i32;

    if transport == PipeWireBufferTransport::SyncTimeline {
        let timelines =
            descriptor
                .timelines
                .as_ref()
                .ok_or(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                    "missing syncobj descriptors",
                ))?;
        if datas.len() != 3 {
            return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
                "sync timeline needs three data planes",
            ));
        }
        datas[1].type_ = spa::sys::SPA_DATA_SyncObj;
        datas[1].flags = spa::sys::SPA_DATA_FLAG_READABLE;
        datas[1].fd = timelines.ready.as_raw_fd() as i64;
        datas[1].mapoffset = 0;
        datas[1].maxsize = 0;
        datas[1].data = std::ptr::null_mut();
        datas[2].type_ = spa::sys::SPA_DATA_SyncObj;
        datas[2].flags = spa::sys::SPA_DATA_FLAG_READWRITE;
        datas[2].fd = timelines.reuse.as_raw_fd() as i64;
        datas[2].mapoffset = 0;
        datas[2].maxsize = 0;
        datas[2].data = std::ptr::null_mut();
        let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("missing sync metadata"),
        )?;
        unsafe {
            *sync.as_ptr() = spa::sys::spa_meta_sync_timeline {
                flags: 0,
                padding: 0,
                acquire_point: 0,
                release_point: 0,
            };
        }
    }
    Ok(())
}

fn process_returned_buffers(
    stream: &pw::stream::Stream,
    state: &mut ThreadState,
) -> Result<(), VideoSourceRuntimeError> {
    loop {
        let raw = unsafe { stream.dequeue_raw_buffer() };
        let Some(raw) = NonNull::new(raw) else {
            break;
        };
        let buffer_id = state.buffer_id_for_raw(raw.as_ptr()).ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("dequeued unknown pw_buffer"),
        )?;
        let runtime = state
            .buffer(buffer_id)
            .ok_or(VideoSourceRuntimeError::UnknownBuffer(buffer_id.get()))?;
        let actual_release = match runtime.transport {
            Some(PipeWireBufferTransport::SyncTimeline) => {
                let spa_buffer = unsafe { raw.as_ref().buffer };
                let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
                    VideoSourceRuntimeError::InvalidPipeWireBuffer(
                        "returned buffer lost sync metadata",
                    ),
                )?;
                NonZeroU64::new(unsafe { sync.as_ref().release_point })
            }
            Some(PipeWireBufferTransport::Waited) => None,
            None => return Err(VideoSourceRuntimeError::InvalidOwnership(buffer_id.get())),
        };
        let event = match state.tracker.returned(buffer_id, actual_release)? {
            BufferReturn::Initial {
                buffer_id,
                transport,
            } => VideoSourceEvent::BufferAvailable {
                buffer_id,
                transport,
            },
            BufferReturn::Released { buffer_id } => VideoSourceEvent::BufferReleased { buffer_id },
        };
        state.emit(event)?;
    }
    Ok(())
}

fn publish_frame(
    stream: &pw::stream::Stream,
    state: &mut ThreadState,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    state.tracker.publish(frame)?;
    let runtime = state
        .buffer(frame.buffer_id)
        .ok_or(VideoSourceRuntimeError::UnknownBuffer(
            frame.buffer_id.get(),
        ))?;
    let raw = runtime
        .pipewire_buffer
        .ok_or(VideoSourceRuntimeError::InvalidOwnership(
            frame.buffer_id.get(),
        ))?;
    unsafe { fill_frame(raw.as_ptr(), &runtime.descriptor, runtime.transport, frame)? };
    let result = unsafe { pw::sys::pw_stream_queue_buffer(stream.as_raw_ptr(), raw.as_ptr()) };
    if result < 0 {
        return Err(VideoSourceRuntimeError::PipeWire(format!(
            "queue PipeWire buffer returned {result}"
        )));
    }
    Ok(())
}

unsafe fn fill_frame(
    raw: *mut pw::sys::pw_buffer,
    descriptor: &VideoBuffer,
    transport: Option<PipeWireBufferTransport>,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    let pipewire_buffer = unsafe { raw.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null pw_buffer"),
    )?;
    let spa_buffer = pipewire_buffer.buffer;
    let buffer = unsafe { spa_buffer.as_mut() }.ok_or(
        VideoSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    unsafe { fill_optional_frame_metadata(spa_buffer, frame)? };

    if transport == Some(PipeWireBufferTransport::SyncTimeline) {
        let point = frame
            .acquire_point
            .ok_or(VideoSourceRuntimeError::MissingAcquirePoint(
                frame.buffer_id.get(),
            ))?;
        let sync = unsafe { sync_meta(spa_buffer) }.ok_or(
            VideoSourceRuntimeError::InvalidPipeWireBuffer("missing sync metadata"),
        )?;
        unsafe {
            *sync.as_ptr() = spa::sys::spa_meta_sync_timeline {
                flags: spa::sys::SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,
                padding: 0,
                acquire_point: point.get(),
                release_point: point.get(),
            };
        }
    }

    if buffer.datas.is_null() || buffer.n_datas == 0 {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing frame data plane",
        ));
    }
    let data = unsafe { &mut *buffer.datas };
    if data.chunk.is_null() {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "missing frame chunk",
        ));
    }
    let chunk = unsafe { &mut *data.chunk };
    chunk.offset = 0;
    chunk.size = descriptor.layout.size.get() as u32;
    chunk.stride = descriptor.layout.pitch.get() as i32;
    Ok(())
}

unsafe fn fill_optional_frame_metadata(
    buffer: *mut spa::sys::spa_buffer,
    frame: VideoFrame,
) -> Result<(), VideoSourceRuntimeError> {
    // ParamMeta values are offers. A consumer may omit Header and VideoDamage
    // while still accepting the DMA-BUF data plane, so populate either one
    // when present without making it a transport prerequisite.
    let header = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            buffer,
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        )
    }
    .cast::<spa::sys::spa_meta_header>();
    if let Some(header) = unsafe { header.as_mut() } {
        header.flags = if frame.discontinuity {
            spa::sys::SPA_META_HEADER_FLAG_DISCONT
        } else {
            0
        };
        header.offset = 0;
        header.pts = frame.pts_ns;
        header.dts_offset = 0;
        header.seq = frame.sequence;
    }

    let damage_meta =
        unsafe { spa::sys::spa_buffer_find_meta(buffer, spa::sys::SPA_META_VideoDamage) };
    let Some(damage_meta) = (unsafe { damage_meta.as_mut() }) else {
        return Ok(());
    };
    if damage_meta.data.is_null()
        || damage_meta.size < std::mem::size_of::<spa::sys::spa_meta_region>() as u32
    {
        return Err(VideoSourceRuntimeError::InvalidPipeWireBuffer(
            "short damage metadata",
        ));
    }
    let region_count = damage_meta.size as usize / std::mem::size_of::<spa::sys::spa_meta_region>();
    let regions = unsafe {
        std::slice::from_raw_parts_mut(
            damage_meta.data.cast::<spa::sys::spa_meta_region>(),
            region_count,
        )
    };
    for region in regions.iter_mut() {
        region.region = spa::sys::spa_region {
            position: spa::sys::spa_point { x: 0, y: 0 },
            size: spa::sys::spa_rectangle {
                width: 0,
                height: 0,
            },
        };
    }
    regions[0].region = spa::sys::spa_region {
        position: spa::sys::spa_point {
            x: frame.damage.x as i32,
            y: frame.damage.y as i32,
        },
        size: spa::sys::spa_rectangle {
            width: frame.damage.width.get(),
            height: frame.damage.height.get(),
        },
    };
    Ok(())
}

unsafe fn sync_meta(
    buffer: *mut spa::sys::spa_buffer,
) -> Option<NonNull<spa::sys::spa_meta_sync_timeline>> {
    if buffer.is_null() {
        return None;
    }
    let meta = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            buffer,
            spa::sys::SPA_META_SyncTimeline,
            std::mem::size_of::<spa::sys::spa_meta_sync_timeline>(),
        )
    };
    NonNull::new(meta.cast())
}

fn pipewire_error(operation: &'static str, error: pw::Error) -> VideoSourceRuntimeError {
    VideoSourceRuntimeError::PipeWire(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            pitch: NonZeroU32::new(7680).unwrap(),
            size: NonZeroU64::new(8_294_400).unwrap(),
            modifier: 0,
        };
        let bytes = format_parameter(NonZeroU32::new(60).unwrap(), layout).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut info = VideoInfoRaw::new();
        info.parse(pod).unwrap();

        assert_eq!(info.format(), VideoFormat::BGRx);
        assert_eq!(info.size().width, 1920);
        assert_eq!(info.size().height, 1080);
        assert!(!info.flags().contains(VideoFlags::MODIFIER));

        let non_linear = crate::VideoBufferLayout {
            modifier: 1,
            ..layout
        };
        assert!(matches!(
            format_parameter(NonZeroU32::new(60).unwrap(), non_linear),
            Err(VideoSourceRuntimeError::UnsupportedFormat)
        ));
    }

    #[test]
    fn a_disconnected_consumer_clears_format_without_poisoning_the_source() {
        assert!(matches!(
            classify_format_change(None),
            FormatChange::Cleared
        ));

        let layout = crate::VideoBufferLayout {
            width: NonZeroU32::new(1920).unwrap(),
            height: NonZeroU32::new(1080).unwrap(),
            pitch: NonZeroU32::new(7680).unwrap(),
            size: NonZeroU64::new(8_294_400).unwrap(),
            modifier: 0,
        };
        let bytes = format_parameter(NonZeroU32::new(60).unwrap(), layout).unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        assert!(matches!(
            classify_format_change(Some(pod)),
            FormatChange::Negotiated(_)
        ));
    }
}
