//! Pure HDMI-CEC to Device-control translation.
//!
//! The bridge models a network display as a TV at logical address 0 and an
//! audio system at address 5. It contains no CastKMS, D-Bus, or backend types.

use crate::device_control_port::{DeviceControlKind, DeviceControlOperation};

const LOGICAL_ADDRESS_TV: u8 = 0;
const LOGICAL_ADDRESS_AUDIO_SYSTEM: u8 = 5;
const LOGICAL_ADDRESS_BROADCAST: u8 = 15;

const OPCODE_FEATURE_ABORT: u8 = 0x00;
const OPCODE_IMAGE_VIEW_ON: u8 = 0x04;
const OPCODE_TEXT_VIEW_ON: u8 = 0x0d;
const OPCODE_STANDBY: u8 = 0x36;
const OPCODE_USER_CONTROL_PRESSED: u8 = 0x44;
const OPCODE_USER_CONTROL_RELEASED: u8 = 0x45;
const OPCODE_SET_AUDIO_VOLUME_LEVEL: u8 = 0x73;
const OPCODE_ACTIVE_SOURCE: u8 = 0x82;
const OPCODE_INACTIVE_SOURCE: u8 = 0x9d;

const ABORT_UNRECOGNIZED_OPCODE: u8 = 0x00;
const ABORT_INVALID_OPERAND: u8 = 0x03;

const UI_POWER: u8 = 0x40;
const UI_VOLUME_UP: u8 = 0x41;
const UI_VOLUME_DOWN: u8 = 0x42;
const UI_MUTE: u8 = 0x43;
const UI_POWER_TOGGLE: u8 = 0x6b;
const UI_POWER_OFF: u8 = 0x6c;
const UI_POWER_ON: u8 = 0x6d;

const RELATIVE_VOLUME_STEP_PERCENT: i32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CecBridgeAction {
    Acknowledge,
    NotAcknowledged,
    Reply(Vec<u8>),
    Control(DeviceControlOperation),
}

#[derive(Debug, Default)]
pub struct CecBridge {
    pressed_key: Option<String>,
}

impl CecBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn translate(&mut self, message: &[u8]) -> CecBridgeAction {
        if message.is_empty() || message.len() > 16 {
            return CecBridgeAction::NotAcknowledged;
        }
        let initiator = message[0] >> 4;
        let destination = message[0] & 0x0f;

        if message.len() == 1 {
            return if initiator == destination && is_remote_address(destination) {
                CecBridgeAction::Acknowledge
            } else {
                CecBridgeAction::NotAcknowledged
            };
        }
        if !is_remote_address(destination) && destination != LOGICAL_ADDRESS_BROADCAST {
            return CecBridgeAction::NotAcknowledged;
        }

        let opcode = message[1];
        let result = match opcode {
            OPCODE_IMAGE_VIEW_ON | OPCODE_TEXT_VIEW_ON
                if destination == LOGICAL_ADDRESS_TV && message.len() == 2 =>
            {
                Some(DeviceControlOperation::simple(DeviceControlKind::Activate))
            }
            OPCODE_ACTIVE_SOURCE
                if destination == LOGICAL_ADDRESS_BROADCAST && message.len() == 4 =>
            {
                Some(DeviceControlOperation::simple(DeviceControlKind::Activate))
            }
            OPCODE_INACTIVE_SOURCE
                if destination == LOGICAL_ADDRESS_BROADCAST && message.len() == 4 =>
            {
                Some(DeviceControlOperation::simple(
                    DeviceControlKind::Deactivate,
                ))
            }
            OPCODE_STANDBY if message.len() == 2 => {
                Some(DeviceControlOperation::simple(DeviceControlKind::Standby))
            }
            OPCODE_USER_CONTROL_PRESSED if is_remote_address(destination) && message.len() == 3 => {
                return self.user_control_pressed(message[2]);
            }
            OPCODE_USER_CONTROL_RELEASED
                if is_remote_address(destination) && message.len() == 2 =>
            {
                return self.user_control_released();
            }
            OPCODE_SET_AUDIO_VOLUME_LEVEL
                if is_remote_address(destination) && message.len() == 3 && message[2] <= 100 =>
            {
                Some(DeviceControlOperation::valued(
                    DeviceControlKind::Volume,
                    "absolute",
                    i32::from(message[2]),
                ))
            }
            OPCODE_IMAGE_VIEW_ON
            | OPCODE_TEXT_VIEW_ON
            | OPCODE_ACTIVE_SOURCE
            | OPCODE_INACTIVE_SOURCE
            | OPCODE_STANDBY
            | OPCODE_USER_CONTROL_PRESSED
            | OPCODE_USER_CONTROL_RELEASED
            | OPCODE_SET_AUDIO_VOLUME_LEVEL => {
                return abort_or_acknowledge(initiator, destination, opcode, ABORT_INVALID_OPERAND);
            }
            _ => {
                return abort_or_acknowledge(
                    initiator,
                    destination,
                    opcode,
                    ABORT_UNRECOGNIZED_OPCODE,
                );
            }
        };

        CecBridgeAction::Control(result.expect("matched CEC control has an operation"))
    }

    fn user_control_pressed(&mut self, code: u8) -> CecBridgeAction {
        let operation = match code {
            UI_VOLUME_UP => DeviceControlOperation::valued(
                DeviceControlKind::Volume,
                "relative",
                RELATIVE_VOLUME_STEP_PERCENT,
            ),
            UI_VOLUME_DOWN => DeviceControlOperation::valued(
                DeviceControlKind::Volume,
                "relative",
                -RELATIVE_VOLUME_STEP_PERCENT,
            ),
            UI_MUTE => DeviceControlOperation::coded(DeviceControlKind::Mute, "toggle"),
            UI_POWER | UI_POWER_TOGGLE => {
                DeviceControlOperation::coded(DeviceControlKind::Power, "toggle")
            }
            UI_POWER_OFF => DeviceControlOperation::simple(DeviceControlKind::Standby),
            UI_POWER_ON => DeviceControlOperation::coded(DeviceControlKind::Power, "on"),
            code => {
                let code = format!("cec-ui-{code:02x}");
                self.pressed_key = Some(code.clone());
                DeviceControlOperation::coded(DeviceControlKind::KeyDown, code)
            }
        };
        CecBridgeAction::Control(operation)
    }

    fn user_control_released(&mut self) -> CecBridgeAction {
        match self.pressed_key.take() {
            Some(code) => CecBridgeAction::Control(DeviceControlOperation::coded(
                DeviceControlKind::KeyUp,
                code,
            )),
            None => CecBridgeAction::Acknowledge,
        }
    }
}

fn is_remote_address(address: u8) -> bool {
    matches!(address, LOGICAL_ADDRESS_TV | LOGICAL_ADDRESS_AUDIO_SYSTEM)
}

fn abort_or_acknowledge(initiator: u8, destination: u8, opcode: u8, reason: u8) -> CecBridgeAction {
    if destination == LOGICAL_ADDRESS_BROADCAST || initiator == LOGICAL_ADDRESS_BROADCAST {
        CecBridgeAction::Acknowledge
    } else {
        CecBridgeAction::Reply(vec![
            (destination << 4) | initiator,
            OPCODE_FEATURE_ABORT,
            opcode,
            reason,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation(action: CecBridgeAction) -> DeviceControlOperation {
        let CecBridgeAction::Control(operation) = action else {
            panic!("expected control operation, got {action:?}");
        };
        operation
    }

    #[test]
    fn polling_reports_only_the_emulated_remote_addresses() {
        let mut bridge = CecBridge::new();
        assert_eq!(bridge.translate(&[0x00]), CecBridgeAction::Acknowledge);
        assert_eq!(bridge.translate(&[0x55]), CecBridgeAction::Acknowledge);
        assert_eq!(bridge.translate(&[0x44]), CecBridgeAction::NotAcknowledged);
        assert_eq!(bridge.translate(&[0x04]), CecBridgeAction::NotAcknowledged);
    }

    #[test]
    fn routes_source_and_power_operations_without_protocol_types() {
        let mut bridge = CecBridge::new();
        for (message, kind) in [
            (
                &[0x40, OPCODE_IMAGE_VIEW_ON][..],
                DeviceControlKind::Activate,
            ),
            (
                &[0x4f, OPCODE_ACTIVE_SOURCE, 0x10, 0x00][..],
                DeviceControlKind::Activate,
            ),
            (
                &[0x4f, OPCODE_INACTIVE_SOURCE, 0x10, 0x00][..],
                DeviceControlKind::Deactivate,
            ),
            (&[0x4f, OPCODE_STANDBY][..], DeviceControlKind::Standby),
        ] {
            assert_eq!(operation(bridge.translate(message)).kind, kind);
        }
    }

    #[test]
    fn normalizes_relative_absolute_volume_mute_and_power() {
        let mut bridge = CecBridge::new();
        assert_eq!(
            operation(bridge.translate(&[0x45, OPCODE_USER_CONTROL_PRESSED, UI_VOLUME_UP])),
            DeviceControlOperation::valued(DeviceControlKind::Volume, "relative", 5)
        );
        assert_eq!(
            operation(bridge.translate(&[0x45, OPCODE_USER_CONTROL_PRESSED, UI_VOLUME_DOWN])),
            DeviceControlOperation::valued(DeviceControlKind::Volume, "relative", -5)
        );
        assert_eq!(
            operation(bridge.translate(&[0x45, OPCODE_USER_CONTROL_PRESSED, UI_MUTE])),
            DeviceControlOperation::coded(DeviceControlKind::Mute, "toggle")
        );
        assert_eq!(
            operation(bridge.translate(&[0x45, OPCODE_USER_CONTROL_PRESSED, UI_POWER_ON])),
            DeviceControlOperation::coded(DeviceControlKind::Power, "on")
        );
        assert_eq!(
            operation(bridge.translate(&[0x45, OPCODE_SET_AUDIO_VOLUME_LEVEL, 73])),
            DeviceControlOperation::valued(DeviceControlKind::Volume, "absolute", 73)
        );
    }

    #[test]
    fn remembers_the_generic_key_for_key_up() {
        let mut bridge = CecBridge::new();
        assert_eq!(
            operation(bridge.translate(&[0x40, OPCODE_USER_CONTROL_PRESSED, 0x44])),
            DeviceControlOperation::coded(DeviceControlKind::KeyDown, "cec-ui-44")
        );
        assert_eq!(
            operation(bridge.translate(&[0x40, OPCODE_USER_CONTROL_RELEASED])),
            DeviceControlOperation::coded(DeviceControlKind::KeyUp, "cec-ui-44")
        );
        assert_eq!(
            bridge.translate(&[0x40, OPCODE_USER_CONTROL_RELEASED]),
            CecBridgeAction::Acknowledge
        );
    }

    #[test]
    fn addressed_unknown_and_invalid_messages_get_feature_abort_replies() {
        let mut bridge = CecBridge::new();
        assert_eq!(
            bridge.translate(&[0x40, 0xee]),
            CecBridgeAction::Reply(vec![0x04, OPCODE_FEATURE_ABORT, 0xee, 0])
        );
        assert_eq!(
            bridge.translate(&[0x40, OPCODE_SET_AUDIO_VOLUME_LEVEL, 101]),
            CecBridgeAction::Reply(vec![
                0x04,
                OPCODE_FEATURE_ABORT,
                OPCODE_SET_AUDIO_VOLUME_LEVEL,
                ABORT_INVALID_OPERAND,
            ])
        );
        assert_eq!(
            bridge.translate(&[0x4f, 0xee]),
            CecBridgeAction::Acknowledge
        );
    }

    #[test]
    fn rejects_unrepresented_destinations_and_unbounded_messages() {
        let mut bridge = CecBridge::new();
        assert_eq!(
            bridge.translate(&[0x42, OPCODE_STANDBY]),
            CecBridgeAction::NotAcknowledged
        );
        assert_eq!(bridge.translate(&[]), CecBridgeAction::NotAcknowledged);
        assert_eq!(bridge.translate(&[0; 17]), CecBridgeAction::NotAcknowledged);
    }
}
