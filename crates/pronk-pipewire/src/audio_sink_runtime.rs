//! One-shot libpipewire registry snapshot for CastKMS audio resolution.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::thread::JoinHandle;

use pipewire as pw;
use tokio::sync::oneshot;

use crate::audio_sink_model::{
    resolve_audio_sink, AudioObjectKind, AudioObjectObservation, AudioSinkResolutionError,
    CASTKMS_AUDIO_OUTPUT_INDEX_PROPERTY, CASTKMS_AUDIO_SINK_PROPERTY,
};
use crate::policy_gate::PolicyGate;
use crate::{CastKmsAudioSinkRequest, CastKmsAudioSinkTarget, PipeWireRemote};

const CORE_OBJECT_ID: u32 = 0;

enum BoundAudioObject {
    Device {
        _proxy: pw::device::Device,
        _listener: pw::device::DeviceListener,
    },
    Node {
        _proxy: pw::node::Node,
        _listener: pw::node::NodeListener,
    },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub(crate) enum AudioSinkRuntimeError {
    #[error("create or connect PipeWire audio resolver: {0}")]
    PipeWire(String),
    #[error("PipeWire core error {code}: {message}")]
    Core { code: i32, message: String },
    #[error("the versioned WirePlumber private-media policy is unavailable")]
    PolicyUnavailable,
    #[error(transparent)]
    Resolution(#[from] AudioSinkResolutionError),
    #[error("PipeWire audio resolution was cancelled")]
    Cancelled,
    #[error("PipeWire audio resolver thread panicked")]
    ThreadPanicked,
}

pub(crate) struct RuntimeHandle {
    pub cancel: pw::channel::Sender<()>,
    pub result: oneshot::Receiver<Result<CastKmsAudioSinkTarget, AudioSinkRuntimeError>>,
    pub thread: JoinHandle<()>,
}

pub(crate) fn spawn(
    request: CastKmsAudioSinkRequest,
    remote: PipeWireRemote,
) -> Result<RuntimeHandle, std::io::Error> {
    let (cancel, cancel_receiver) = pw::channel::channel();
    let (result_tx, result) = oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("pronk-pw-audio-resolve".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| run(request, remote, cancel_receiver)))
                .unwrap_or(Err(AudioSinkRuntimeError::ThreadPanicked));
            let _ = result_tx.send(result);
        })?;
    Ok(RuntimeHandle {
        cancel,
        result,
        thread,
    })
}

fn run(
    request: CastKmsAudioSinkRequest,
    remote: PipeWireRemote,
    cancel: pw::channel::Receiver<()>,
) -> Result<CastKmsAudioSinkTarget, AudioSinkRuntimeError> {
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

    let observations = Rc::new(RefCell::new(HashMap::new()));
    let bound_objects = Rc::new(RefCell::new(HashMap::new()));
    let policy_gate = Rc::new(RefCell::new(PolicyGate::new(requires_policy)));
    let sync_seq = Rc::new(Cell::new(None));
    let bound_sync_started = Rc::new(Cell::new(false));
    let sync_complete = Rc::new(Cell::new(false));
    let resolved_target = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(Cell::new(false));
    let runtime_error = Rc::new(RefCell::new(None));

    let cancelled_for_cancel = cancelled.clone();
    let mainloop_for_cancel = mainloop.clone();
    let _cancel = cancel.attach(mainloop.loop_(), move |()| {
        cancelled_for_cancel.set(true);
        mainloop_for_cancel.quit();
    });

    let sync_seq_for_done = sync_seq.clone();
    let bound_sync_started_for_done = bound_sync_started.clone();
    let sync_complete_for_done = sync_complete.clone();
    let mainloop_for_done = mainloop.clone();
    let core_for_done = core.clone();
    let runtime_error_for_done = runtime_error.clone();
    let observations_for_done = observations.clone();
    let gate_for_done = policy_gate.clone();
    let resolved_target_for_done = resolved_target.clone();
    let request_for_done = request.clone();
    let runtime_error_for_core = runtime_error.clone();
    let mainloop_for_core = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == CORE_OBJECT_ID && sync_seq_for_done.get() == Some(seq.seq()) {
                if bound_sync_started_for_done.replace(true) {
                    sync_complete_for_done.set(true);
                    if !gate_for_done.borrow().is_open() {
                        runtime_error_for_done
                            .borrow_mut()
                            .get_or_insert(AudioSinkRuntimeError::PolicyUnavailable);
                        mainloop_for_done.quit();
                    } else {
                        finish_resolution_if_ready(
                            &request_for_done,
                            &observations_for_done,
                            &gate_for_done,
                            &sync_complete_for_done,
                            &resolved_target_for_done,
                            &runtime_error_for_done,
                            &mainloop_for_done,
                        );
                    }
                } else {
                    match core_for_done.sync(0) {
                        Ok(sync) => sync_seq_for_done.set(Some(sync.seq())),
                        Err(error) => {
                            runtime_error_for_done.borrow_mut().get_or_insert_with(|| {
                                pipewire_error("synchronize bound audio objects", error)
                            });
                            mainloop_for_done.quit();
                        }
                    }
                }
            }
        })
        .error(move |_id, _seq, code, message| {
            if code < 0 {
                runtime_error_for_core
                    .borrow_mut()
                    .get_or_insert(AudioSinkRuntimeError::Core {
                        code,
                        message: message.to_string(),
                    });
                mainloop_for_core.quit();
            }
        })
        .register();

    let observations_for_global = observations.clone();
    let bound_objects_for_global = bound_objects.clone();
    let registry_for_global = registry.downgrade();
    let gate_for_global = policy_gate.clone();
    let runtime_error_for_global = runtime_error.clone();
    let mainloop_for_global = mainloop.clone();
    let sync_complete_for_global = sync_complete.clone();
    let resolved_target_for_global = resolved_target.clone();
    let request_for_global = request.clone();
    let observations_for_remove = observations.clone();
    let bound_objects_for_remove = bound_objects.clone();
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
            let Some(registry) = registry_for_global.upgrade() else {
                return;
            };
            let bound = if global.type_ == pw::types::ObjectType::Device {
                registry.bind::<pw::device::Device, _>(global).map(|proxy| {
                    let observations = observations_for_global.clone();
                    let gate = gate_for_global.clone();
                    let sync_complete = sync_complete_for_global.clone();
                    let resolved_target = resolved_target_for_global.clone();
                    let runtime_error = runtime_error_for_global.clone();
                    let mainloop = mainloop_for_global.clone();
                    let request = request_for_global.clone();
                    let listener = proxy
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(props) = info.props() {
                                observations.borrow_mut().insert(
                                    info.id(),
                                    observation(info.id(), AudioObjectKind::Device, props),
                                );
                                finish_resolution_if_ready(
                                    &request,
                                    &observations,
                                    &gate,
                                    &sync_complete,
                                    &resolved_target,
                                    &runtime_error,
                                    &mainloop,
                                );
                            }
                        })
                        .register();
                    BoundAudioObject::Device {
                        _proxy: proxy,
                        _listener: listener,
                    }
                })
            } else if global.type_ == pw::types::ObjectType::Node {
                registry.bind::<pw::node::Node, _>(global).map(|proxy| {
                    let observations = observations_for_global.clone();
                    let gate = gate_for_global.clone();
                    let sync_complete = sync_complete_for_global.clone();
                    let resolved_target = resolved_target_for_global.clone();
                    let runtime_error = runtime_error_for_global.clone();
                    let mainloop = mainloop_for_global.clone();
                    let request = request_for_global.clone();
                    let listener = proxy
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(props) = info.props() {
                                observations.borrow_mut().insert(
                                    info.id(),
                                    observation(info.id(), AudioObjectKind::Node, props),
                                );
                                finish_resolution_if_ready(
                                    &request,
                                    &observations,
                                    &gate,
                                    &sync_complete,
                                    &resolved_target,
                                    &runtime_error,
                                    &mainloop,
                                );
                            }
                        })
                        .register();
                    BoundAudioObject::Node {
                        _proxy: proxy,
                        _listener: listener,
                    }
                })
            } else {
                return;
            };
            match bound {
                Ok(bound) => {
                    bound_objects_for_global
                        .borrow_mut()
                        .insert(global.id, bound);
                }
                Err(error) => {
                    runtime_error_for_global
                        .borrow_mut()
                        .get_or_insert_with(|| pipewire_error("bind audio registry object", error));
                    mainloop_for_global.quit();
                }
            }
        })
        .global_remove(move |id| {
            observations_for_remove.borrow_mut().remove(&id);
            bound_objects_for_remove.borrow_mut().remove(&id);
            gate_for_remove.borrow_mut().remove_object(id);
        })
        .register();

    let sync = core
        .sync(0)
        .map_err(|error| pipewire_error("synchronize audio registry", error))?;
    sync_seq.set(Some(sync.seq()));
    mainloop.run();

    if cancelled.get() {
        return Err(AudioSinkRuntimeError::Cancelled);
    }
    if let Some(error) = runtime_error.borrow_mut().take() {
        return Err(error);
    }
    if !sync_complete.get() {
        return Err(AudioSinkRuntimeError::PipeWire(
            "audio registry synchronization stopped unexpectedly".into(),
        ));
    }
    let resolved = resolved_target.borrow_mut().take().ok_or_else(|| {
        AudioSinkRuntimeError::PipeWire(
            "audio registry synchronization stopped before a matching sink appeared".into(),
        )
    });
    resolved
}

fn finish_resolution_if_ready(
    request: &CastKmsAudioSinkRequest,
    observations: &RefCell<HashMap<u32, AudioObjectObservation>>,
    policy_gate: &RefCell<PolicyGate>,
    sync_complete: &Cell<bool>,
    resolved_target: &RefCell<Option<CastKmsAudioSinkTarget>>,
    runtime_error: &RefCell<Option<AudioSinkRuntimeError>>,
    mainloop: &pw::main_loop::MainLoopRc,
) {
    if !sync_complete.get()
        || !policy_gate.borrow().is_open()
        || resolved_target.borrow().is_some()
        || runtime_error.borrow().is_some()
    {
        return;
    }

    let result = resolve_audio_sink(request, observations.borrow().values().cloned());
    match result {
        Ok(target) => {
            resolved_target.borrow_mut().replace(target);
            mainloop.quit();
        }
        Err(AudioSinkResolutionError::NotFound { .. }) => {
            // ALSA devices and their PCM nodes are separate registry objects.
            // Keep listening until the matching node arrives or the public
            // resolver timeout cancels this foreign loop.
        }
        Err(error) => {
            runtime_error.borrow_mut().replace(error.into());
            mainloop.quit();
        }
    }
}

fn copy_property(props: &pw::spa::utils::dict::DictRef, key: &str) -> Option<String> {
    props.get(key).map(str::to_owned)
}

fn observation(
    object_id: u32,
    kind: AudioObjectKind,
    props: &pw::spa::utils::dict::DictRef,
) -> AudioObjectObservation {
    AudioObjectObservation {
        object_id,
        kind,
        media_class: copy_property(props, "media.class"),
        // Direct ALSA devices use the canonical api.alsa.card.id property;
        // ACP also supplies the older PulseAudio-compatible alsa.id spelling.
        card_id: copy_property(props, "api.alsa.card.id")
            .or_else(|| copy_property(props, "alsa.id")),
        policy_marker: match kind {
            AudioObjectKind::Device => None,
            AudioObjectKind::Node => copy_property(props, CASTKMS_AUDIO_SINK_PROPERTY),
        },
        device_bus_path: copy_property(props, "device.bus-path"),
        device_id: copy_property(props, "device.id"),
        output_index: copy_property(props, CASTKMS_AUDIO_OUTPUT_INDEX_PROPERTY),
        pcm_stream: copy_property(props, "api.alsa.pcm.stream"),
        node_name: copy_property(props, "node.name"),
        object_serial: copy_property(props, "object.serial"),
    }
}

fn pipewire_error(context: &str, error: impl std::fmt::Display) -> AudioSinkRuntimeError {
    AudioSinkRuntimeError::PipeWire(format!("{context}: {error}"))
}
