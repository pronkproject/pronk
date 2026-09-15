//! One kernel scene job bound to its qualified native execution profile.

use std::io;
use std::os::fd::AsFd;

use castkms_renderer::{SceneJob, SourceReleaseError};
use pronk_gpu::vulkan::Device;

use crate::{SceneComposer, SceneFrames, SceneSource, SubmittedSceneReads};

/// A complete-scene job and the only composer qualified from its metadata.
#[must_use = "release the scene without access or submit its bound native reads"]
pub struct QualifiedSceneJob<'job, 'renderer, F: AsFd> {
    job: SceneJob<'job, 'renderer, F>,
    composer: SceneComposer,
}

impl<'job, 'renderer, F: AsFd> QualifiedSceneJob<'job, 'renderer, F> {
    pub fn new(
        device: &Device,
        job: SceneJob<'job, 'renderer, F>,
    ) -> Result<Self, QualifySceneJobError<SceneJob<'job, 'renderer, F>>> {
        match SceneComposer::from_scene_job(device, &job) {
            Ok(composer) => Ok(Self { job, composer }),
            Err(cause) => Err(QualifySceneJobError { job, cause }),
        }
    }

    pub fn job(&self) -> &SceneJob<'job, 'renderer, F> {
        &self.job
    }

    pub fn composer(&self) -> &SceneComposer {
        &self.composer
    }

    /// Import every layer under this job's nominal scene identity.
    pub fn import_sources(&self, device: &Device) -> io::Result<Vec<SceneSource>> {
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(self.job.layers().len())
            .map_err(io::Error::other)?;
        for layer in self.job.layers() {
            sources.push(SceneSource::import(
                self.composer.profile(),
                device,
                layer,
                self.job.producer_completion(),
            )?);
        }
        Ok(sources)
    }

    pub fn release_without_access(
        self,
    ) -> Result<(), SourceReleaseError<SceneJob<'job, 'renderer, F>>> {
        self.job.release_without_access()
    }

    /// Transfer the bound aggregate completion and close source admission.
    pub fn release_submitted(
        self,
        reads: SubmittedSceneReads,
    ) -> Result<ReleasedSceneReads, Box<ReleaseSceneJobError<'job, 'renderer, F>>> {
        if !reads.belongs_to(&self.composer) {
            return Err(Box::new(ReleaseSceneJobError {
                scene: self,
                reads,
                cause: invalid("submitted reads belong to another qualified scene"),
            }));
        }
        let content_serial = self.job.content_serial();
        let completion = reads.completion().map(AsFd::as_fd);
        let Self { job, composer } = self;
        match job.release_submitted(completion) {
            Ok(()) => Ok(ReleasedSceneReads {
                reads,
                content_serial,
            }),
            Err(error) => {
                let (job, cause) = error.into_parts();
                Err(Box::new(ReleaseSceneJobError {
                    scene: Self { job, composer },
                    reads,
                    cause,
                }))
            }
        }
    }
}

/// Qualification failure retaining the unreleased kernel job.
pub struct QualifySceneJobError<J> {
    job: J,
    cause: io::Error,
}

impl<J> QualifySceneJobError<J> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(self) -> (J, io::Error) {
        (self.job, self.cause)
    }
}

impl<J> std::fmt::Debug for QualifySceneJobError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QualifySceneJobError")
            .field("cause", &self.cause)
            .finish()
    }
}

impl<J> std::fmt::Display for QualifySceneJobError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "qualify complete scene job: {}", self.cause)
    }
}

impl<J> std::error::Error for QualifySceneJobError<J> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Failed aggregate release retaining both the kernel job and native reads.
pub struct ReleaseSceneJobError<'job, 'renderer, F: AsFd> {
    scene: QualifiedSceneJob<'job, 'renderer, F>,
    reads: SubmittedSceneReads,
    cause: io::Error,
}

impl<'job, 'renderer, F: AsFd> ReleaseSceneJobError<'job, 'renderer, F> {
    pub fn cause(&self) -> &io::Error {
        &self.cause
    }

    pub fn into_parts(
        self,
    ) -> (
        QualifiedSceneJob<'job, 'renderer, F>,
        SubmittedSceneReads,
        io::Error,
    ) {
        (self.scene, self.reads, self.cause)
    }
}

impl<F: AsFd> std::fmt::Debug for ReleaseSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReleaseSceneJobError")
            .field("cause", &self.cause)
            .finish()
    }
}

impl<F: AsFd> std::fmt::Display for ReleaseSceneJobError<'_, '_, F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "release complete scene job: {}", self.cause)
    }
}

impl<F: AsFd> std::error::Error for ReleaseSceneJobError<'_, '_, F> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Native reads accepted by CastKMS with no remaining source claim.
#[must_use = "wait for valid private pixels before composing the scene"]
pub struct ReleasedSceneReads {
    reads: SubmittedSceneReads,
    content_serial: std::num::NonZeroU64,
}

impl ReleasedSceneReads {
    pub fn wait(self) -> io::Result<SceneFrames> {
        self.reads.wait(self.content_serial)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
