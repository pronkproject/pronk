//! Optional live receiver for capture qualification probes.
//! Only encoded access units cross this module's boundary, never pixel storage.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{ensure, Context};
use chromiacast::{
    CastApp, CastConnection, EncodedFrame, FrameDependency, Framerate, Offer, Resolution,
    SenderSession, StreamHandle, StreamStatistics, UdpTransport, VideoCodec, VideoStreamConfig,
    APP_MIRRORING,
};
use pronk_media::{EncodedVideoAccessUnit, VideoFrameDependency};
use tokio::sync::mpsc;

pub use chromiacast::SenderEvent;

/// Kept outside the probe timeout so cancellation still attempts remote stop.
/// Always call `shutdown`, including after a failed or canceled `start`.
#[derive(Default)]
#[must_use = "Call shutdown to stop the launched receiver application"]
pub struct Receiver {
    connection: Option<CastConnection>,
    app: Option<CastApp>,
    session: Option<SenderSession>,
    video: Option<StreamHandle>,
    events: Option<mpsc::Receiver<SenderEvent>>,
}

impl Receiver {
    pub async fn start(
        &mut self,
        address: SocketAddr,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        ensure!(self.connection.is_none(), "receiver already started");
        let offer = Offer::builder()
            .video(VideoStreamConfig {
                codec: VideoCodec::H264,
                max_bit_rate: 4_000_000,
                max_frame_rate: Framerate::new(30, 1)?,
                resolutions: vec![Resolution::new(width, height)?],
                target_delay: Duration::from_millis(400),
            })
            .build()?;
        self.connection = Some(CastConnection::connect_address(address).await?);
        let connection = self.connection.as_ref().unwrap();
        ensure!(
            connection.device_identity().is_some(),
            "receiver not authenticated"
        );
        self.app = Some(connection.launch(APP_MIRRORING).await?);
        let answer = connection
            .exchange_offer(&offer, self.app.as_ref().unwrap())
            .await?;
        validate_profile(&answer, width, height)?;
        let bind = match address.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let transport = UdpTransport::bind(SocketAddr::new(bind, 0)).await?;
        let (session, events) =
            SenderSession::start_address(&offer, &answer, address, transport).await?;
        self.video = session.video();
        self.session = Some(session);
        self.events = Some(events);
        ensure!(self.video.is_some(), "receiver rejected video");
        Ok(())
    }

    pub async fn send(&self, frame: EncodedVideoAccessUnit) -> anyhow::Result<()> {
        let dependency = match frame.dependency {
            VideoFrameDependency::KeyFrame => FrameDependency::KeyFrame,
            VideoFrameDependency::Delta => FrameDependency::Delta,
        };
        // Failure ends this finite probe. Never continue a delta chain after
        // dropping an encoded frame that the receiver would need as a reference.
        self.video
            .as_ref()
            .context("video not started")?
            .send(
                EncodedFrame::new(
                    dependency,
                    frame.data.into(),
                    frame.media_timestamp,
                    frame.reference_time,
                )
                .with_duration(frame.duration),
            )
            .await?;
        Ok(())
    }

    pub async fn next_event(&mut self) -> anyhow::Result<SenderEvent> {
        self.events
            .as_mut()
            .context("receiver not started")?
            .recv()
            .await
            .context("receiver stopped")
    }

    pub async fn statistics(&self) -> anyhow::Result<StreamStatistics> {
        self.session
            .as_ref()
            .context("sender not started")?
            .statistics()
            .await?
            .video
            .context("video statistics missing")
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let mut failure = None;
        if let Some(session) = self.session.take() {
            match tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await {
                Ok(Ok(())) => (),
                Ok(Err(error)) => {
                    failure = Some(error.into());
                }
                Err(error) => {
                    failure = Some(error.into());
                }
            }
        }
        if let Some(connection) = self.connection.take() {
            if let Some(app) = self.app.take() {
                match tokio::time::timeout(Duration::from_secs(5), connection.stop(&app)).await {
                    Ok(Ok(())) => (),
                    Ok(Err(error)) => {
                        failure.get_or_insert(error.into());
                    }
                    Err(error) => {
                        failure.get_or_insert(error.into());
                    }
                }
            }
            match tokio::time::timeout(Duration::from_secs(5), connection.close()).await {
                Ok(Ok(())) => (),
                Ok(Err(error)) => {
                    failure.get_or_insert(error.into());
                }
                Err(error) => {
                    failure.get_or_insert(error.into());
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn validate_profile(answer: &chromiacast::Answer, width: u32, height: u32) -> anyhow::Result<()> {
    ensure!(
        answer.send_indexes == [0],
        "receiver did not select the offered H.264 stream"
    );
    if let Some(video) = answer
        .constraints
        .as_ref()
        .and_then(|constraints| constraints.video.as_ref())
    {
        ensure!(
            !video
                .min_resolution
                .is_some_and(|minimum| width < minimum.width() || height < minimum.height())
                && !video
                    .max_dimensions
                    .is_some_and(|maximum| width > maximum.width
                        || height > maximum.height
                        || maximum
                            .frame_rate
                            .is_some_and(|rate| 30 * u64::from(rate.denominator())
                                > u64::from(rate.numerator())))
                && video
                    .min_bit_rate
                    .is_none_or(|minimum| minimum <= 4_000_000)
                && video
                    .max_bit_rate
                    .is_none_or(|maximum| maximum >= 4_000_000)
                && video.max_delay.is_none_or(|maximum| maximum >= 400)
                && !video
                    .max_pixels_per_second
                    .is_some_and(|maximum| f64::from(width) * f64::from(height) * 30.0 > maximum),
            "receiver constraints reject the fixed reference media profile"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer(video: serde_json::Value) -> chromiacast::Answer {
        serde_json::from_value(json!({
            "udpPort": 12345, "sendIndexes": [0], "ssrcs": [123],
            "constraints": {"video": video}
        }))
        .unwrap()
    }

    #[test]
    fn profile_respects_each_receiver_limit() {
        validate_profile(&answer(json!({})), 1920, 1080).unwrap();
        validate_profile(
            &answer(json!({
                "maxDimensions": {"width": 1920, "height": 1080, "frameRate": "30/1"},
                "minBitRate": 4_000_000, "maxBitRate": 4_000_000,
                "maxDelay": 400, "maxPixelsPerSecond": 1920 * 1080 * 30
            })),
            1920,
            1080,
        )
        .unwrap();
        for limit in [
            json!({"maxDimensions": {"width": 1280, "height": 1080}}),
            json!({"maxDimensions": {"width": 1920, "height": 720}}),
            json!({"maxDimensions": {"width": 1920, "height": 1080, "frameRate": "30000/1001"}}),
            json!({"minResolution": {"width": 3840, "height": 2160}}),
            json!({"minBitRate": 4_000_001}),
            json!({"maxBitRate": 3_999_999}),
            json!({"maxDelay": 399}),
            json!({"maxPixelsPerSecond": 1920 * 1080 * 30 - 1}),
        ] {
            assert!(
                validate_profile(&answer(limit.clone()), 1920, 1080).is_err(),
                "accepted {limit}"
            );
        }
        let mut wrong = answer(json!({}));
        wrong.send_indexes = vec![1];
        assert!(validate_profile(&wrong, 1920, 1080).is_err());
    }

    #[tokio::test]
    async fn shutdown_without_a_receiver_does_not_connect() {
        Receiver::default().shutdown().await.unwrap();
    }
}
