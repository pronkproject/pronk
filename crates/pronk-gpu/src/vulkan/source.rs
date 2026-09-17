//! Native source imports are distinct from executor-owned writable allocations.

use std::io;
use std::os::fd::{AsFd, OwnedFd};

use pronk_dmabuf::{Access, SyncFile};

use super::external::ExternalImage;
use super::image::ImageUse;
use super::{Device, ImageLayout};

/// One imported source use with its explicit producer dependency retained.
///
/// This type has no clear, output-publication or writable-image conversion API.
/// It does not represent a capture grant or revoke other copies of the source fd.
///
/// ```compile_fail
/// use pronk_gpu::vulkan::SourceImage;
/// fn overwrite(source: SourceImage) {
///     source.clear_and_wait([0, 0, 0]);
/// }
/// ```
pub struct SourceImage {
    pub(super) external: ExternalImage,
    pub(super) producer: Option<SyncFile>,
}

impl Device {
    /// Import a single-plane packed source through the ordinary Vulkan driver.
    ///
    /// Both descriptors are consumed on success and failure. The explicit
    /// producer dependency remains separate from a later reservation snapshot.
    /// Import performs no source reading and does not wait for the producer.
    /// Native usage requires reading the selected format and modifier, not
    /// writing it or exporting newly allocated storage in the same layout.
    ///
    /// # Safety
    ///
    /// The descriptor and metadata must identify a compatible image allocation
    /// on this physical GPU, satisfying Vulkan external-memory requirements for
    /// the reported format, dimensions and read-only transfer usage. The submitted
    /// fence must cover all producer writes and release in GENERAL layout with
    /// foreign queue ownership. Failed producer completion is allowed as input,
    /// but must not authorize reading invalid pixels. Native bounds checks
    /// cannot establish those cross-API facts. The caller must hold source-read
    /// authority and prevent pixel reuse throughout the eventual read operation.
    pub unsafe fn import_source(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
        producer: SyncFile,
    ) -> io::Result<SourceImage> {
        // SAFETY: The caller supplies the external-image contract documented
        // above; the producer record is retained by the imported source.
        unsafe { self.import_source_with_completion(fd, layout, Some(producer)) }
    }

    /// Import a source whose captured producer work has already completed.
    ///
    /// Reservation dependencies discovered at submission time remain mandatory.
    /// The operation only omits a separate captured producer wait.
    ///
    /// # Safety
    ///
    /// The descriptor and metadata have the external-image obligations of
    /// [`Self::import_source`]. In addition, every producer dependency that the
    /// source owner captured before issuing the descriptor must have completed
    /// successfully. A missing record alone does not establish that fact.
    pub unsafe fn import_ready_source(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
    ) -> io::Result<SourceImage> {
        // SAFETY: The caller supplies both documented external contracts.
        unsafe { self.import_source_with_completion(fd, layout, None) }
    }

    unsafe fn import_source_with_completion(
        &self,
        fd: OwnedFd,
        layout: ImageLayout,
        producer: Option<SyncFile>,
    ) -> io::Result<SourceImage> {
        // SAFETY: The caller supplies the source-specific cross-API contract
        // documented by the public import methods above.
        let external = unsafe {
            self.import_external_image(fd, layout, ImageUse::ImportedSource, Access::Read)
        }?;
        Ok(SourceImage { external, producer })
    }
}

impl SourceImage {
    pub fn layout(&self) -> ImageLayout {
        self.external.layout
    }

    /// Whether this import belongs to the supplied logical device instance.
    pub fn is_owned_by(&self, device: &Device) -> bool {
        std::sync::Arc::ptr_eq(&self.external.device, &device.inner)
    }

    pub(super) fn wait_for_producer(&self) -> io::Result<()> {
        let Some(producer) = &self.producer else {
            return Ok(());
        };
        super::submission::require_success(
            SyncFile::from_fd(producer.as_fd().try_clone_to_owned()?)?.wait_blocking()?,
        )
    }
}
