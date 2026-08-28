//! Application-owned boundary for one attached kernel display.
//!
//! The slot use case depends on this contract. CastKMS is one adapter; neither
//! its ioctl types nor its file-descriptor ownership leak into the slot actor.

use std::fmt;

use async_trait::async_trait;
use thiserror::Error;

use crate::display_state::{DisplayGrantState, DisplayTopology};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDisplayMetadata {
    pub grant_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelDisplayObservation {
    pub topology: DisplayTopology,
    pub grant_state: DisplayGrantState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelDisplayEvent {
    Changed(KernelDisplayObservation),
    Revoked,
    MediaFailed(String),
}

#[derive(Debug, Error)]
#[error("{operation}: {message}")]
pub struct KernelDisplayError {
    operation: &'static str,
    message: String,
}

impl KernelDisplayError {
    pub fn new(operation: &'static str, message: impl Into<String>) -> Self {
        Self {
            operation,
            message: message.into(),
        }
    }
}

/// Narrow kernel-facing port consumed by a cast-display slot actor.
#[async_trait]
pub trait KernelDisplayPort: fmt::Debug + Send + 'static {
    fn metadata(&self) -> KernelDisplayMetadata;
    fn initial_observation(&self) -> KernelDisplayObservation;
    async fn next_event(&mut self) -> Result<KernelDisplayEvent, KernelDisplayError>;
    async fn detach(self: Box<Self>) -> Result<(), KernelDisplayError>;
}
