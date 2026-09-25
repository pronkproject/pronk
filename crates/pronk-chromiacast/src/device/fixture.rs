use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use pronk_backend_protocol::ControlOperation;
use pronk_media::{EncodedAudioPacket, EncodedVideoAccessUnit};
use tokio::sync::watch;

use super::{
    ControlDeviceInfo, ControlSetupInfo, DeviceConnector, DeviceControl, DeviceControlError,
    MirroringAvailability,
};
use crate::transport::{
    AudioSendOutcome, AudioSenderPort, NegotiatedVideoTransport, VideoSendOutcome, VideoSenderPort,
    VideoTransportConfiguration, VideoTransportError, VideoTransportFeedbackSnapshot,
    VideoTransportNegotiator, VideoTransportPressure,
};

#[derive(Debug, Default)]
pub(crate) struct FixtureDeviceConnector;

#[async_trait]
impl DeviceConnector for FixtureDeviceConnector {
    async fn connect(
        &self,
        _endpoint: SocketAddr,
    ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
        Ok(Box::new(FixtureDeviceControl))
    }
}

#[derive(Debug)]
pub(super) struct FixtureDeviceControl;

#[derive(Debug)]
struct FixtureVideoSender {
    feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
    feedback_sent: bool,
}

#[derive(Debug)]
struct FixtureAudioSender {
    feedback: watch::Sender<VideoTransportFeedbackSnapshot>,
}

#[async_trait]
impl AudioSenderPort for FixtureAudioSender {
    async fn send(
        &mut self,
        _packet: EncodedAudioPacket,
    ) -> Result<AudioSendOutcome, VideoTransportError> {
        self.feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_audio_packets =
                snapshot.acknowledged_audio_packets.saturating_add(1);
        });
        Ok(AudioSendOutcome::Accepted)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

#[async_trait]
impl VideoSenderPort for FixtureVideoSender {
    async fn send(
        &mut self,
        _access_unit: EncodedVideoAccessUnit,
    ) -> Result<VideoSendOutcome, VideoTransportError> {
        let emit_initial_pressure = !self.feedback_sent;
        self.feedback_sent = true;
        self.feedback.send_modify(|snapshot| {
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.acknowledged_frames = snapshot.acknowledged_frames.saturating_add(1);
            if emit_initial_pressure {
                snapshot.key_frame_requests = snapshot.key_frame_requests.saturating_add(1);
                snapshot.pressure = Some(VideoTransportPressure {
                    in_flight_frames: 12,
                    in_flight_media_duration: Duration::from_millis(250),
                    max_acceptable_in_flight_duration: Duration::from_millis(100),
                    current_rtt: Some(Duration::from_millis(80)),
                    receiver_playout_delay: None,
                    nack_count: 0,
                    frames_dropped_or_skipped: 1,
                    fraction_lost: Some(32),
                });
            }
        });
        if emit_initial_pressure {
            let feedback = self.feedback.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                feedback.send_modify(|snapshot| {
                    snapshot.revision = snapshot.revision.saturating_add(1);
                    snapshot.pressure = Some(VideoTransportPressure {
                        max_acceptable_in_flight_duration: Duration::from_millis(100),
                        ..VideoTransportPressure::default()
                    });
                });
            });
        }
        Ok(VideoSendOutcome::Accepted)
    }

    async fn shutdown(self: Box<Self>) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

#[async_trait]
impl VideoTransportNegotiator for FixtureDeviceControl {
    async fn negotiate_video(
        &mut self,
        configuration: VideoTransportConfiguration,
    ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
        Ok(fixture_video_transport(configuration.audio.is_some()))
    }

    async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
        Ok(())
    }
}

pub(super) fn fixture_video_transport(with_audio: bool) -> NegotiatedVideoTransport {
    let (feedback, receiver) = watch::channel(VideoTransportFeedbackSnapshot::default());
    NegotiatedVideoTransport {
        video_codec: pronk_media::VideoCodec::Vp8,
        sender: Box::new(FixtureVideoSender {
            feedback: feedback.clone(),
            feedback_sent: false,
        }),
        audio_sender: with_audio
            .then(|| Box::new(FixtureAudioSender { feedback }) as Box<dyn AudioSenderPort>),
        feedback: receiver,
        minimum_bitrate: std::num::NonZeroU32::new(500_000),
    }
}

#[async_trait]
impl DeviceControl for FixtureDeviceControl {
    async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
        Ok(ControlDeviceInfo {
            device_id: "00112233-4455-6677-8899-AABBCCDDEEFF".into(),
            device_model: Some("Authenticated Device Model".into()),
            capabilities: Some(5),
        })
    }

    async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
        Ok(ControlSetupInfo::Available {
            manufacturer: Some("Sony Corporation".into()),
            product_name: Some("BRAVIA 8".into()),
            ssdp_udn: Some("uuid:00112233-4455-6677-8899-aabbccddeeff".into()),
        })
    }

    async fn get_mirroring_availability(
        &self,
    ) -> Result<MirroringAvailability, DeviceControlError> {
        Ok(MirroringAvailability::Available)
    }

    async fn transmit_control(
        &mut self,
        _operation: &ControlOperation,
    ) -> Result<(), DeviceControlError> {
        Ok(())
    }

    async fn close(self: Box<Self>) -> Result<(), DeviceControlError> {
        Ok(())
    }
}
