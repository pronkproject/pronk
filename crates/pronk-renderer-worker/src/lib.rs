//! CastKMS source execution through portable renderer and native GPU layers.

mod activation;
mod output;
mod pool;
mod reader;
mod reads;
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
pub use source::{ImportError, ImportedSource};
pub use submission::{
    PreparedSource, ReleasedSource, SourcePreparationError, SourceReleaseError,
    SourceSubmissionError, SubmittedSource,
};
