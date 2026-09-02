use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, OwnedFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::time::TimeValLike;
use nix::time::{clock_gettime, ClockId};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id, SpaTypes};
use tokio::sync::{mpsc, oneshot};

use crate::audio_source::{
    AudioNodeIdentity, AudioSourceConfig, AudioSourceEvent, AudioSourceRuntimeError,
    AudioStartupSlot,
};
use crate::policy_gate::{
    PolicyGate, PolicyMarkerChange, PRIVATE_NODE_POLICY_VERSION, PRIVATE_NODE_PROPERTY,
};
use crate::PipeWireRemote;

const EVENT_QUEUE_CAPACITY: usize = 8;
const CORE_OBJECT_ID: u32 = 0;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;
const AUDIO_FRAME_BYTES: usize = 4;
const AUDIO_PERIOD_FRAMES: usize = 480;
const AUDIO_PERIOD_BYTES: usize = AUDIO_PERIOD_FRAMES * AUDIO_FRAME_BYTES;
const AUDIO_PERIOD: Duration = Duration::from_millis(10);
const AUDIO_BUFFER_COUNT: i32 = 8;
const AUDIO_MAX_BUFFER_COUNT: i32 = 16;

pub(crate) enum Command {
    Shutdown,
}

pub(crate) struct RuntimeHandle {
    pub commands: pw::channel::Sender<Command>,
    pub startup_cancel: pw::channel::Sender<()>,
    pub events: mpsc::Receiver<AudioSourceEvent>,
    pub startup: oneshot::Receiver<Result<AudioNodeIdentity, AudioSourceRuntimeError>>,
    pub thread: JoinHandle<()>,
}

pub(crate) fn spawn(
    config: AudioSourceConfig,
    tap: OwnedFd,
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
        .name("pronk-pw-audio-source".to_string())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run(
                    config,
                    tap,
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
                    let _ = supervisor_events.blocking_send(AudioSourceEvent::Failed(error));
                }
                Err(_) => {
                    let error = AudioSourceRuntimeError::ThreadPanicked;
                    send_startup(&supervisor_startup, Err(error.clone()));
                    let _ = supervisor_events.blocking_send(AudioSourceEvent::Failed(error));
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

struct ThreadState {
    config: AudioSourceConfig,
    tap: OwnedFd,
    events: mpsc::Sender<AudioSourceEvent>,
    startup: AudioStartupSlot,
    identity: Option<AudioNodeIdentity>,
    stream_node_id: Option<NonZeroU32>,
    first_process: bool,
    timeline: AudioTimeline,
    failed: bool,
    shutting_down: bool,
}

#[derive(Debug, Default)]
struct AudioTimeline {
    origin_ns: Option<i64>,
    published_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioBufferTiming {
    presentation_timestamp_ns: i64,
    first_frame: u64,
    discontinuity: bool,
}

impl AudioTimeline {
    fn next(
        &mut self,
        monotonic_now_ns: i64,
    ) -> Result<AudioBufferTiming, AudioSourceRuntimeError> {
        let discontinuity = self.origin_ns.is_none();
        let origin_ns = match self.origin_ns {
            Some(origin_ns) => origin_ns,
            None if monotonic_now_ns >= 0 => {
                self.origin_ns = Some(monotonic_now_ns);
                monotonic_now_ns
            }
            None => {
                return Err(AudioSourceRuntimeError::PipeWire(
                    "audio stream reported a negative monotonic time".to_string(),
                ));
            }
        };
        let elapsed_ns = u128::from(self.published_frames)
            .checked_mul(1_000_000_000)
            .and_then(|value| value.checked_div(u128::from(AUDIO_RATE)))
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| {
                AudioSourceRuntimeError::PipeWire(
                    "audio presentation timestamp overflowed".to_string(),
                )
            })?;
        let presentation_timestamp_ns = origin_ns.checked_add(elapsed_ns).ok_or_else(|| {
            AudioSourceRuntimeError::PipeWire("audio presentation timestamp overflowed".to_string())
        })?;
        let first_frame = self.published_frames;
        self.published_frames = self
            .published_frames
            .checked_add(AUDIO_PERIOD_FRAMES as u64)
            .ok_or_else(|| {
                AudioSourceRuntimeError::PipeWire("audio frame sequence overflowed".to_string())
            })?;
        Ok(AudioBufferTiming {
            presentation_timestamp_ns,
            first_frame,
            discontinuity,
        })
    }
}

impl ThreadState {
    fn observe_node(
        &mut self,
        object_id: u32,
        node_name: &str,
        object_serial: &str,
    ) -> Result<(), AudioSourceRuntimeError> {
        if node_name != self.config.node_name {
            return Ok(());
        }
        let object_id = NonZeroU32::new(object_id).ok_or_else(|| {
            AudioSourceRuntimeError::PipeWire("audio source node has zero object ID".to_string())
        })?;
        let object_serial = object_serial
            .parse::<u64>()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                AudioSourceRuntimeError::PipeWire(
                    "audio source node has invalid object.serial".to_string(),
                )
            })?;
        self.identity = Some(AudioNodeIdentity {
            node_name: self.config.node_name.clone(),
            object_id,
            object_serial,
            media_generation: self.config.media_generation,
        });
        self.maybe_complete_startup()
    }

    fn observe_stream_node(&mut self, object_id: u32) -> Result<(), AudioSourceRuntimeError> {
        self.stream_node_id = NonZeroU32::new(object_id);
        if self.stream_node_id.is_none() || object_id == pw::constants::ID_ANY {
            return Err(AudioSourceRuntimeError::PipeWire(
                "audio source stream has no node ID".to_string(),
            ));
        }
        self.maybe_complete_startup()
    }

    fn maybe_complete_startup(&mut self) -> Result<(), AudioSourceRuntimeError> {
        let (Some(identity), Some(stream_node_id)) = (&self.identity, self.stream_node_id) else {
            return Ok(());
        };
        if identity.object_id != stream_node_id {
            return Err(AudioSourceRuntimeError::PipeWire(
                "audio registry node identity differs from stream node ID".to_string(),
            ));
        }
        send_startup(&self.startup, Ok(identity.clone()));
        Ok(())
    }

    fn fail(&mut self, error: AudioSourceRuntimeError) {
        if self.failed || self.shutting_down {
            return;
        }
        self.failed = true;
        send_startup(&self.startup, Err(error.clone()));
        let _ = self.events.try_send(AudioSourceEvent::Failed(error));
    }
}

fn send_startup(
    startup: &AudioStartupSlot,
    result: Result<AudioNodeIdentity, AudioSourceRuntimeError>,
) {
    if let Some(sender) = startup.lock().expect("audio startup mutex poisoned").take() {
        let _ = sender.send(result);
    }
}

fn fail(
    state: &Rc<RefCell<ThreadState>>,
    mainloop: &pw::main_loop::MainLoopRc,
    error: AudioSourceRuntimeError,
) {
    state.borrow_mut().fail(error);
    mainloop.quit();
}

fn run(
    config: AudioSourceConfig,
    tap: OwnedFd,
    remote: PipeWireRemote,
    command_receiver: pw::channel::Receiver<Command>,
    startup_cancel_receiver: pw::channel::Receiver<()>,
    events: mpsc::Sender<AudioSourceEvent>,
    startup: AudioStartupSlot,
) -> Result<(), AudioSourceRuntimeError> {
    let requires_policy = matches!(&remote, PipeWireRemote::Connected(_));
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| pipewire_error("create audio main loop", error))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| pipewire_error("create audio context", error))?;
    let core = match remote {
        PipeWireRemote::Connected(fd) => context.connect_fd_rc(fd, None),
        PipeWireRemote::AmbientDevelopment => context.connect_rc(None),
    }
    .map_err(|error| pipewire_error("connect audio core", error))?;
    let registry = core
        .get_registry_rc()
        .map_err(|error| pipewire_error("get audio registry", error))?;
    let state = Rc::new(RefCell::new(ThreadState {
        config,
        tap,
        events,
        startup,
        identity: None,
        stream_node_id: None,
        first_process: true,
        timeline: AudioTimeline::default(),
        failed: false,
        shutting_down: false,
    }));
    let policy_gate = Rc::new(RefCell::new(PolicyGate::new(requires_policy)));
    let initial_sync_seq = Rc::new(Cell::new(None));
    let initial_sync_complete = Rc::new(Cell::new(false));

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
                    AudioSourceRuntimeError::Core {
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
            let result =
                state_for_global
                    .borrow_mut()
                    .observe_node(global.id, node_name, object_serial);
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
                    AudioSourceRuntimeError::PolicyUnavailable,
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
                    AudioSourceRuntimeError::NodeRemoved,
                );
            }
        })
        .register();

    let sync = core
        .sync(0)
        .map_err(|error| pipewire_error("synchronize audio policy registry", error))?;
    initial_sync_seq.set(Some(sync.seq()));
    mainloop.run();
    if state.borrow().failed || state.borrow().shutting_down {
        return Ok(());
    }
    if !initial_sync_complete.get() {
        return Err(AudioSourceRuntimeError::PipeWire(
            "audio policy registry synchronization stopped unexpectedly".to_string(),
        ));
    }
    if !policy_gate.borrow().is_open() {
        return Err(AudioSourceRuntimeError::PolicyUnavailable);
    }

    let properties = source_properties(&state.borrow().config);
    let stream =
        pw::stream::StreamRc::new(core.clone(), &state.borrow().config.node_name, properties)
            .map_err(|error| pipewire_error("create audio source stream", error))?;

    let state_for_stream = state.clone();
    let mainloop_for_stream = mainloop.clone();
    let state_for_param = state.clone();
    let mainloop_for_param = mainloop.clone();
    let state_for_process = state.clone();
    let mainloop_for_process = mainloop.clone();
    let _stream_listener = stream
        .add_local_listener::<()>()
        .state_changed(move |stream, _, _old, new| match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                let result = state_for_stream
                    .borrow_mut()
                    .observe_stream_node(stream.node_id());
                if let Err(error) = result {
                    fail(&state_for_stream, &mainloop_for_stream, error);
                }
            }
            pw::stream::StreamState::Error(message) => fail(
                &state_for_stream,
                &mainloop_for_stream,
                AudioSourceRuntimeError::Stream(message),
            ),
            pw::stream::StreamState::Unconnected if !state_for_stream.borrow().shutting_down => {
                fail(
                    &state_for_stream,
                    &mainloop_for_stream,
                    AudioSourceRuntimeError::Stream("audio source stream disconnected".to_string()),
                );
            }
            _ => {}
        })
        .param_changed(move |stream, _, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let result = negotiate_buffers(stream, param);
            if let Err(error) = result {
                fail(&state_for_param, &mainloop_for_param, error);
            }
        })
        .process(move |stream, _| {
            let result = process_audio(stream, &mut state_for_process.borrow_mut());
            if let Err(error) = result {
                fail(&state_for_process, &mainloop_for_process, error);
            }
        })
        .register()
        .map_err(|error| pipewire_error("register audio stream listener", error))?;

    let state_for_commands = state.clone();
    let mainloop_for_commands = mainloop.clone();
    let _commands = command_receiver.attach(mainloop.loop_(), move |command| match command {
        Command::Shutdown => {
            state_for_commands.borrow_mut().shutting_down = true;
            mainloop_for_commands.quit();
        }
    });

    let stream_for_timer = stream.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if let Err(error) = stream_for_timer.trigger_process() {
            tracing::trace!(%error, "PipeWire audio graph trigger was coalesced");
        }
    });
    timer
        .update_timer(Some(AUDIO_PERIOD), Some(AUDIO_PERIOD))
        .into_result()
        .map_err(|error| {
            AudioSourceRuntimeError::PipeWire(format!("arm audio source timer: {error}"))
        })?;

    let format = format_parameter()?;
    let mut params = [Pod::from_bytes(&format).ok_or_else(|| {
        AudioSourceRuntimeError::PipeWire("serialize PipeWire audio format pod".to_string())
    })?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::EXCLUSIVE,
            &mut params,
        )
        .map_err(|error| pipewire_error("connect audio source stream", error))?;

    mainloop.run();

    let state = state.borrow_mut();
    if !state.failed {
        if !state.shutting_down {
            return Err(AudioSourceRuntimeError::Stream(
                "PipeWire audio loop stopped unexpectedly".to_string(),
            ));
        }
        send_startup(
            &state.startup,
            Err(AudioSourceRuntimeError::Stream(
                "PipeWire audio source stopped before publishing".to_string(),
            )),
        );
        let _ = state.events.try_send(AudioSourceEvent::Stopped);
    }
    Ok(())
}

fn validate_format(param: Option<&Pod>) -> Result<(), AudioSourceRuntimeError> {
    let Some(param) = param else {
        return Ok(());
    };
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(param)
        .map_err(|_| AudioSourceRuntimeError::UnsupportedFormat)?;
    let mut info = AudioInfoRaw::new();
    info.parse(param)
        .map_err(|_| AudioSourceRuntimeError::UnsupportedFormat)?;
    if media_type != MediaType::Audio
        || media_subtype != MediaSubtype::Raw
        || info.format() != AudioFormat::S16LE
        || info.rate() != AUDIO_RATE
        || info.channels() != AUDIO_CHANNELS
    {
        return Err(AudioSourceRuntimeError::UnsupportedFormat);
    }
    Ok(())
}

fn negotiate_buffers(
    stream: &pw::stream::Stream,
    format: Option<&Pod>,
) -> Result<(), AudioSourceRuntimeError> {
    validate_format(format)?;
    let Some(_) = format else {
        return Ok(());
    };
    let mem_ptr_flag = 1i32
        .checked_shl(spa::sys::SPA_DATA_MemPtr)
        .ok_or_else(|| invalid_data_type("MemPtr"))?;
    let mem_fd_flag = 1i32
        .checked_shl(spa::sys::SPA_DATA_MemFd)
        .ok_or_else(|| invalid_data_type("MemFd"))?;
    let data_type_flags = mem_ptr_flag | mem_fd_flag;
    let values = [
        Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
            id: spa::sys::SPA_PARAM_Buffers,
            properties: vec![
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_buffers,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Range {
                            default: AUDIO_BUFFER_COUNT,
                            min: 2,
                            max: AUDIO_MAX_BUFFER_COUNT,
                        },
                    ))),
                ),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_size,
                    Value::Int(AUDIO_PERIOD_BYTES as i32),
                ),
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_stride,
                    Value::Int(AUDIO_FRAME_BYTES as i32),
                ),
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Flags {
                            default: data_type_flags,
                            flags: vec![mem_ptr_flag, mem_fd_flag],
                        },
                    ))),
                ),
            ],
        }),
        Value::Object(Object {
            type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
            id: spa::sys::SPA_PARAM_Meta,
            properties: vec![
                Property::new(
                    spa::sys::SPA_PARAM_META_type,
                    Value::Id(Id(spa::sys::SPA_META_Header)),
                ),
                Property::new(
                    spa::sys::SPA_PARAM_META_size,
                    Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
                ),
            ],
        }),
    ];
    let serialized = values
        .iter()
        .map(|value| {
            PodSerializer::serialize(Cursor::new(Vec::new()), value)
                .map(|(cursor, _)| cursor.into_inner())
                .map_err(|error| {
                    AudioSourceRuntimeError::PipeWire(format!(
                        "serialize PipeWire audio buffer parameter: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut params = serialized
        .iter()
        .map(|bytes| {
            Pod::from_bytes(bytes).ok_or_else(|| {
                AudioSourceRuntimeError::PipeWire(
                    "serialize PipeWire audio buffer parameter".to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    stream
        .update_params(&mut params)
        .map_err(|error| pipewire_error("update audio source buffer parameters", error))
}

fn invalid_data_type(name: &str) -> AudioSourceRuntimeError {
    AudioSourceRuntimeError::PipeWire(format!("invalid PipeWire {name} data type"))
}

fn process_audio(
    stream: &pw::stream::Stream,
    state: &mut ThreadState,
) -> Result<(), AudioSourceRuntimeError> {
    let Some(raw) = NonNull::new(unsafe { stream.dequeue_raw_buffer() }) else {
        return Ok(());
    };
    let result = unsafe { fill_audio_buffer(raw, state) };
    unsafe { stream.queue_raw_buffer(raw.as_ptr()) };
    result
}

unsafe fn fill_audio_buffer(
    mut raw: NonNull<pw::sys::pw_buffer>,
    state: &mut ThreadState,
) -> Result<(), AudioSourceRuntimeError> {
    let mut spa_buffer = NonNull::new(unsafe { raw.as_ref().buffer }).ok_or(
        AudioSourceRuntimeError::InvalidPipeWireBuffer("null spa_buffer"),
    )?;
    let buffer = unsafe { spa_buffer.as_mut() };
    if buffer.n_datas == 0 || buffer.datas.is_null() {
        return Err(AudioSourceRuntimeError::InvalidPipeWireBuffer(
            "missing data plane",
        ));
    }
    let data = unsafe { &mut *buffer.datas };
    if data.data.is_null() {
        return Err(AudioSourceRuntimeError::InvalidPipeWireBuffer(
            "unmapped data plane",
        ));
    }
    let samples =
        unsafe { std::slice::from_raw_parts_mut(data.data.cast::<u8>(), data.maxsize as usize) };
    if samples.len() < AUDIO_PERIOD_BYTES {
        return Err(AudioSourceRuntimeError::InvalidPipeWireBuffer(
            "data plane is smaller than one 10 ms audio period",
        ));
    }
    let bytes = AUDIO_PERIOD_BYTES;

    if state.first_process {
        drain_audio_tap(state.tap.as_raw_fd())?;
        state.first_process = false;
    }
    samples[..bytes].fill(0);
    let mut filled = 0;
    while filled < bytes {
        match nix::unistd::read(state.tap.as_raw_fd(), &mut samples[filled..bytes]) {
            Ok(0) => {
                return Err(AudioSourceRuntimeError::AudioTap(
                    "tap returned end of file".to_string(),
                ));
            }
            Ok(count) if count % AUDIO_FRAME_BYTES != 0 => {
                return Err(AudioSourceRuntimeError::AudioTap(format!(
                    "tap returned a partial audio frame ({count} bytes)"
                )));
            }
            Ok(count) => filled += count,
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => break,
            Err(error) => return Err(AudioSourceRuntimeError::AudioTap(error.to_string())),
        }
    }

    let chunk = unsafe { data.chunk.as_mut() }.ok_or(
        AudioSourceRuntimeError::InvalidPipeWireBuffer("missing audio chunk"),
    )?;
    chunk.offset = 0;
    chunk.stride = AUDIO_FRAME_BYTES as i32;
    chunk.size = bytes as u32;
    unsafe { raw.as_mut() }.size = AUDIO_PERIOD_FRAMES as u64;

    let monotonic_now_ns = if state.timeline.origin_ns.is_none() {
        clock_gettime(ClockId::CLOCK_MONOTONIC)
            .map_err(|error| {
                AudioSourceRuntimeError::PipeWire(format!(
                    "read monotonic clock for audio presentation timestamp: {error}"
                ))
            })?
            .num_nanoseconds()
    } else {
        0
    };
    let timing = state.timeline.next(monotonic_now_ns)?;
    let header = unsafe {
        spa::sys::spa_buffer_find_meta_data(
            spa_buffer.as_ptr(),
            spa::sys::SPA_META_Header,
            std::mem::size_of::<spa::sys::spa_meta_header>(),
        )
    }
    .cast::<spa::sys::spa_meta_header>();
    if let Some(header) = unsafe { header.as_mut() } {
        header.flags = if timing.discontinuity {
            spa::sys::SPA_META_HEADER_FLAG_DISCONT
        } else {
            0
        };
        header.offset = 0;
        header.pts = timing.presentation_timestamp_ns;
        header.dts_offset = 0;
        header.seq = timing.first_frame;
    }
    Ok(())
}

fn drain_audio_tap(fd: std::os::fd::RawFd) -> Result<(), AudioSourceRuntimeError> {
    let mut scratch = [0_u8; 4096];
    loop {
        match nix::unistd::read(fd, &mut scratch) {
            Ok(0) => {
                return Err(AudioSourceRuntimeError::AudioTap(
                    "tap returned end of file".to_string(),
                ));
            }
            Ok(_) => {}
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => return Ok(()),
            Err(error) => return Err(AudioSourceRuntimeError::AudioTap(error.to_string())),
        }
    }
}

fn source_properties(config: &AudioSourceConfig) -> pw::properties::PropertiesBox {
    let connector_id = config.connector_id.to_string();
    let output_index = config.output_index.to_string();
    let grant_id = config.grant_id.to_string();
    let media_generation = config.media_generation.to_string();
    let node_latency = format!("{AUDIO_PERIOD_FRAMES}/{AUDIO_RATE}");
    let node_rate = format!("1/{AUDIO_RATE}");
    properties! {
        *pw::keys::MEDIA_CLASS => "Audio/Source",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => config.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => config.node_description.as_str(),
        *pw::keys::NODE_EXCLUSIVE => "true",
        *pw::keys::NODE_LATENCY => node_latency,
        *pw::keys::NODE_RATE => node_rate,
        "node.reliable" => "true",
        *pw::keys::NODE_VIRTUAL => "true",
        "device.api" => "castkms-audio-tap",
        PRIVATE_NODE_PROPERTY => PRIVATE_NODE_POLICY_VERSION,
        "api.pronk.kernel-audio-tap" => "v1",
        "api.pronk.session-id" => config.session_id.as_str(),
        "api.pronk.device-instance" => config.device_instance.as_str(),
        "api.pronk.connector-id" => connector_id,
        "api.pronk.output-index" => output_index,
        "api.pronk.grant-id" => grant_id,
        "api.pronk.media-generation" => media_generation
    }
}

fn format_parameter() -> Result<Vec<u8>, AudioSourceRuntimeError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|error| {
            AudioSourceRuntimeError::PipeWire(format!("serialize PipeWire audio format: {error}"))
        })
}

fn pipewire_error(operation: &'static str, error: pw::Error) -> AudioSourceRuntimeError {
    AudioSourceRuntimeError::PipeWire(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_timeline_uses_monotonic_origin_and_sample_sequence() {
        let mut timeline = AudioTimeline::default();

        assert_eq!(
            timeline.next(7_000_000_000).unwrap(),
            AudioBufferTiming {
                presentation_timestamp_ns: 7_000_000_000,
                first_frame: 0,
                discontinuity: true,
            }
        );
        assert_eq!(
            timeline.next(1).unwrap(),
            AudioBufferTiming {
                presentation_timestamp_ns: 7_010_000_000,
                first_frame: 480,
                discontinuity: false,
            }
        );
        assert_eq!(
            timeline.next(1).unwrap(),
            AudioBufferTiming {
                presentation_timestamp_ns: 7_020_000_000,
                first_frame: 960,
                discontinuity: false,
            }
        );
    }

    #[test]
    fn audio_timeline_rejects_a_negative_origin() {
        assert!(AudioTimeline::default().next(-1).is_err());
    }
}
