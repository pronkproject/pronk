//! Bounded decoding and Tokio delivery of events from a CastKMS grant holder.

use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;

use castkms_sys::{
    DrmEvent, DrmEventCastkmsCaptureFrame, DrmEventCastkmsCecTx, DrmEventCastkmsGrantRevoked,
    DrmEventCastkmsGrantState, CAPTURE_EVENT_FRAME, CAPTURE_EVENT_GRANT_REVOKED,
    CAPTURE_EVENT_GRANT_STATE, CEC_EVENT_TX,
};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use thiserror::Error;
use tokio::io::unix::AsyncFd;

use super::{CaptureProtocolError, CastKmsClient, GrantState};

pub const DRM_EVENT_HEADER_SIZE: usize = size_of::<DrmEvent>();
pub const DRM_EVENT_READ_SIZE: usize = 4096;
pub const MAX_DRM_EVENT_SIZE: usize = 4096;
pub const MAX_EVENTS_PER_READINESS: usize = 1024;

const GRANT_REVOKED_EVENT_SIZE: usize = size_of::<DrmEventCastkmsGrantRevoked>();
const GRANT_STATE_EVENT_SIZE: usize = size_of::<DrmEventCastkmsGrantState>();
const CAPTURE_FRAME_EVENT_SIZE: usize = size_of::<DrmEventCastkmsCaptureFrame>();
const CEC_TRANSMIT_EVENT_SIZE: usize = size_of::<DrmEventCastkmsCecTx>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantRevokedEvent {
    pub grant_id: u32,
    pub status: i32,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantStateEvent {
    pub grant_id: u32,
    pub state: GrantState,
    pub status: i32,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFrameEvent {
    pub user_data: u64,
    pub sequence: u64,
    pub timestamp_ns: i64,
    pub mode_generation: u64,
    pub stream_id: u32,
    pub buffer_id: u32,
    pub status: i32,
    pub flags: u32,
    pub dropped_frames: u32,
    pub damage_x: i32,
    pub damage_y: i32,
    pub damage_width: u32,
    pub damage_height: u32,
    pub cursor_serial: u32,
    pub cursor_flags: u32,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_hotspot_x: u32,
    pub cursor_hotspot_y: u32,
    pub cursor_width: u32,
    pub cursor_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CecTransmitEvent {
    pub transport_id: u32,
    pub transport_generation: u64,
    pub state_generation: u64,
    pub cookie: u64,
    pub connector_id: u32,
    pub output_index: u32,
    pub attempts: u8,
    pub signal_free_time: u32,
    pub(super) message_length: u8,
    pub(super) message: [u8; 16],
}

impl CecTransmitEvent {
    pub fn message(&self) -> &[u8] {
        &self.message[..usize::from(self.message_length)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownEvent {
    pub event_type: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKmsEvent {
    CaptureFrame(CaptureFrameEvent),
    CecTransmit(CecTransmitEvent),
    GrantState(GrantStateEvent),
    GrantRevoked(GrantRevokedEvent),
    Unknown(UnknownEvent),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EventDecodeError {
    #[error("DRM event input chunk is {actual} bytes; maximum is {maximum}")]
    InputTooLarge { actual: usize, maximum: usize },
    #[error("DRM event type 0x{event_type:08x} declares invalid length {length}")]
    LengthTooSmall { event_type: u32, length: u32 },
    #[error("DRM event type 0x{event_type:08x} declares length {length}; maximum is {maximum}")]
    LengthTooLarge {
        event_type: u32,
        length: u32,
        maximum: usize,
    },
    #[error(
        "DRM event type 0x{event_type:08x} is {actual} bytes; its known prefix is {minimum} bytes"
    )]
    KnownEventTooShort {
        event_type: u32,
        actual: u32,
        minimum: usize,
    },
    #[error("DRM event type 0x{event_type:08x} has nonzero reserved field 0x{value:08x}")]
    NonZeroReserved { event_type: u32, value: u32 },
    #[error("grant-state event contains unknown state {0}")]
    UnknownGrantState(u32),
    #[error("CastKMS CEC transmit event contains invalid {0}")]
    InvalidCecTransmit(&'static str),
    #[error(
        "DRM event stream ended with {buffered} buffered bytes{expected_suffix}",
        expected_suffix = expected.map_or_else(String::new, |length| format!(" of a {length}-byte event"))
    )]
    Truncated {
        buffered: usize,
        expected: Option<usize>,
    },
}

/// Incremental decoder for the byte stream returned by `read(2)` on a DRM fd.
///
/// The carry allocation can never exceed `MAX_DRM_EVENT_SIZE`. Callers feed at
/// most `DRM_EVENT_READ_SIZE` bytes at once; known records accept trailing
/// extension bytes, while unknown records are represented without retaining
/// their payload.
#[derive(Debug)]
pub struct EventDecoder {
    carry: Vec<u8>,
}

impl Default for EventDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventDecoder {
    pub fn new() -> Self {
        Self {
            carry: Vec::with_capacity(MAX_DRM_EVENT_SIZE),
        }
    }

    pub fn buffered_len(&self) -> usize {
        self.carry.len()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<CastKmsEvent>, EventDecodeError> {
        if bytes.len() > DRM_EVENT_READ_SIZE {
            return Err(EventDecodeError::InputTooLarge {
                actual: bytes.len(),
                maximum: DRM_EVENT_READ_SIZE,
            });
        }

        let mut input = bytes;
        let mut events = Vec::new();

        while !input.is_empty() {
            if self.carry.len() < DRM_EVENT_HEADER_SIZE {
                let count = (DRM_EVENT_HEADER_SIZE - self.carry.len()).min(input.len());
                self.carry.extend_from_slice(&input[..count]);
                input = &input[count..];
                if self.carry.len() < DRM_EVENT_HEADER_SIZE {
                    break;
                }
            }

            let event_type = read_u32(&self.carry, 0);
            let declared_length = read_u32(&self.carry, 4);
            let event_length = validate_declared_length(event_type, declared_length)?;

            let count = (event_length - self.carry.len()).min(input.len());
            self.carry.extend_from_slice(&input[..count]);
            input = &input[count..];
            if self.carry.len() < event_length {
                break;
            }

            let event = decode_event(&self.carry, event_type, declared_length)?;
            self.carry.clear();
            events.push(event);
        }

        Ok(events)
    }

    pub fn finish(&self) -> Result<(), EventDecodeError> {
        if self.carry.is_empty() {
            return Ok(());
        }

        let expected =
            (self.carry.len() >= DRM_EVENT_HEADER_SIZE).then(|| read_u32(&self.carry, 4) as usize);
        Err(EventDecodeError::Truncated {
            buffered: self.carry.len(),
            expected,
        })
    }
}

#[derive(Debug, Error)]
pub enum EventReadError {
    #[error("wait for CastKMS DRM events: {0}")]
    Wait(#[source] io::Error),
    #[error("read CastKMS DRM events: {0}")]
    Read(#[source] io::Error),
    #[error(transparent)]
    Decode(#[from] EventDecodeError),
    #[error("CastKMS DRM event stream closed")]
    StreamClosed,
    #[error("CastKMS event references grant {actual}; holder owns grant {expected}")]
    ForeignGrant { expected: u32, actual: u32 },
    #[error("CastKMS event references connector {actual}; holder owns connector {expected}")]
    ForeignConnector { expected: u32, actual: u32 },
    #[error("more than {maximum} DRM events arrived in one readiness drain")]
    TooManyEvents { maximum: usize },
    #[error(transparent)]
    CaptureProtocol(#[from] CaptureProtocolError),
}

/// Tokio registration for the same client and sole holder used by all ioctls.
///
/// Construction must occur inside a Tokio runtime with I/O enabled. A read or
/// decode error is terminal for this event stream; the owning actor should
/// discard this object and perform normal session cleanup.
#[derive(Debug)]
pub struct AsyncCastKmsClient {
    io: AsyncFd<CastKmsClient>,
    decoder: EventDecoder,
}

impl AsyncCastKmsClient {
    pub(super) fn new(client: CastKmsClient) -> io::Result<Self> {
        let status_flags = fcntl(client.as_raw_fd(), FcntlArg::F_GETFL).map_err(io::Error::from)?;
        if !OFlag::from_bits_truncate(status_flags).contains(OFlag::O_NONBLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CastKMS holder must have O_NONBLOCK set before Tokio registration",
            ));
        }

        Ok(Self {
            io: AsyncFd::new(client)?,
            decoder: EventDecoder::new(),
        })
    }

    pub fn client(&self) -> &CastKmsClient {
        self.io.get_ref()
    }

    pub fn client_mut(&mut self) -> &mut CastKmsClient {
        self.io.get_mut()
    }

    /// Unregister readiness and recover the sole grant-holder client.
    ///
    /// Owning actors use this for ordered capture/attachment teardown. Any
    /// partially decoded event bytes are deliberately discarded because the
    /// event stream is no longer being observed.
    pub fn into_client(self) -> CastKmsClient {
        self.io.into_inner()
    }

    /// Wait for readability and drain the holder until `EAGAIN`.
    ///
    /// A partial final record remains in the bounded decoder for the next
    /// readiness cycle. The returned batch is never empty.
    pub async fn read_events(&mut self) -> Result<Vec<CastKmsEvent>, EventReadError> {
        let grant_id = self.client().grant_id();
        let connector_id = self.client().connector_id();
        let events =
            read_event_batch(&self.io, &mut self.decoder, Some((grant_id, connector_id))).await?;
        for event in &events {
            if let CastKmsEvent::CaptureFrame(frame) = event {
                self.io.get_mut().record_capture_frame(*frame)?;
            }
        }
        Ok(events)
    }
}

async fn read_event_batch<T: AsRawFd>(
    io: &AsyncFd<T>,
    decoder: &mut EventDecoder,
    expected_scope: Option<(u32, u32)>,
) -> Result<Vec<CastKmsEvent>, EventReadError> {
    let mut events = Vec::new();
    let mut read_buffer = [0_u8; DRM_EVENT_READ_SIZE];

    loop {
        let mut readiness = io.readable().await.map_err(EventReadError::Wait)?;

        loop {
            let result = readiness.try_io(|registered| {
                read_nonblocking(registered.get_ref().as_raw_fd(), &mut read_buffer)
            });

            match result {
                Ok(Ok(0)) => {
                    decoder.finish()?;
                    return Err(EventReadError::StreamClosed);
                }
                Ok(Ok(count)) => {
                    let decoded = decoder.push(&read_buffer[..count])?;
                    if events.len() + decoded.len() > MAX_EVENTS_PER_READINESS {
                        return Err(EventReadError::TooManyEvents {
                            maximum: MAX_EVENTS_PER_READINESS,
                        });
                    }
                    for event in decoded {
                        if let Some((grant_id, connector_id)) = expected_scope {
                            validate_event_scope(grant_id, connector_id, &event)?;
                        }
                        events.push(event);
                    }
                }
                Ok(Err(error)) => return Err(EventReadError::Read(error)),
                Err(_would_block) => break,
            }
        }

        drop(readiness);
        if !events.is_empty() {
            return Ok(events);
        }
    }
}

fn read_nonblocking(fd: std::os::fd::RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match nix::unistd::read(fd, buffer) {
            Ok(count) => return Ok(count),
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

fn validate_event_scope(
    expected_grant: u32,
    expected_connector: u32,
    event: &CastKmsEvent,
) -> Result<(), EventReadError> {
    let actual_grant = match event {
        CastKmsEvent::GrantState(event) => Some(event.grant_id),
        CastKmsEvent::GrantRevoked(event) => Some(event.grant_id),
        CastKmsEvent::CaptureFrame(_) | CastKmsEvent::CecTransmit(_) | CastKmsEvent::Unknown(_) => {
            None
        }
    };

    if let Some(actual) = actual_grant {
        if actual != expected_grant {
            return Err(EventReadError::ForeignGrant {
                expected: expected_grant,
                actual,
            });
        }
    }
    if let CastKmsEvent::CecTransmit(event) = event {
        if event.connector_id != expected_connector {
            return Err(EventReadError::ForeignConnector {
                expected: expected_connector,
                actual: event.connector_id,
            });
        }
    }
    Ok(())
}

fn validate_declared_length(
    event_type: u32,
    declared_length: u32,
) -> Result<usize, EventDecodeError> {
    let length = declared_length as usize;
    if length < DRM_EVENT_HEADER_SIZE {
        return Err(EventDecodeError::LengthTooSmall {
            event_type,
            length: declared_length,
        });
    }
    if length > MAX_DRM_EVENT_SIZE {
        return Err(EventDecodeError::LengthTooLarge {
            event_type,
            length: declared_length,
            maximum: MAX_DRM_EVENT_SIZE,
        });
    }
    Ok(length)
}

fn decode_event(
    bytes: &[u8],
    event_type: u32,
    declared_length: u32,
) -> Result<CastKmsEvent, EventDecodeError> {
    match event_type {
        CAPTURE_EVENT_GRANT_REVOKED => {
            require_known_prefix(event_type, declared_length, GRANT_REVOKED_EVENT_SIZE)?;
            Ok(CastKmsEvent::GrantRevoked(GrantRevokedEvent {
                grant_id: read_u32(bytes, 8),
                status: read_i32(bytes, 12),
                timestamp_ns: read_u64(bytes, 16),
            }))
        }
        CAPTURE_EVENT_GRANT_STATE => {
            require_known_prefix(event_type, declared_length, GRANT_STATE_EVENT_SIZE)?;
            let raw_state = read_u32(bytes, 12);
            let state = GrantState::try_from(raw_state)
                .map_err(|_| EventDecodeError::UnknownGrantState(raw_state))?;
            let reserved = read_u32(bytes, 20);
            require_zero_reserved(event_type, reserved)?;
            Ok(CastKmsEvent::GrantState(GrantStateEvent {
                grant_id: read_u32(bytes, 8),
                state,
                status: read_i32(bytes, 16),
                timestamp_ns: read_u64(bytes, 24),
            }))
        }
        CAPTURE_EVENT_FRAME => {
            require_known_prefix(event_type, declared_length, CAPTURE_FRAME_EVENT_SIZE)?;
            let reserved = read_u32(bytes, 108);
            require_zero_reserved(event_type, reserved)?;
            Ok(CastKmsEvent::CaptureFrame(CaptureFrameEvent {
                user_data: read_u64(bytes, 8),
                sequence: read_u64(bytes, 16),
                timestamp_ns: read_i64(bytes, 24),
                mode_generation: read_u64(bytes, 32),
                stream_id: read_u32(bytes, 40),
                buffer_id: read_u32(bytes, 44),
                status: read_i32(bytes, 48),
                flags: read_u32(bytes, 52),
                dropped_frames: read_u32(bytes, 56),
                damage_x: read_i32(bytes, 60),
                damage_y: read_i32(bytes, 64),
                damage_width: read_u32(bytes, 68),
                damage_height: read_u32(bytes, 72),
                cursor_serial: read_u32(bytes, 76),
                cursor_flags: read_u32(bytes, 80),
                cursor_x: read_i32(bytes, 84),
                cursor_y: read_i32(bytes, 88),
                cursor_hotspot_x: read_u32(bytes, 92),
                cursor_hotspot_y: read_u32(bytes, 96),
                cursor_width: read_u32(bytes, 100),
                cursor_height: read_u32(bytes, 104),
            }))
        }
        CEC_EVENT_TX => {
            require_known_prefix(event_type, declared_length, CEC_TRANSMIT_EVENT_SIZE)?;
            require_zero_reserved(event_type, read_u32(bytes, 12))?;
            require_zero_reserved(event_type, u32::from(read_u16(bytes, 66)))?;
            let transport_id = read_u32(bytes, 8);
            let transport_generation = read_u64(bytes, 16);
            let state_generation = read_u64(bytes, 24);
            let cookie = read_u64(bytes, 32);
            let connector_id = read_u32(bytes, 40);
            let attempts = bytes[48];
            let message_length = bytes[49];
            for (valid, field) in [
                (transport_id != 0, "transport ID"),
                (transport_generation != 0, "transport generation"),
                (state_generation != 0, "state generation"),
                (cookie != 0, "transaction cookie"),
                (connector_id != 0, "connector ID"),
                (attempts != 0, "attempt count"),
                ((1..=16).contains(&message_length), "message length"),
            ] {
                if !valid {
                    return Err(EventDecodeError::InvalidCecTransmit(field));
                }
            }
            let mut message = [0_u8; 16];
            message.copy_from_slice(&bytes[50..66]);
            Ok(CastKmsEvent::CecTransmit(CecTransmitEvent {
                transport_id,
                transport_generation,
                state_generation,
                cookie,
                connector_id,
                output_index: read_u32(bytes, 44),
                attempts,
                signal_free_time: read_u32(bytes, 68),
                message_length,
                message,
            }))
        }
        _ => Ok(CastKmsEvent::Unknown(UnknownEvent {
            event_type,
            length: declared_length,
        })),
    }
}

fn require_known_prefix(
    event_type: u32,
    actual: u32,
    minimum: usize,
) -> Result<(), EventDecodeError> {
    if actual as usize >= minimum {
        Ok(())
    } else {
        Err(EventDecodeError::KnownEventTooShort {
            event_type,
            actual,
            minimum,
        })
    }
}

fn require_zero_reserved(event_type: u32, value: u32) -> Result<(), EventDecodeError> {
    if value == 0 {
        Ok(())
    } else {
        Err(EventDecodeError::NonZeroReserved { event_type, value })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated prefix"),
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated prefix"),
    )
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated prefix"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated prefix"),
    )
}

fn read_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated prefix"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use castkms_sys::{
        CAPTURE_FRAME_FULL_DAMAGE, CAPTURE_FRAME_MODE_CHANGED, CURSOR_IMAGE_CHANGED, CURSOR_VISIBLE,
    };
    use tokio::runtime::Builder;
    use tokio::sync::oneshot;

    const GRANT_ID: u32 = 17;
    const CONNECTOR_ID: u32 = 29;

    #[test]
    fn decodes_multiple_known_and_unknown_events_in_one_read() {
        let revoked = revoked_event();
        let state = state_event(GrantState::SuspendedOtherMaster);
        let frame = frame_event();
        let unknown = unknown_event(0x8000_00fe, 19);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&revoked);
        bytes.extend_from_slice(&state);
        bytes.extend_from_slice(&unknown);
        bytes.extend_from_slice(&frame);

        let events = EventDecoder::new().push(&bytes).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0],
            CastKmsEvent::GrantRevoked(GrantRevokedEvent {
                grant_id: GRANT_ID,
                status: -128,
                timestamp_ns: 9_001,
            })
        );
        assert_eq!(
            events[1],
            CastKmsEvent::GrantState(GrantStateEvent {
                grant_id: GRANT_ID,
                state: GrantState::SuspendedOtherMaster,
                status: -13,
                timestamp_ns: 9_002,
            })
        );
        assert_eq!(
            events[2],
            CastKmsEvent::Unknown(UnknownEvent {
                event_type: 0x8000_00fe,
                length: 19,
            })
        );
        assert_eq!(events[3], CastKmsEvent::CaptureFrame(expected_frame()));
    }

    #[test]
    fn accepts_every_two_chunk_split_of_each_known_event() {
        for bytes in [
            revoked_event(),
            state_event(GrantState::Active),
            frame_event(),
            cec_transmit_event(),
        ] {
            for split in 0..=bytes.len() {
                let mut decoder = EventDecoder::new();
                let mut decoded = decoder.push(&bytes[..split]).unwrap();
                decoded.extend(decoder.push(&bytes[split..]).unwrap());
                assert_eq!(decoded.len(), 1, "split at {split} of {}", bytes.len());
                assert_eq!(decoder.buffered_len(), 0);
                decoder.finish().unwrap();
            }
        }
    }

    #[test]
    fn accepts_a_stream_one_byte_at_a_time() {
        let bytes = frame_event();
        let mut decoder = EventDecoder::new();
        let mut decoded = Vec::new();
        for byte in bytes {
            decoded.extend(decoder.push(&[byte]).unwrap());
            assert!(decoder.buffered_len() <= MAX_DRM_EVENT_SIZE);
        }
        assert_eq!(decoded, [CastKmsEvent::CaptureFrame(expected_frame())]);
    }

    #[test]
    fn decodes_a_bounded_cec_transmit_event() {
        let event = EventDecoder::new()
            .push(&cec_transmit_event())
            .unwrap()
            .pop()
            .unwrap();
        let CastKmsEvent::CecTransmit(event) = event else {
            panic!("expected CEC transmit event");
        };
        assert_eq!(event.transport_id, 7);
        assert_eq!(event.transport_generation, 11);
        assert_eq!(event.state_generation, 13);
        assert_eq!(event.cookie, 17);
        assert_eq!(event.connector_id, CONNECTOR_ID);
        assert_eq!(event.output_index, 3);
        assert_eq!(event.attempts, 2);
        assert_eq!(event.signal_free_time, 1);
        assert_eq!(event.message(), &[0x04, 0x44, 0x41]);
    }

    #[test]
    fn skips_unknown_payload_without_losing_the_next_event() {
        let mut bytes = unknown_event(0x8765_4321, 257);
        bytes.extend_from_slice(&revoked_event());
        let events = EventDecoder::new().push(&bytes).unwrap();
        assert!(matches!(events[0], CastKmsEvent::Unknown(_)));
        assert!(matches!(events[1], CastKmsEvent::GrantRevoked(_)));
    }

    #[test]
    fn accepts_trailing_extensions_on_known_events() {
        let mut extended = state_event(GrantState::Active);
        extended.resize(GRANT_STATE_EVENT_SIZE + 11, 0xa5);
        let extended_length = extended.len() as u32;
        put_u32(&mut extended, 4, extended_length);
        extended.extend_from_slice(&revoked_event());

        let events = EventDecoder::new().push(&extended).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], CastKmsEvent::GrantState(_)));
        assert!(matches!(events[1], CastKmsEvent::GrantRevoked(_)));
    }

    #[test]
    fn accepts_an_unknown_event_at_the_maximum_size() {
        let bytes = unknown_event(0x8000_ffff, MAX_DRM_EVENT_SIZE);
        let mut decoder = EventDecoder::new();
        let events = decoder.push(&bytes).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn rejects_small_zero_and_oversized_lengths() {
        for length in [0, 1, (DRM_EVENT_HEADER_SIZE - 1) as u32] {
            let bytes = header(0x8000_00aa, length);
            assert!(matches!(
                EventDecoder::new().push(&bytes),
                Err(EventDecodeError::LengthTooSmall { length: actual, .. }) if actual == length
            ));
        }

        let bytes = header(0x8000_00aa, (MAX_DRM_EVENT_SIZE + 1) as u32);
        assert!(matches!(
            EventDecoder::new().push(&bytes),
            Err(EventDecodeError::LengthTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_an_oversized_input_chunk_without_buffering_it() {
        let bytes = vec![0; DRM_EVENT_READ_SIZE + 1];
        let mut decoder = EventDecoder::new();
        assert!(matches!(
            decoder.push(&bytes),
            Err(EventDecodeError::InputTooLarge { .. })
        ));
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn rejects_short_known_records() {
        for (event_type, length) in [
            (CAPTURE_EVENT_GRANT_REVOKED, GRANT_REVOKED_EVENT_SIZE - 1),
            (CAPTURE_EVENT_GRANT_STATE, GRANT_STATE_EVENT_SIZE - 1),
            (CAPTURE_EVENT_FRAME, CAPTURE_FRAME_EVENT_SIZE - 1),
            (CEC_EVENT_TX, CEC_TRANSMIT_EVENT_SIZE - 1),
        ] {
            let mut bytes = vec![0; length];
            put_u32(&mut bytes, 0, event_type);
            put_u32(&mut bytes, 4, length as u32);
            assert!(matches!(
                EventDecoder::new().push(&bytes),
                Err(EventDecodeError::KnownEventTooShort { .. })
            ));
        }
    }

    #[test]
    fn rejects_nonzero_reserved_fields() {
        let mut state = state_event(GrantState::Active);
        put_u32(&mut state, 20, 1);
        assert!(matches!(
            EventDecoder::new().push(&state),
            Err(EventDecodeError::NonZeroReserved { .. })
        ));

        let mut frame = frame_event();
        put_u32(&mut frame, 108, 1);
        assert!(matches!(
            EventDecoder::new().push(&frame),
            Err(EventDecodeError::NonZeroReserved { .. })
        ));

        let mut cec = cec_transmit_event();
        put_u32(&mut cec, 12, 1);
        assert!(matches!(
            EventDecoder::new().push(&cec),
            Err(EventDecodeError::NonZeroReserved { .. })
        ));
    }

    #[test]
    fn rejects_invalid_cec_transmit_identity_and_length() {
        for (offset, width) in [(8, 4), (16, 8), (24, 8), (32, 8), (40, 4)] {
            let mut event = cec_transmit_event();
            event[offset..offset + width].fill(0);
            assert!(matches!(
                EventDecoder::new().push(&event),
                Err(EventDecodeError::InvalidCecTransmit(_))
            ));
        }
        for length in [0, 17] {
            let mut event = cec_transmit_event();
            event[49] = length;
            assert_eq!(
                EventDecoder::new().push(&event).unwrap_err(),
                EventDecodeError::InvalidCecTransmit("message length")
            );
        }
    }

    #[test]
    fn rejects_an_unknown_grant_state() {
        let mut state = state_event(GrantState::Active);
        put_u32(&mut state, 12, 99);
        assert_eq!(
            EventDecoder::new().push(&state).unwrap_err(),
            EventDecodeError::UnknownGrantState(99)
        );
    }

    #[test]
    fn reports_truncated_header_and_body_at_end_of_stream() {
        let mut decoder = EventDecoder::new();
        decoder.push(&[1, 2, 3]).unwrap();
        assert_eq!(
            decoder.finish().unwrap_err(),
            EventDecodeError::Truncated {
                buffered: 3,
                expected: None,
            }
        );

        let bytes = state_event(GrantState::Active);
        let mut decoder = EventDecoder::new();
        decoder.push(&bytes[..17]).unwrap();
        assert_eq!(
            decoder.finish().unwrap_err(),
            EventDecodeError::Truncated {
                buffered: 17,
                expected: Some(GRANT_STATE_EVENT_SIZE),
            }
        );
    }

    #[test]
    fn tokio_reader_drains_and_preserves_a_partial_record() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (reader, mut writer) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            writer.set_nonblocking(false).unwrap();
            let reader = AsyncFd::new(reader).unwrap();

            let mut bytes = state_event(GrantState::Active);
            bytes.extend_from_slice(&revoked_event());
            let split = DRM_EVENT_HEADER_SIZE + 3;
            let (release_tx, release_rx) = oneshot::channel();
            let writer_task = tokio::task::spawn_blocking(move || {
                writer.write_all(&bytes[..split]).unwrap();
                std::thread::sleep(Duration::from_millis(10));
                writer.write_all(&bytes[split..]).unwrap();
                let _ = release_rx.blocking_recv();
            });

            let mut decoder = EventDecoder::new();
            let events = read_event_batch(&reader, &mut decoder, Some((GRANT_ID, CONNECTOR_ID)))
                .await
                .unwrap();
            assert_eq!(events.len(), 2);
            assert!(matches!(events[0], CastKmsEvent::GrantState(_)));
            assert!(matches!(events[1], CastKmsEvent::GrantRevoked(_)));
            assert_eq!(decoder.buffered_len(), 0);

            release_tx.send(()).unwrap();
            writer_task.await.unwrap();
        });
    }

    #[test]
    fn rejects_a_grant_event_for_another_holder() {
        let event = CastKmsEvent::GrantRevoked(GrantRevokedEvent {
            grant_id: GRANT_ID + 1,
            status: -128,
            timestamp_ns: 1,
        });
        assert!(matches!(
            validate_event_scope(GRANT_ID, CONNECTOR_ID, &event),
            Err(EventReadError::ForeignGrant {
                expected: GRANT_ID,
                actual
            }) if actual == GRANT_ID + 1
        ));
    }

    #[test]
    fn rejects_a_cec_event_for_another_connector() {
        let CastKmsEvent::CecTransmit(mut event) = EventDecoder::new()
            .push(&cec_transmit_event())
            .unwrap()
            .pop()
            .unwrap()
        else {
            panic!("expected CEC transmit event");
        };
        event.connector_id += 1;
        assert!(matches!(
            validate_event_scope(GRANT_ID, CONNECTOR_ID, &CastKmsEvent::CecTransmit(event)),
            Err(EventReadError::ForeignConnector {
                expected: CONNECTOR_ID,
                actual
            }) if actual == CONNECTOR_ID + 1
        ));
    }

    fn revoked_event() -> Vec<u8> {
        let mut bytes = vec![0; GRANT_REVOKED_EVENT_SIZE];
        put_u32(&mut bytes, 0, CAPTURE_EVENT_GRANT_REVOKED);
        put_u32(&mut bytes, 4, GRANT_REVOKED_EVENT_SIZE as u32);
        put_u32(&mut bytes, 8, GRANT_ID);
        put_i32(&mut bytes, 12, -128);
        put_u64(&mut bytes, 16, 9_001);
        bytes
    }

    fn state_event(state: GrantState) -> Vec<u8> {
        let raw_state = match state {
            GrantState::Pending => 0,
            GrantState::Active => 1,
            GrantState::SuspendedNoMaster => 2,
            GrantState::SuspendedOtherMaster => 3,
            GrantState::SuspendedForeignContent => 4,
            GrantState::Revoked => 5,
        };
        let mut bytes = vec![0; GRANT_STATE_EVENT_SIZE];
        put_u32(&mut bytes, 0, CAPTURE_EVENT_GRANT_STATE);
        put_u32(&mut bytes, 4, GRANT_STATE_EVENT_SIZE as u32);
        put_u32(&mut bytes, 8, GRANT_ID);
        put_u32(&mut bytes, 12, raw_state);
        put_i32(&mut bytes, 16, -13);
        put_u64(&mut bytes, 24, 9_002);
        bytes
    }

    fn frame_event() -> Vec<u8> {
        let expected = expected_frame();
        let mut bytes = vec![0; CAPTURE_FRAME_EVENT_SIZE];
        put_u32(&mut bytes, 0, CAPTURE_EVENT_FRAME);
        put_u32(&mut bytes, 4, CAPTURE_FRAME_EVENT_SIZE as u32);
        put_u64(&mut bytes, 8, expected.user_data);
        put_u64(&mut bytes, 16, expected.sequence);
        put_i64(&mut bytes, 24, expected.timestamp_ns);
        put_u64(&mut bytes, 32, expected.mode_generation);
        put_u32(&mut bytes, 40, expected.stream_id);
        put_u32(&mut bytes, 44, expected.buffer_id);
        put_i32(&mut bytes, 48, expected.status);
        put_u32(&mut bytes, 52, expected.flags);
        put_u32(&mut bytes, 56, expected.dropped_frames);
        put_i32(&mut bytes, 60, expected.damage_x);
        put_i32(&mut bytes, 64, expected.damage_y);
        put_u32(&mut bytes, 68, expected.damage_width);
        put_u32(&mut bytes, 72, expected.damage_height);
        put_u32(&mut bytes, 76, expected.cursor_serial);
        put_u32(&mut bytes, 80, expected.cursor_flags);
        put_i32(&mut bytes, 84, expected.cursor_x);
        put_i32(&mut bytes, 88, expected.cursor_y);
        put_u32(&mut bytes, 92, expected.cursor_hotspot_x);
        put_u32(&mut bytes, 96, expected.cursor_hotspot_y);
        put_u32(&mut bytes, 100, expected.cursor_width);
        put_u32(&mut bytes, 104, expected.cursor_height);
        bytes
    }

    fn cec_transmit_event() -> Vec<u8> {
        let mut bytes = vec![0; CEC_TRANSMIT_EVENT_SIZE];
        put_u32(&mut bytes, 0, CEC_EVENT_TX);
        put_u32(&mut bytes, 4, CEC_TRANSMIT_EVENT_SIZE as u32);
        put_u32(&mut bytes, 8, 7);
        put_u64(&mut bytes, 16, 11);
        put_u64(&mut bytes, 24, 13);
        put_u64(&mut bytes, 32, 17);
        put_u32(&mut bytes, 40, CONNECTOR_ID);
        put_u32(&mut bytes, 44, 3);
        bytes[48] = 2;
        bytes[49] = 3;
        bytes[50..53].copy_from_slice(&[0x04, 0x44, 0x41]);
        put_u32(&mut bytes, 68, 1);
        bytes
    }

    fn expected_frame() -> CaptureFrameEvent {
        CaptureFrameEvent {
            user_data: 0x1122_3344_5566_7788,
            sequence: 41,
            timestamp_ns: -7,
            mode_generation: 12,
            stream_id: 3,
            buffer_id: 4,
            status: 0,
            flags: CAPTURE_FRAME_FULL_DAMAGE | CAPTURE_FRAME_MODE_CHANGED,
            dropped_frames: 5,
            damage_x: -2,
            damage_y: 6,
            damage_width: 1920,
            damage_height: 1080,
            cursor_serial: 19,
            cursor_flags: CURSOR_VISIBLE | CURSOR_IMAGE_CHANGED,
            cursor_x: -20,
            cursor_y: 30,
            cursor_hotspot_x: 2,
            cursor_hotspot_y: 3,
            cursor_width: 64,
            cursor_height: 64,
        }
    }

    fn unknown_event(event_type: u32, length: usize) -> Vec<u8> {
        let mut bytes = vec![0x5a; length];
        put_u32(&mut bytes, 0, event_type);
        put_u32(&mut bytes, 4, length as u32);
        bytes
    }

    fn header(event_type: u32, length: u32) -> Vec<u8> {
        let mut bytes = vec![0; DRM_EVENT_HEADER_SIZE];
        put_u32(&mut bytes, 0, event_type);
        put_u32(&mut bytes, 4, length);
        bytes
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
    }
}
