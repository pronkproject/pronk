use std::num::NonZeroU64;

/// Phase visible to the sender actor while it owns a transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SenderState {
    Configured,
    Streaming,
    Suspended,
    Failed,
}

/// Published lifecycle state shared by audio and video sender observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SenderStatus {
    Empty,
    Configured(NonZeroU64),
    Streaming(NonZeroU64),
    Suspended(NonZeroU64),
    Failed {
        generation: NonZeroU64,
        error: String,
    },
    Completed {
        generation: NonZeroU64,
        error: Option<String>,
    },
    Stopped {
        generation: Option<NonZeroU64>,
        error: Option<String>,
    },
}

impl SenderStatus {
    pub(crate) fn generation(&self) -> Option<NonZeroU64> {
        match self {
            Self::Empty => None,
            Self::Configured(generation)
            | Self::Streaming(generation)
            | Self::Suspended(generation)
            | Self::Failed { generation, .. }
            | Self::Completed { generation, .. } => Some(*generation),
            Self::Stopped { generation, .. } => *generation,
        }
    }

    pub(crate) fn active(generation: NonZeroU64, state: SenderState) -> Self {
        match state {
            SenderState::Configured => Self::Configured(generation),
            SenderState::Streaming => Self::Streaming(generation),
            SenderState::Suspended => Self::Suspended(generation),
            SenderState::Failed => unreachable!("failed sender needs an error"),
        }
    }
}
