//! Protocol-neutral control of one prepared network Device session.

use std::fmt;

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceControlKind {
    Activate,
    Deactivate,
    Power,
    Standby,
    KeyDown,
    KeyUp,
    Volume,
    Mute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceControlOperation {
    pub kind: DeviceControlKind,
    pub code: Option<String>,
    pub value: i32,
}

impl DeviceControlOperation {
    pub fn simple(kind: DeviceControlKind) -> Self {
        Self {
            kind,
            code: None,
            value: 0,
        }
    }

    pub fn coded(kind: DeviceControlKind, code: impl Into<String>) -> Self {
        Self {
            kind,
            code: Some(code.into()),
            value: 0,
        }
    }

    pub fn valued(kind: DeviceControlKind, code: impl Into<String>, value: i32) -> Self {
        Self {
            kind,
            code: Some(code.into()),
            value,
        }
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct DeviceControlError(String);

impl DeviceControlError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait DeviceControlPort: fmt::Debug + Send + Sync + 'static {
    async fn transmit_control(
        &self,
        operation: DeviceControlOperation,
    ) -> Result<(), DeviceControlError>;
}
