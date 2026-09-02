//! Field-by-field codec for the one-shot grant-helper protocol.

use thiserror::Error;

use crate::grant::GrantProfile;

pub const MAGIC: [u8; 4] = *b"PRNK";
pub const PROTOCOL_MAJOR: u16 = 2;
pub const PROTOCOL_MINOR: u16 = 0;
pub const HEADER_LENGTH: usize = 32;
pub const MAX_MESSAGE_LENGTH: usize = 128;
pub const HELLO_LENGTH: usize = 64;
pub const CREATE_REQUEST_LENGTH: usize = 64;
pub const CREATE_RESULT_LENGTH: usize = 80;

pub const HELPER_FEATURE_ADMIN_CONTROL_FD: u32 = 1 << 0;
pub const PROFILE_DISPLAY_V1: u32 = 1 << 0;
pub const PROFILE_DISPLAY_CEC_V1: u32 = 1 << 1;
pub const PROFILE_DISPLAY_CEC_AUDIO_V1: u32 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    Hello = 1,
    CreateRequest = 2,
    CreateResult = 3,
}

impl TryFrom<u16> for MessageType {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::CreateRequest),
            3 => Ok(Self::CreateResult),
            _ => Err(ProtocolError::UnknownMessageType(value)),
        }
    }
}

impl TryFrom<u16> for GrantProfile {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::DisplayV1),
            2 => Ok(Self::DisplayCecV1),
            3 => Ok(Self::DisplayCecAudioV1),
            _ => Err(ProtocolError::UnknownGrantProfile(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DiagnosticStage {
    None = 0,
    Protocol = 1,
    Caller = 2,
    Device = 3,
    Connector = 4,
    CreateGrant = 5,
    VerifyGrant = 6,
    DropMaster = 7,
    SendResult = 8,
}

impl TryFrom<u32> for DiagnosticStage {
    type Error = ProtocolError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Protocol),
            2 => Ok(Self::Caller),
            3 => Ok(Self::Device),
            4 => Ok(Self::Connector),
            5 => Ok(Self::CreateGrant),
            6 => Ok(Self::VerifyGrant),
            7 => Ok(Self::DropMaster),
            8 => Ok(Self::SendResult),
            _ => Err(ProtocolError::UnknownDiagnosticStage(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub build: BuildVersion,
    pub pkexec_uid: u32,
    pub helper_pid: u32,
    pub parent_pid: u32,
    pub supported_profiles: u32,
    pub helper_features: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub expected_daemon_pid: u32,
    pub device_major: u32,
    pub device_minor: u32,
    pub connector_id: u32,
    pub profile: GrantProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateResult {
    pub status: i32,
    pub diagnostic_stage: DiagnosticStage,
    pub grant_id: u32,
    pub connector_id: u32,
    pub output_index: u32,
    pub actual_rights: u32,
    pub grant_flags: u32,
    pub initial_grant_state: u32,
    pub capture_uapi_major: u16,
    pub capture_uapi_minor: u16,
    pub helper_features: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    CreateRequest {
        request_id: u64,
        request: CreateRequest,
    },
    CreateResult {
        request_id: u64,
        result: CreateResult,
    },
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let (message_type, request_id, total_length) = match self {
            Self::Hello(_) => (MessageType::Hello, 0, HELLO_LENGTH),
            Self::CreateRequest { request_id, .. } => {
                require_nonzero_request_id(*request_id)?;
                (
                    MessageType::CreateRequest,
                    *request_id,
                    CREATE_REQUEST_LENGTH,
                )
            }
            Self::CreateResult { request_id, .. } => {
                require_nonzero_request_id(*request_id)?;
                (MessageType::CreateResult, *request_id, CREATE_RESULT_LENGTH)
            }
        };

        let mut bytes = vec![0_u8; total_length];
        bytes[0..4].copy_from_slice(&MAGIC);
        put_u16(&mut bytes, 4, PROTOCOL_MAJOR);
        put_u16(&mut bytes, 6, PROTOCOL_MINOR);
        put_u16(&mut bytes, 8, message_type as u16);
        put_u16(&mut bytes, 10, HEADER_LENGTH as u16);
        put_u32(&mut bytes, 12, total_length as u32);
        put_u64(&mut bytes, 16, request_id);

        match self {
            Self::Hello(hello) => encode_hello(&mut bytes, hello),
            Self::CreateRequest { request, .. } => encode_create_request(&mut bytes, request),
            Self::CreateResult { result, .. } => encode_create_result(&mut bytes, result)?,
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let header = decode_header(bytes)?;
        match header.message_type {
            MessageType::Hello => decode_hello(bytes, header.request_id),
            MessageType::CreateRequest => decode_create_request(bytes, header.request_id),
            MessageType::CreateResult => decode_create_result(bytes, header.request_id),
        }
    }

    pub fn validate_received_fd_count(&self, count: usize) -> Result<(), ProtocolError> {
        let expected = match self {
            Self::CreateResult { result, .. } if result.status == 0 => 2,
            _ => 0,
        };
        if count == expected {
            Ok(())
        } else {
            Err(ProtocolError::WrongFileDescriptorCount {
                expected,
                actual: count,
            })
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("message is shorter than the 32-byte header")]
    HeaderTooShort,
    #[error("message exceeds the 128-byte protocol limit")]
    MessageTooLong,
    #[error("message length is not eight-byte aligned")]
    MisalignedLength,
    #[error("invalid protocol magic")]
    InvalidMagic,
    #[error("unsupported protocol major {0}")]
    UnsupportedMajor(u16),
    #[error("unsupported protocol minor {0}")]
    UnsupportedMinor(u16),
    #[error("unknown message type {0}")]
    UnknownMessageType(u16),
    #[error("invalid header length {0}")]
    InvalidHeaderLength(u16),
    #[error("declared length {declared} differs from datagram length {actual}")]
    DeclaredLengthMismatch { declared: usize, actual: usize },
    #[error("{message_type:?} has length {actual}, expected {expected}")]
    WrongMessageLength {
        message_type: MessageType,
        expected: usize,
        actual: usize,
    },
    #[error("unknown header flags 0x{0:08x}")]
    UnknownHeaderFlags(u32),
    #[error("reserved field is nonzero")]
    NonzeroReserved,
    #[error("HELLO must use request ID zero")]
    HelloRequestIdNotZero,
    #[error("request and result messages require a nonzero request ID")]
    ZeroRequestId,
    #[error("unknown grant profile {0}")]
    UnknownGrantProfile(u16),
    #[error("unknown diagnostic stage {0}")]
    UnknownDiagnosticStage(u32),
    #[error("CREATE_RESULT status must be zero or a negative Linux errno")]
    PositiveResultStatus,
    #[error("a failed CREATE_RESULT contains grant metadata")]
    FailureContainsGrantMetadata,
    #[error("successful CREATE_RESULT must use diagnostic stage NONE")]
    SuccessContainsDiagnosticStage,
    #[error("message expected {expected} file descriptors but received {actual}")]
    WrongFileDescriptorCount { expected: usize, actual: usize },
}

#[derive(Debug, Clone, Copy)]
struct Header {
    message_type: MessageType,
    request_id: u64,
}

fn decode_header(bytes: &[u8]) -> Result<Header, ProtocolError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(ProtocolError::HeaderTooShort);
    }
    if bytes.len() > MAX_MESSAGE_LENGTH {
        return Err(ProtocolError::MessageTooLong);
    }
    if bytes.len() % 8 != 0 {
        return Err(ProtocolError::MisalignedLength);
    }
    if bytes[0..4] != MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let major = get_u16(bytes, 4);
    if major != PROTOCOL_MAJOR {
        return Err(ProtocolError::UnsupportedMajor(major));
    }
    let minor = get_u16(bytes, 6);
    if minor != PROTOCOL_MINOR {
        return Err(ProtocolError::UnsupportedMinor(minor));
    }
    let message_type = MessageType::try_from(get_u16(bytes, 8))?;
    let header_length = get_u16(bytes, 10);
    if usize::from(header_length) != HEADER_LENGTH {
        return Err(ProtocolError::InvalidHeaderLength(header_length));
    }
    let declared = get_u32(bytes, 12) as usize;
    if declared != bytes.len() {
        return Err(ProtocolError::DeclaredLengthMismatch {
            declared,
            actual: bytes.len(),
        });
    }
    let expected = expected_message_length(message_type);
    if bytes.len() != expected {
        return Err(ProtocolError::WrongMessageLength {
            message_type,
            expected,
            actual: bytes.len(),
        });
    }
    if get_u32(bytes, 24) != 0 {
        return Err(ProtocolError::UnknownHeaderFlags(get_u32(bytes, 24)));
    }
    if get_u32(bytes, 28) != 0 {
        return Err(ProtocolError::NonzeroReserved);
    }
    let request_id = get_u64(bytes, 16);
    match message_type {
        MessageType::Hello if request_id != 0 => return Err(ProtocolError::HelloRequestIdNotZero),
        MessageType::CreateRequest | MessageType::CreateResult if request_id == 0 => {
            return Err(ProtocolError::ZeroRequestId)
        }
        _ => {}
    }
    Ok(Header {
        message_type,
        request_id,
    })
}

const fn expected_message_length(message_type: MessageType) -> usize {
    match message_type {
        MessageType::Hello => HELLO_LENGTH,
        MessageType::CreateRequest => CREATE_REQUEST_LENGTH,
        MessageType::CreateResult => CREATE_RESULT_LENGTH,
    }
}

fn encode_hello(bytes: &mut [u8], hello: &Hello) {
    put_u32(bytes, 32, hello.build.major);
    put_u32(bytes, 36, hello.build.minor);
    put_u32(bytes, 40, hello.build.patch);
    put_u32(bytes, 44, hello.pkexec_uid);
    put_u32(bytes, 48, hello.helper_pid);
    put_u32(bytes, 52, hello.parent_pid);
    put_u32(bytes, 56, hello.supported_profiles);
    put_u32(bytes, 60, hello.helper_features);
}

fn decode_hello(bytes: &[u8], request_id: u64) -> Result<Message, ProtocolError> {
    debug_assert_eq!(request_id, 0);
    Ok(Message::Hello(Hello {
        build: BuildVersion {
            major: get_u32(bytes, 32),
            minor: get_u32(bytes, 36),
            patch: get_u32(bytes, 40),
        },
        pkexec_uid: get_u32(bytes, 44),
        helper_pid: get_u32(bytes, 48),
        parent_pid: get_u32(bytes, 52),
        supported_profiles: get_u32(bytes, 56),
        helper_features: get_u32(bytes, 60),
    }))
}

fn encode_create_request(bytes: &mut [u8], request: &CreateRequest) {
    put_u32(bytes, 32, request.expected_daemon_pid);
    put_u32(bytes, 36, request.device_major);
    put_u32(bytes, 40, request.device_minor);
    put_u32(bytes, 44, request.connector_id);
    put_u16(bytes, 48, request.profile as u16);
}

fn decode_create_request(bytes: &[u8], request_id: u64) -> Result<Message, ProtocolError> {
    if bytes[50..64].iter().any(|value| *value != 0) {
        return Err(ProtocolError::NonzeroReserved);
    }
    Ok(Message::CreateRequest {
        request_id,
        request: CreateRequest {
            expected_daemon_pid: get_u32(bytes, 32),
            device_major: get_u32(bytes, 36),
            device_minor: get_u32(bytes, 40),
            connector_id: get_u32(bytes, 44),
            profile: GrantProfile::try_from(get_u16(bytes, 48))?,
        },
    })
}

fn encode_create_result(bytes: &mut [u8], result: &CreateResult) -> Result<(), ProtocolError> {
    validate_create_result(result)?;
    put_i32(bytes, 32, result.status);
    put_u32(bytes, 36, result.diagnostic_stage as u32);
    put_u32(bytes, 40, result.grant_id);
    put_u32(bytes, 44, result.connector_id);
    put_u32(bytes, 48, result.output_index);
    put_u32(bytes, 52, result.actual_rights);
    put_u32(bytes, 56, result.grant_flags);
    put_u32(bytes, 60, result.initial_grant_state);
    put_u16(bytes, 64, result.capture_uapi_major);
    put_u16(bytes, 66, result.capture_uapi_minor);
    put_u32(bytes, 68, result.helper_features);
    Ok(())
}

fn decode_create_result(bytes: &[u8], request_id: u64) -> Result<Message, ProtocolError> {
    if bytes[72..80].iter().any(|value| *value != 0) {
        return Err(ProtocolError::NonzeroReserved);
    }
    let result = CreateResult {
        status: get_i32(bytes, 32),
        diagnostic_stage: DiagnosticStage::try_from(get_u32(bytes, 36))?,
        grant_id: get_u32(bytes, 40),
        connector_id: get_u32(bytes, 44),
        output_index: get_u32(bytes, 48),
        actual_rights: get_u32(bytes, 52),
        grant_flags: get_u32(bytes, 56),
        initial_grant_state: get_u32(bytes, 60),
        capture_uapi_major: get_u16(bytes, 64),
        capture_uapi_minor: get_u16(bytes, 66),
        helper_features: get_u32(bytes, 68),
    };
    validate_create_result(&result)?;
    Ok(Message::CreateResult { request_id, result })
}

fn validate_create_result(result: &CreateResult) -> Result<(), ProtocolError> {
    if result.status > 0 {
        return Err(ProtocolError::PositiveResultStatus);
    }
    if result.status == 0 && result.diagnostic_stage != DiagnosticStage::None {
        return Err(ProtocolError::SuccessContainsDiagnosticStage);
    }
    if result.status < 0
        && (result.grant_id != 0
            || result.connector_id != 0
            || result.output_index != 0
            || result.actual_rights != 0
            || result.grant_flags != 0
            || result.initial_grant_state != 0)
    {
        return Err(ProtocolError::FailureContainsGrantMetadata);
    }
    Ok(())
}

fn require_nonzero_request_id(request_id: u64) -> Result<(), ProtocolError> {
    if request_id == 0 {
        Err(ProtocolError::ZeroRequestId)
    } else {
        Ok(())
    }
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed field"))
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed field"))
}

fn get_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed field"))
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed field"))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use castkms_sys::{CAPTURE_UAPI_MAJOR, CAPTURE_UAPI_MINOR, GRANT_FLAG_ADMIN};

    use super::*;

    const REQUEST_ID: u64 = 0x0123_4567_89ab_cdef;

    fn hello() -> Message {
        Message::Hello(Hello {
            build: BuildVersion {
                major: 1,
                minor: 2,
                patch: 3,
            },
            pkexec_uid: 991,
            helper_pid: 2000,
            parent_pid: 1999,
            supported_profiles: PROFILE_DISPLAY_V1 | PROFILE_DISPLAY_CEC_V1,
            helper_features: HELPER_FEATURE_ADMIN_CONTROL_FD,
        })
    }

    fn request() -> Message {
        Message::CreateRequest {
            request_id: REQUEST_ID,
            request: CreateRequest {
                expected_daemon_pid: 4242,
                device_major: 226,
                device_minor: 9,
                connector_id: 37,
                profile: GrantProfile::DisplayCecV1,
            },
        }
    }

    fn result(status: i32) -> Message {
        Message::CreateResult {
            request_id: REQUEST_ID,
            result: if status == 0 {
                CreateResult {
                    status,
                    diagnostic_stage: DiagnosticStage::None,
                    grant_id: 19,
                    connector_id: 37,
                    output_index: 2,
                    actual_rights: GrantProfile::DisplayCecV1.rights(),
                    grant_flags: GRANT_FLAG_ADMIN,
                    initial_grant_state: 1,
                    capture_uapi_major: CAPTURE_UAPI_MAJOR,
                    capture_uapi_minor: CAPTURE_UAPI_MINOR,
                    helper_features: HELPER_FEATURE_ADMIN_CONTROL_FD,
                }
            } else {
                CreateResult {
                    status,
                    diagnostic_stage: DiagnosticStage::Device,
                    grant_id: 0,
                    connector_id: 0,
                    output_index: 0,
                    actual_rights: 0,
                    grant_flags: 0,
                    initial_grant_state: 0,
                    capture_uapi_major: 0,
                    capture_uapi_minor: 0,
                    helper_features: HELPER_FEATURE_ADMIN_CONTROL_FD,
                }
            },
        }
    }

    #[test]
    fn messages_round_trip_at_fixed_lengths() {
        for (message, expected_length) in [
            (hello(), HELLO_LENGTH),
            (request(), CREATE_REQUEST_LENGTH),
            (result(0), CREATE_RESULT_LENGTH),
            (result(-13), CREATE_RESULT_LENGTH),
        ] {
            let bytes = message.encode().unwrap();
            assert_eq!(bytes.len(), expected_length);
            assert_eq!(Message::decode(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn successful_results_require_holder_and_control_fds() {
        assert!(result(0).validate_received_fd_count(2).is_ok());
        for count in [0, 1, 3, 4] {
            assert!(matches!(
                result(0).validate_received_fd_count(count),
                Err(ProtocolError::WrongFileDescriptorCount {
                    expected: 2,
                    actual
                }) if actual == count
            ));
        }
        assert!(result(-13).validate_received_fd_count(0).is_ok());
    }

    #[test]
    fn reserved_bytes_and_unknown_versions_are_rejected() {
        let mut bytes = request().encode().unwrap();
        bytes[63] = 1;
        assert_eq!(Message::decode(&bytes), Err(ProtocolError::NonzeroReserved));

        let mut bytes = hello().encode().unwrap();
        put_u16(&mut bytes, 6, PROTOCOL_MINOR + 1);
        assert_eq!(
            Message::decode(&bytes),
            Err(ProtocolError::UnsupportedMinor(PROTOCOL_MINOR + 1))
        );
    }
}
