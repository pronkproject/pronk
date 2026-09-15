//! CastKMS source execution through portable renderer and native GPU layers.

mod activation;
mod output;
mod pool;
mod reader;
mod reads;
mod scene;
mod scene_job;
mod scene_pool;
mod scene_profile;
mod scene_reads;
mod scene_transaction;
mod source;
mod submission;

pub use activation::{
    activate_with_private_probe, PrivateProbe, ProbePreparationError, RendererActivationError,
};
pub use output::{
    CompletedOutput, CompletedReturn, FinishedOutput, OutputDestination, OutputPool, OutputReturn,
    OutputScope, PendingOutput, PublishedOutput, ReadyOutput,
};
pub use pool::{PrivateBuffer, PrivateFrame, PrivatePool, RejectedBuffer};
pub use reader::{
    RejectedSource, SourceAttempt, SourceAttemptError, SourceOpportunity, SourceReader,
    SourceReaderStartError,
};
pub use reads::{ReadCollectionError, SubmittedReads};
pub use scene::{
    ComposedFrame, RejectedScene, RejectedSceneFrames, SceneComposer, SceneCompositionError,
    SceneFrames, SceneInputs, SceneStorageProfile,
};
pub use scene_job::{QualifiedSceneJob, QualifySceneJobError};
pub use scene_pool::{
    RejectedSceneBuffers, RejectedSceneSources, SceneBuffers, ScenePool, MAX_SCENE_LAYERS,
};
pub use scene_transaction::{
    CancelSceneJobError, CompositableScene, PrepareSceneJobError, PreparedSceneJob,
    ReleaseSubmittedSceneError, ReleasedSceneJob, SceneWaitError, SubmitSceneJobError,
    SubmittedSceneJob,
};
pub use source::{ImportError, ImportedSource, SourceAlpha};
pub use submission::{
    PreparedSource, ReleasedSource, SourcePreparationError, SourceReleaseError,
    SourceSubmissionError, SubmittedSource,
};
