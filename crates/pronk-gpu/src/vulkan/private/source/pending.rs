//! Accepted private acquisition, with accounting separate from pixel access.

use std::io;

use pronk_dmabuf::SyncFile;

use super::PrivateImage;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::SourceImage;

/// Submitted reading into private storage, retaining source import and pixels.
///
/// The native completion record is available before waiting. It is neither
/// source authority nor permission to expose pixels. Successful wait destroys
/// the source import and returns the initialized private image. Failed execution
/// may retire access without producing valid pixels.
///
/// Drop may block on native retirement. Keep this owner on a blocking graphics
/// worker, including failure paths. Device loss permits teardown, not successful
/// pixels; unexplained native wait errors retain resources until process exit.
///
/// ```compile_fail
/// use pronk_gpu::vulkan::{Image, PendingPrivateRead};
/// fn publish_pending(read: PendingPrivateRead, output: Image) {
///     read.copy_into_waited(output);
/// }
/// ```
#[must_use = "wait for private pixels or deliberately retire the submitted read"]
pub struct PendingPrivateRead {
    job: Job<(SourceImage, PrivateImage)>,
    completion: Option<SyncFile>,
}

impl PendingPrivateRead {
    pub(super) fn new(job: Job<(SourceImage, PrivateImage)>, completion: Option<SyncFile>) -> Self {
        Self { job, completion }
    }

    /// Borrow the submitted read's native completion record without waiting.
    ///
    /// `None` means Vulkan returned its already-completed SYNC_FD sentinel, not
    /// that work is unsubmitted or that a future completion will be supplied.
    /// Such a read needs no pending-fence dependency. `Some` may itself already
    /// be complete; its descriptor may be duplicated for independent accounting.
    /// Either case still requires [`Self::wait`] before accessing private pixels.
    pub fn completion(&self) -> Option<&SyncFile> {
        self.completion.as_ref()
    }

    pub fn wait(self) -> io::Result<PrivateImage> {
        let Self { job, completion } = self;
        let (source, mut destination) = job.finish()?;
        if let Some(sync) = completion {
            require_success(sync.wait_blocking()?)?;
        }
        drop(source);
        destination.initialized = true;
        Ok(destination)
    }
}
