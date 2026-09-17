//! Blocking setup with one retained namespace for the issued capture file.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use drm_capture::Access;
use pronk_capture::allocation::Heap;
use pronk_capture::{Actor, Buffer, Config as ActorConfig, Layout, Session};
use pronk_pipewire::{MAX_VIDEO_BUFFERS, MIN_VIDEO_BUFFERS};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{require_route_layout, DrmCapturePipelineConfig};
use crate::media_pipeline_port::MediaPipelineError;
use crate::media_session::MediaStartRequest;

pub(crate) type CaptureOwner = Arc<drm_capture::Client>;

#[derive(Debug, Clone)]
pub(crate) struct SetupConfig {
    pub pool_size: NonZeroU32,
    pub request_capacity: NonZeroU32,
    pub pool_byte_limit: NonZeroU64,
    pub heap_path: PathBuf,
    pub poll_interval: Duration,
    pub shutdown_timeout: Duration,
}

impl From<&DrmCapturePipelineConfig> for SetupConfig {
    fn from(config: &DrmCapturePipelineConfig) -> Self {
        Self {
            pool_size: config.pool_size,
            request_capacity: config.request_capacity,
            pool_byte_limit: config.pool_byte_limit,
            heap_path: config.heap_path.clone(),
            poll_interval: config.poll_interval,
            shutdown_timeout: config.shutdown_timeout,
        }
    }
}

impl SetupConfig {
    pub(crate) fn validate(&self) -> Result<(), MediaPipelineError> {
        if !(MIN_VIDEO_BUFFERS..=MAX_VIDEO_BUFFERS).contains(&(self.pool_size.get() as usize))
            || self.request_capacity > self.pool_size
            || self.poll_interval.is_zero()
            || self.shutdown_timeout.is_zero()
        {
            return Err(MediaPipelineError::new(
                "invalid capture pool, queue, or timing configuration",
            ));
        }
        Ok(())
    }
}

pub(crate) struct Setup(Arc<Mutex<State>>);

struct State {
    access: Access,
    session: Option<Session>,
}

impl Setup {
    pub(crate) fn new(access: Access) -> Self {
        Self(Arc::new(Mutex::new(State {
            access,
            session: None,
        })))
    }

    pub(crate) async fn create_actor(
        &self,
        config: SetupConfig,
        request: MediaStartRequest,
        expected_offer: Option<drm_capture::OfferId>,
        cancellation: CancellationToken,
    ) -> Result<(Actor<CaptureOwner>, Layout), MediaPipelineError> {
        on_worker(Arc::clone(&self.0), cancellation, move |state, cancel| {
            state.create_actor(config, request, expected_offer, cancel)
        })
        .await
    }

    pub(crate) async fn create_actor_with_buffers(
        &self,
        config: SetupConfig,
        request: MediaStartRequest,
        expected_offer: drm_capture::OfferId,
        buffers: Vec<Buffer>,
        cancellation: CancellationToken,
    ) -> Result<(Actor<CaptureOwner>, Layout), MediaPipelineError> {
        on_worker(Arc::clone(&self.0), cancellation, move |state, cancel| {
            state.create_actor_from_buffers(config, request, Some(expected_offer), buffers, cancel)
        })
        .await
    }

    pub(crate) async fn describe(
        &self,
        cancellation: CancellationToken,
    ) -> Result<drm_capture::Description, MediaPipelineError> {
        on_worker(Arc::clone(&self.0), cancellation, |state, _| {
            state.describe().map_err(|error| {
                MediaPipelineError::new(format!("describe capture output: {error}"))
            })
        })
        .await
    }
}

impl State {
    fn describe(&self) -> std::io::Result<drm_capture::Description> {
        match &self.session {
            Some(session) => session.describe(),
            None => self.access.describe(),
        }
    }

    fn create_actor(
        &mut self,
        config: SetupConfig,
        request: MediaStartRequest,
        expected_offer: Option<drm_capture::OfferId>,
        cancellation: &CancellationToken,
    ) -> Result<(Actor<CaptureOwner>, Layout), MediaPipelineError> {
        let offer = self.describe().map_err(|error| {
            MediaPipelineError::new(format!("describe capture output: {error}"))
        })?;
        if expected_offer.is_some_and(|expected| expected != offer.offer) {
            return Err(MediaPipelineError::new(
                "capture offer changed while the generation was starting",
            ));
        }
        let layout = Layout {
            width: offer.width,
            height: offer.height,
        };
        require_route_layout(layout, request)?;
        check_cancellation(cancellation)?;
        let buffers = Heap::open(&config.heap_path)
            .and_then(|heap| heap.allocate(layout, config.pool_size, config.pool_byte_limit))
            .map_err(|error| MediaPipelineError::new(format!("allocate capture pool: {error}")))?;
        self.spawn_actor(config, request, expected_offer, buffers, cancellation)
    }

    fn create_actor_from_buffers(
        &mut self,
        config: SetupConfig,
        request: MediaStartRequest,
        expected_offer: Option<drm_capture::OfferId>,
        buffers: Vec<Buffer>,
        cancellation: &CancellationToken,
    ) -> Result<(Actor<CaptureOwner>, Layout), MediaPipelineError> {
        if buffers.len() != config.pool_size.get() as usize {
            return Err(MediaPipelineError::new(
                "capture buffer count does not match pool policy",
            ));
        }
        self.spawn_actor(config, request, expected_offer, buffers, cancellation)
    }

    fn spawn_actor(
        &mut self,
        config: SetupConfig,
        request: MediaStartRequest,
        expected_offer: Option<drm_capture::OfferId>,
        buffers: Vec<Buffer>,
        cancellation: &CancellationToken,
    ) -> Result<(Actor<CaptureOwner>, Layout), MediaPipelineError> {
        if self.session.is_none() {
            let client = self.access.open().map_err(|error| {
                MediaPipelineError::new(format!("open capture session: {error}"))
            })?;
            self.session = Some(Session::new(client));
        }
        let session = self
            .session
            .as_mut()
            .expect("capture file retains its namespace");
        check_cancellation(cancellation)?;
        let actor_config = ActorConfig {
            capacity: config.request_capacity,
            poll_interval: config.poll_interval,
            shutdown_timeout: config.shutdown_timeout,
        };
        let actor = match expected_offer {
            Some(offer) => session.spawn_for_offer(buffers, actor_config, offer),
            None => session.spawn(buffers, actor_config),
        }
        .map_err(|error| MediaPipelineError::new(format!("start capture actor: {error}")))?;
        let layout = actor.layout();
        require_route_layout(layout, request).map_err(|_| {
            MediaPipelineError::new("capture layout changed while the generation was starting")
        })?;
        Ok((actor, layout))
    }
}

async fn on_worker<S: Send + 'static, R: Send + 'static>(
    state: Arc<Mutex<S>>,
    cancellation: CancellationToken,
    prepare: impl FnOnce(&mut S, &CancellationToken) -> Result<R, MediaPipelineError> + Send + 'static,
) -> Result<R, MediaPipelineError> {
    check_cancellation(&cancellation)?;
    // Wait asynchronously for earlier setup. An abandoned allocator must not
    // occupy another blocking worker for every retry of the same display.
    let mut state = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return Err(MediaPipelineError::new("capture start was cancelled"));
        }
        state = state.lock_owned() => state,
    };
    tokio::task::spawn_blocking(move || {
        // Only generation setup uses this mutex. Streaming, returning buffers,
        // and dropping the pipeline never wait for allocation to finish.
        check_cancellation(&cancellation)?;
        prepare(&mut state, &cancellation)
    })
    .await
    .map_err(|error| MediaPipelineError::new(format!("capture setup worker failed: {error}")))?
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), MediaPipelineError> {
    if cancellation.is_cancelled() {
        Err(MediaPipelineError::new("capture start was cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn config() -> SetupConfig {
        SetupConfig {
            pool_size: NonZeroU32::new(4).unwrap(),
            request_capacity: NonZeroU32::new(3).unwrap(),
            pool_byte_limit: NonZeroU64::new(128 * 1024 * 1024).unwrap(),
            heap_path: "/dev/dma_heap/system".into(),
            poll_interval: Duration::from_millis(2),
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn shared_setup_policy_rejects_an_oversubscribed_request_queue() {
        let mut config = config();
        config.request_capacity = NonZeroU32::new(5).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn shared_setup_policy_rejects_zero_timing() {
        let mut config = config();
        config.poll_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn blocking_setup_does_not_occupy_the_async_thread() {
        let async_thread = std::thread::current().id();
        let state = Arc::new(Mutex::new(0));
        let (started, entered) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(on_worker(
            Arc::clone(&state),
            CancellationToken::new(),
            move |value, _| {
                assert_ne!(std::thread::current().id(), async_thread);
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                *value += 1;
                Ok(())
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), entered)
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(*state.lock().await, 1);
    }

    #[tokio::test]
    async fn abandoning_setup_preserves_changes_to_the_shared_namespace() {
        let state = Arc::new(Mutex::new(0));
        let (started, entered) = oneshot::channel();
        let (finished, completed) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(on_worker(
            Arc::clone(&state),
            CancellationToken::new(),
            move |value, _| {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                *value += 1;
                finished.send(()).unwrap();
                Ok(())
            },
        ));
        entered.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        completed.await.unwrap();
        let next = on_worker(state, CancellationToken::new(), |value, _| {
            *value += 1;
            Ok(*value)
        })
        .await
        .unwrap();
        assert_eq!(next, 2);
    }

    #[tokio::test]
    async fn a_retry_can_be_cancelled_while_earlier_setup_owns_the_namespace() {
        let state = Arc::new(Mutex::new(0));
        let previous = state.lock().await;
        let cancel = CancellationToken::new();
        let mut retry = Box::pin(on_worker(Arc::clone(&state), cancel.clone(), |value, _| {
            *value += 1;
            Ok(())
        }));
        std::future::poll_fn(|context| {
            assert!(retry.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(1), retry)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert_eq!(*previous, 0);
    }

    #[test]
    fn cancelled_queued_setup_does_not_touch_the_capture_file() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started, entered) = oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            entered.await.unwrap();
            let state = Arc::new(Mutex::new(0));
            let cancel = CancellationToken::new();
            let mut work = Box::pin(on_worker(Arc::clone(&state), cancel.clone(), |value, _| {
                *value += 1;
                Ok(())
            }));
            std::future::poll_fn(|context| {
                assert!(work.as_mut().poll(context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            cancel.cancel();
            release.send(()).unwrap();
            blocker.await.unwrap();
            assert!(work.await.unwrap_err().to_string().contains("cancelled"));
            assert_eq!(*state.lock().await, 0);
        });
    }
}
