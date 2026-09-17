//! One kernel scene job bound to its qualified native execution profile.

use std::io;
use std::os::fd::AsFd;

use crate::scene_reads::SceneSource;
use crate::{SceneComposer, SceneStorageProfile};
use castkms_renderer::{SceneJob, SourceReleaseError};

/// A complete-scene job and the only composer qualified from its metadata.
#[must_use = "release the scene without access or submit its bound native reads"]
pub struct QualifiedSceneJob<'job, F: AsFd> {
    job: SceneJob<'job, F>,
    composer: SceneComposer,
}

impl<'job, F: AsFd> QualifiedSceneJob<'job, F> {
    pub fn new(
        storage: &SceneStorageProfile,
        job: SceneJob<'job, F>,
    ) -> Result<Self, QualifySceneJobError<SceneJob<'job, F>>> {
        match SceneComposer::from_scene_job(storage, &job) {
            Ok(composer) => Ok(Self { job, composer }),
            Err(cause) => Err(QualifySceneJobError { job, cause }),
        }
    }

    pub(crate) fn composer(&self) -> &SceneComposer {
        &self.composer
    }

    pub(crate) fn into_parts(self) -> (SceneJob<'job, F>, SceneComposer) {
        (self.job, self.composer)
    }

    /// Import every layer under this job's nominal scene identity.
    pub(crate) fn import_sources(&self) -> io::Result<Vec<SceneSource>> {
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(self.job.layers().len())
            .map_err(io::Error::other)?;
        for layer in self.job.layers() {
            sources.push(SceneSource::import(
                self.composer.profile(),
                self.composer.device(),
                layer,
                self.job.producer_completion(),
            )?);
        }
        Ok(sources)
    }

    pub fn release_without_access(self) -> Result<(), SourceReleaseError<SceneJob<'job, F>>> {
        self.job.release_without_access()
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
