//! Ownership of accepted source reads, separate from their completion record.

use std::io;
use std::os::fd::AsFd;

use pronk_dmabuf::SyncFile;

use super::geometry::Transfer;
use crate::vulkan::image::ImageState;
use crate::vulkan::submission::{require_success, Job};
use crate::vulkan::{Image, SourceImage};

type Resources = (Vec<(SourceImage, Transfer)>, Image);

/// Accepted GPU reads and their exclusively owned private destination.
///
/// The completion record may be retained for source-use accounting before
/// native execution finishes. It grants no pixel access or capture authority.
/// Waiting consumes this owner; successful return destroys every source import
/// and makes the completed private image available for independent output work.
///
/// Drop retires accepted native work and may block. Keep this owner on a
/// blocking graphics worker, including error paths. Native device loss permits
/// teardown but never authorizes successful pixels; unexplained wait errors
/// retain native resources until process exit.
#[must_use = "wait for private pixels or deliberately retire the submitted work"]
pub struct PendingStage {
    job: Job<Resources>,
    completion: SyncFile,
}

impl PendingStage {
    pub(super) fn new(job: Job<Resources>, completion: SyncFile) -> Self {
        Self { job, completion }
    }

    /// Borrow the materialized native completion record without waiting.
    /// Duplicate its descriptor to retain it beyond the borrow; descriptor
    /// ownership does not retain this job or allow reuse before completion.
    pub fn completion(&self) -> &SyncFile {
        &self.completion
    }

    /// Wait for successful source reads and return their initialized private
    /// image. The completion record is returned for existing waited callers.
    pub fn wait(self) -> io::Result<(Image, SyncFile)> {
        let Self { job, completion } = self;
        let (sources, mut destination) = job.finish()?;
        drop(sources);
        require_success(
            SyncFile::from_fd(completion.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )?;
        destination.state = ImageState::Released;
        Ok((destination, completion))
    }
}
