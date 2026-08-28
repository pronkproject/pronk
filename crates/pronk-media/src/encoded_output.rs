use tokio::sync::mpsc;

use crate::model::{
    EncodedAudioPacket, EncodedVideoAccessUnit, MediaGraphError, VideoFrameDependency,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputAdmission {
    Forwarded,
    Dropped { request_key_frame: bool },
}

pub(crate) struct EncodedVideoOutput {
    sender: mpsc::Sender<EncodedVideoAccessUnit>,
    needs_key_frame: bool,
}

pub(crate) struct EncodedAudioOutput {
    sender: mpsc::Sender<EncodedAudioPacket>,
}

impl EncodedVideoOutput {
    pub(crate) fn new(sender: mpsc::Sender<EncodedVideoAccessUnit>) -> Self {
        Self {
            sender,
            needs_key_frame: false,
        }
    }

    pub(crate) fn try_forward(
        &mut self,
        access_unit: EncodedVideoAccessUnit,
    ) -> Result<OutputAdmission, MediaGraphError> {
        if self.needs_key_frame && access_unit.dependency == VideoFrameDependency::Delta {
            return Ok(OutputAdmission::Dropped {
                request_key_frame: false,
            });
        }
        let dependency = access_unit.dependency;
        match self.sender.try_send(access_unit) {
            Ok(()) => {
                if dependency == VideoFrameDependency::KeyFrame {
                    self.needs_key_frame = false;
                }
                Ok(OutputAdmission::Forwarded)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                let request_key_frame =
                    !self.needs_key_frame || dependency == VideoFrameDependency::KeyFrame;
                self.needs_key_frame = true;
                Ok(OutputAdmission::Dropped { request_key_frame })
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(MediaGraphError::new(
                "encoded-video output channel closed while media was active",
            )),
        }
    }
}

impl EncodedAudioOutput {
    pub(crate) fn new(sender: mpsc::Sender<EncodedAudioPacket>) -> Self {
        Self { sender }
    }

    pub(crate) fn try_forward(
        &mut self,
        packet: EncodedAudioPacket,
    ) -> Result<OutputAdmission, MediaGraphError> {
        match self.sender.try_send(packet) {
            Ok(()) => Ok(OutputAdmission::Forwarded),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(OutputAdmission::Dropped {
                request_key_frame: false,
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(MediaGraphError::new(
                "encoded-audio output channel closed while media was active",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::time::{Duration, Instant};

    use crate::{EncodedAudioPacket, EncodedVideoAccessUnit, VideoFrameDependency};

    use super::{EncodedAudioOutput, EncodedVideoOutput, OutputAdmission};

    #[test]
    fn overflow_suppresses_deltas_until_a_key_frame_is_admitted() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut output = EncodedVideoOutput::new(sender);

        assert_eq!(
            output.try_forward(access_unit(VideoFrameDependency::KeyFrame)),
            Ok(OutputAdmission::Forwarded)
        );
        assert_eq!(
            output.try_forward(access_unit(VideoFrameDependency::Delta)),
            Ok(OutputAdmission::Dropped {
                request_key_frame: true
            })
        );
        assert_eq!(
            receiver.try_recv().unwrap().dependency,
            VideoFrameDependency::KeyFrame
        );
        assert_eq!(
            output.try_forward(access_unit(VideoFrameDependency::Delta)),
            Ok(OutputAdmission::Dropped {
                request_key_frame: false
            })
        );
        assert_eq!(
            output.try_forward(access_unit(VideoFrameDependency::KeyFrame)),
            Ok(OutputAdmission::Forwarded)
        );
        assert_eq!(
            receiver.try_recv().unwrap().dependency,
            VideoFrameDependency::KeyFrame
        );
        assert_eq!(
            output.try_forward(access_unit(VideoFrameDependency::Delta)),
            Ok(OutputAdmission::Forwarded)
        );
    }

    #[test]
    fn closed_consumer_fails_the_output_port() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        assert!(EncodedVideoOutput::new(sender)
            .try_forward(access_unit(VideoFrameDependency::KeyFrame))
            .is_err());
    }

    #[test]
    fn audio_overflow_is_bounded_and_preserves_timeline_gaps() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut output = EncodedAudioOutput::new(sender);
        assert_eq!(
            output.try_forward(audio_packet(0)),
            Ok(OutputAdmission::Forwarded)
        );
        assert_eq!(
            output.try_forward(audio_packet(20)),
            Ok(OutputAdmission::Dropped {
                request_key_frame: false
            })
        );
        assert_eq!(receiver.try_recv().unwrap().media_timestamp, Duration::ZERO);
        assert_eq!(
            output.try_forward(audio_packet(40)),
            Ok(OutputAdmission::Forwarded)
        );
        assert_eq!(
            receiver.try_recv().unwrap().media_timestamp,
            Duration::from_millis(40)
        );
    }

    fn access_unit(dependency: VideoFrameDependency) -> EncodedVideoAccessUnit {
        EncodedVideoAccessUnit {
            media_generation: NonZeroU64::new(1).unwrap(),
            dependency,
            data: vec![0, 0, 0, 1, 0x65],
            media_timestamp: Duration::ZERO,
            reference_time: Instant::now(),
            duration: Duration::from_millis(16),
        }
    }

    fn audio_packet(timestamp_ms: u64) -> EncodedAudioPacket {
        EncodedAudioPacket {
            media_generation: NonZeroU64::new(1).unwrap(),
            data: vec![0xf8, 0xff, 0xfe],
            media_timestamp: Duration::from_millis(timestamp_ms),
            reference_time: Instant::now() + Duration::from_millis(timestamp_ms),
            duration: Duration::from_millis(20),
        }
    }
}
