//! CastKMS source execution through portable renderer and native GPU layers.

mod activation;
mod pool;
mod reads;
mod recipient;
mod scene;
mod scene_image;
mod scene_job;
mod scene_pool;
mod scene_profile;
mod scene_reader;
mod scene_reads;
mod scene_transaction;
mod source;

pub use activation::{PrivateProbe, ProbePreparationError};
pub use pool::{PrivateBuffer, PrivateFrame, PrivatePool, RejectedBuffer};
pub use reads::{ReadCollectionError, SubmittedReads};
pub use recipient::{DeliveryAttempt, DeliveryError};
pub use scene::{
    ComposedFrame, RejectedScene, RejectedSceneFrames, SceneComposer, SceneCompositionError,
    SceneFrames, SceneInputs, SceneStorageProfile,
};
pub use scene_image::{PreparedSceneImages, RegisteredSceneImages, RenderedFrame};
pub use scene_job::{QualifiedSceneJob, QualifySceneJobError};
pub use scene_pool::{
    RejectedComposedFrame, RejectedSceneBuffers, RejectedSceneSources, SceneBuffers, ScenePool,
    MAX_SCENE_LAYERS,
};
pub use scene_profile::PrimarySceneProfile;
pub use scene_reader::{SceneAttempt, SceneAttemptError, SceneReader, SceneReaderStartError};
pub use scene_transaction::SceneCompletionError;
pub use source::SourceAlpha;
