//! Abort a startup task if its owner never takes over the handle.

use tokio::task::JoinHandle;

pub(crate) struct AbortOnDropTask(Option<JoinHandle<()>>);

impl AbortOnDropTask {
    pub(crate) fn new(task: JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    pub(crate) fn take(&mut self) -> JoinHandle<()> {
        self.0.take().expect("startup task already taken")
    }

    pub(crate) async fn abort(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
