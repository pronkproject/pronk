use chromiacast::CastConnection;
use pronk_backend_protocol::{ControlKind, ControlOperation};

use super::DeviceControlError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CastControlCommand {
    SetVolume(i32),
    AdjustVolume(i32),
    SetMuted(bool),
    ToggleMute,
}

impl TryFrom<&ControlOperation> for CastControlCommand {
    type Error = DeviceControlError;

    fn try_from(operation: &ControlOperation) -> Result<Self, Self::Error> {
        match (operation.kind, operation.code.as_deref()) {
            (ControlKind::Volume, Some("absolute")) => Ok(Self::SetVolume(operation.value)),
            (ControlKind::Volume, Some("relative")) => Ok(Self::AdjustVolume(operation.value)),
            (ControlKind::Volume, _) => Err(DeviceControlError::UnsupportedControl(
                "unknown volume operation".into(),
            )),
            (ControlKind::Mute, Some("on")) => Ok(Self::SetMuted(true)),
            (ControlKind::Mute, Some("off")) => Ok(Self::SetMuted(false)),
            (ControlKind::Mute, Some("toggle")) => Ok(Self::ToggleMute),
            (ControlKind::Mute, _) => Err(DeviceControlError::UnsupportedControl(
                "unknown mute operation".into(),
            )),
            (kind, _) => Err(DeviceControlError::UnsupportedControl(format!(
                "{kind:?} has no proven Cast receiver operation"
            ))),
        }
    }
}

pub(super) async fn transmit(
    connection: &CastConnection,
    operation: &ControlOperation,
) -> Result<(), DeviceControlError> {
    match CastControlCommand::try_from(operation)? {
        CastControlCommand::SetVolume(value) => connection
            .set_volume_level(f64::from(value) / 100.0)
            .await
            .map(|_| ())
            .map_err(|error| DeviceControlError::Control(error.to_string())),
        CastControlCommand::AdjustVolume(value) => {
            let current = connection
                .status()
                .await
                .map_err(|error| DeviceControlError::Control(error.to_string()))?
                .volume_level()
                .ok_or_else(|| {
                    DeviceControlError::Control("receiver status omitted its volume level".into())
                })?;
            connection
                .set_volume_level((current + f64::from(value) / 100.0).clamp(0.0, 1.0))
                .await
                .map(|_| ())
                .map_err(|error| DeviceControlError::Control(error.to_string()))
        }
        CastControlCommand::SetMuted(muted) => connection
            .set_muted(muted)
            .await
            .map(|_| ())
            .map_err(|error| DeviceControlError::Control(error.to_string())),
        CastControlCommand::ToggleMute => {
            let muted = connection
                .status()
                .await
                .map_err(|error| DeviceControlError::Control(error.to_string()))?
                .is_muted()
                .ok_or_else(|| {
                    DeviceControlError::Control("receiver status omitted its mute state".into())
                })?;
            connection
                .set_muted(!muted)
                .await
                .map(|_| ())
                .map_err(|error| DeviceControlError::Control(error.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_control_mapping_preserves_volume_and_mute_intent() {
        for (kind, code, value, expected) in [
            (
                ControlKind::Volume,
                "absolute",
                75,
                CastControlCommand::SetVolume(75),
            ),
            (
                ControlKind::Volume,
                "relative",
                -10,
                CastControlCommand::AdjustVolume(-10),
            ),
            (
                ControlKind::Mute,
                "on",
                0,
                CastControlCommand::SetMuted(true),
            ),
            (
                ControlKind::Mute,
                "off",
                0,
                CastControlCommand::SetMuted(false),
            ),
            (
                ControlKind::Mute,
                "toggle",
                0,
                CastControlCommand::ToggleMute,
            ),
        ] {
            let operation = ControlOperation {
                session_generation: 1,
                kind,
                code: Some(code.into()),
                value,
            };
            assert_eq!(CastControlCommand::try_from(&operation).unwrap(), expected);
        }
    }
}
