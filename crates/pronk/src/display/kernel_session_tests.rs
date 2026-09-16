//! Exercise display attachment with an issuer that has no compositor connection.

use super::*;
use crate::capability_lease::CapabilityLease;
use crate::kernel_session::{KernelSessionControl, MonitorCapabilities};
use crate::renderer_session::{
    RendererAccess, RendererProvider, RendererSession, RendererSessionError,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use pronk_core::output::CastKmsOutput;

#[derive(Debug, Default)]
struct State {
    events: Mutex<Vec<&'static str>>,
    attached: AtomicBool,
    entered: tokio::sync::Notify,
    release_done: tokio::sync::Notify,
    gate: Option<(Mutex<bool>, Condvar)>,
    fail_attach: bool,
    panic_attach: bool,
    fail_detach: bool,
    omit_renderer: bool,
}

impl State {
    fn record(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }

    fn resume(&self) {
        let (ready, wake) = self.gate.as_ref().unwrap();
        *ready.lock().unwrap() = true;
        wake.notify_one();
    }
}

#[derive(Debug)]
struct Control {
    state: Arc<State>,
    lease: CapabilityLease,
}

#[async_trait::async_trait]
impl KernelSessionControl for Control {
    fn monitor_capabilities(&self) -> io::Result<MonitorCapabilities> {
        Ok(MonitorCapabilities { max_edid_size: 512 })
    }

    fn attach_monitor(&self, edid: Option<&[u8]>) -> io::Result<()> {
        assert_eq!(edid.unwrap().len(), 128);
        self.state.record("attach");
        self.state.entered.notify_one();
        assert!(!self.state.panic_attach, "injected monitor-control failure");
        if let Some((ready, wake)) = &self.state.gate {
            let (ready, _) = wake
                .wait_timeout_while(ready.lock().unwrap(), Duration::from_secs(5), |ready| {
                    !*ready
                })
                .unwrap();
            if !*ready {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "test attach gate did not open",
                ));
            }
        }
        if self.state.fail_attach {
            return Err(io::Error::other("attachment rejected"));
        }
        self.state.attached.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn detach_monitor(&self) -> io::Result<()> {
        self.state.record("detach");
        if self.state.fail_detach {
            return Err(io::Error::other("detachment rejected"));
        }
        self.state.attached.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn release(self: Box<Self>) -> Result<(), KernelSessionError> {
        self.lease
            .release()
            .await
            .map_err(|error| KernelSessionError::failed("release fake display", error))
    }
}

#[derive(Debug)]
struct IndependentProvider(Arc<State>);

#[async_trait::async_trait]
impl KernelSessionProvider for IndependentProvider {
    async fn acquire(
        &self,
        _: &CastKmsOutput,
        audio: bool,
        cancellation: CancellationToken,
    ) -> Result<KernelSession, KernelSessionError> {
        assert!(!audio);
        if cancellation.is_cancelled() {
            return Err(KernelSessionError::Cancelled);
        }
        let released = Arc::clone(&self.0);
        let control = Control {
            state: Arc::clone(&self.0),
            lease: CapabilityLease::new(async move {
                released.record("session release");
                released.attached.store(false, Ordering::SeqCst);
                released.release_done.notify_one();
                Ok(())
            }),
        };
        let renderer = if self.0.omit_renderer {
            None
        } else {
            let released = Arc::clone(&self.0);
            Some(RendererAccess::new(
                std::fs::File::open("/dev/null").unwrap().into(),
                CapabilityLease::new(async move {
                    released.record("renderer release");
                    Ok(())
                }),
                "/dev/dri/renderD128".into(),
                RendererSession::new(Arc::new(NoReplacement), None),
            ))
        };
        Ok(KernelSession::new(
            NonZeroU64::new(29).unwrap(),
            Box::new(control),
            drm_capture::Access::from_fd(std::fs::File::open("/dev/null").unwrap().into()),
            renderer,
        ))
    }
}

#[derive(Debug)]
struct NoReplacement;

#[async_trait::async_trait]
impl RendererProvider for NoReplacement {
    async fn acquire(&self, _: CancellationToken) -> Result<RendererAccess, RendererSessionError> {
        Err(RendererSessionError::Unavailable)
    }
}

async fn session(state: &Arc<State>) -> KernelSession {
    let output = CastKmsOutput {
        id: pronk_core::output::CastKmsOutputId {
            device_path: "test-device".into(),
            output_index: 0,
        },
        node_path: "/dev/dri/card9".into(),
        device_major: 226,
        device_minor: 9,
        crtc_id: 17,
        connector_id: 29,
        connector_name: "Virtual-1".into(),
        connection: pronk_core::output::OutputConnection::Disconnected,
    };
    let provider: Box<dyn KernelSessionProvider> = Box::new(IndependentProvider(Arc::clone(state)));
    provider
        .acquire(&output, false, CancellationToken::new())
        .await
        .unwrap()
}

async fn attach(
    session: KernelSession,
    cancellation: CancellationToken,
) -> Result<AttachedKernelSession, DisplaySetupError> {
    attach_with_source(session, CaptureSource::Renderer, cancellation).await
}

async fn attach_with_source(
    session: KernelSession,
    source: CaptureSource,
    cancellation: CancellationToken,
) -> Result<AttachedKernelSession, DisplaySetupError> {
    let mut bytes = vec![0; 128];
    bytes[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
    bytes[127] = 0u8.wrapping_sub(bytes.iter().copied().fold(0u8, u8::wrapping_add));
    attach_kernel_session(
        session,
        ValidatedEdid::new(bytes).unwrap(),
        NonZeroU32::new(17).unwrap(),
        vec![EdidMode::new(1280, 720, 60_000).unwrap()],
        source,
        &cancellation,
    )
    .await
}

async fn notified(notify: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(2), notify.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn independent_provider_uses_the_production_attachment_lifecycle() {
    let state = Arc::new(State::default());
    let attached = attach(session(&state).await, CancellationToken::new())
        .await
        .unwrap();
    assert!(state.attached.load(Ordering::SeqCst));
    assert_eq!(attached.kernel.metadata().session_id.get(), 29);
    cleanup_attached_kernel_session(attached).await;
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "renderer release", "detach", "session release"]
    );
}

#[tokio::test]
async fn cancellation_before_attachment_does_not_change_the_monitor() {
    let state = Arc::new(State::default());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        attach(session(&state).await, cancellation).await,
        Err(DisplaySetupError::Cancelled)
    ));
    notified(&state.release_done).await;
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["renderer release", "session release"]
    );
}

#[tokio::test]
async fn cancellation_retires_a_monitor_that_finishes_attaching_later() {
    let state = Arc::new(State {
        gate: Some((Mutex::new(false), Condvar::new())),
        ..State::default()
    });
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(attach(session(&state).await, cancellation.clone()));
    notified(&state.entered).await;
    cancellation.cancel();
    state.resume();
    assert!(matches!(
        task.await.unwrap(),
        Err(DisplaySetupError::Cancelled)
    ));
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "detach", "renderer release", "session release"]
    );
}

#[tokio::test]
async fn failed_attachment_still_releases_issued_authority() {
    let state = Arc::new(State {
        fail_attach: true,
        ..State::default()
    });
    assert!(matches!(
        attach(session(&state).await, CancellationToken::new()).await,
        Err(DisplaySetupError::KernelAttach(_))
    ));
    notified(&state.release_done).await;
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "session release")
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_detachment_still_releases_the_session() {
    let state = Arc::new(State {
        fail_detach: true,
        ..State::default()
    });
    let attached = attach(session(&state).await, CancellationToken::new())
        .await
        .unwrap();
    attached.media.release().await.unwrap();
    assert!(Box::new(attached.kernel).detach().await.is_err());
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "renderer release", "detach", "session release"]
    );
}

#[tokio::test]
async fn missing_media_authority_retires_the_attached_monitor() {
    let state = Arc::new(State {
        omit_renderer: true,
        ..State::default()
    });
    assert!(matches!(
        attach(session(&state).await, CancellationToken::new()).await,
        Err(DisplaySetupError::KernelAccess(_))
    ));
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "detach", "session release"]
    );
}

#[tokio::test]
async fn final_image_capture_attaches_without_renderer_authority() {
    let state = Arc::new(State {
        omit_renderer: true,
        ..State::default()
    });
    let attached = attach_with_source(
        session(&state).await,
        CaptureSource::FinalImage,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(attached.media, DisplayMediaAccess::FinalImage(_)));
    assert!(state.attached.load(Ordering::SeqCst));
    cleanup_attached_kernel_session(attached).await;
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "detach", "session release"]
    );
}

#[tokio::test]
async fn final_image_capture_does_not_take_the_issued_renderer() {
    let state = Arc::new(State::default());
    let attached = attach_with_source(
        session(&state).await,
        CaptureSource::FinalImage,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    cleanup_attached_kernel_session(attached).await;
    assert_eq!(
        state.events.lock().unwrap().as_slice(),
        &["attach", "detach", "renderer release", "session release"]
    );
}

#[tokio::test]
async fn invalid_observation_is_rejected_before_attachment() {
    let state = Arc::new(State::default());
    let result = KernelDisplay::attach(
        session(&state).await,
        None,
        KernelDisplayConfig {
            crtc_id: NonZeroU32::new(17).unwrap(),
            modes: Vec::new(),
            poll_interval: DEFAULT_TOPOLOGY_POLL_INTERVAL,
        },
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(result, Err(AttachError::Configuration(_))));
    notified(&state.release_done).await;
    assert!(!state.events.lock().unwrap().contains(&"attach"));
}

#[tokio::test]
async fn abandoned_attachment_releases_authority_after_the_worker_finishes() {
    let state = Arc::new(State {
        gate: Some((Mutex::new(false), Condvar::new())),
        ..State::default()
    });
    let task = tokio::spawn(attach(session(&state).await, CancellationToken::new()));
    notified(&state.entered).await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    state.resume();
    notified(&state.release_done).await;
    assert!(!state.attached.load(Ordering::SeqCst));
    assert_eq!(
        state
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "session release")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_panicking_attachment_worker_still_releases_its_session() {
    let state = Arc::new(State {
        panic_attach: true,
        ..State::default()
    });
    assert!(matches!(
        attach(session(&state).await, CancellationToken::new()).await,
        Err(DisplaySetupError::KernelAttach(AttachError::Worker(_)))
    ));
    notified(&state.release_done).await;
    assert!(!state.attached.load(Ordering::SeqCst));
}

#[test]
fn cancellation_while_attachment_is_queued_skips_monitor_io() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        ready.await.unwrap();
        let state = Arc::new(State::default());
        let cancellation = CancellationToken::new();
        let mut task = Box::pin(attach(session(&state).await, cancellation.clone()));
        assert!(futures_util::poll!(&mut task).is_pending());
        cancellation.cancel();
        resume.send(()).unwrap();
        blocker.await.unwrap();
        assert!(matches!(task.await, Err(DisplaySetupError::Cancelled)));
        assert_eq!(
            state.events.lock().unwrap().as_slice(),
            &["renderer release", "session release"]
        );
    });
}
