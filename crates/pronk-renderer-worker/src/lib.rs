//! CastKMS source execution through portable renderer and native GPU layers.

mod activation;
mod output;
mod pool;
mod reader;
mod source;
mod submission;

pub use activation::{PrivateProbe, ProbePreparationError};
pub use output::{
    CompletedOutput, CompletedReturn, FinishedOutput, OutputDestination, OutputPool, OutputReturn,
    OutputScope, PendingOutput, PublishedOutput, ReadyOutput,
};
pub use pool::{PrivateBuffer, PrivateFrame, PrivatePool, RejectedBuffer};
pub use reader::{RejectedSource, SourceOpportunity, SourceReader, SourceReaderStartError};
pub use source::{ImportError, ImportedSource};
pub use submission::{
    PreparedSource, ReleasedSource, SourcePreparationError, SourceReleaseError,
    SourceSubmissionError, SubmittedSource,
};
