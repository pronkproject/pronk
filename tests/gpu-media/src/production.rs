//! The production media graph behind the generated-image fixture.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use pronk_media::{
    EncodedVideoAccessUnit, MediaGraphActor, MediaGraphConfiguration, MediaGraphError,
    MediaGraphStatistics, PipeWireVideoInput, VideoCadence, VideoEncoder,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::pattern::FRAMES;
use crate::OutputSize;

pub const MINIMUM_ENCODED_FRAMES: usize = 12;

pub enum Event {
    Activated,
    Encoded(EncodedVideoAccessUnit),
}

pub struct Consumer {
    generation: NonZeroU64,
    activation: Option<JoinHandle<Result<MediaGraphActor, MediaGraphError>>>,
    actor: Option<MediaGraphActor>,
    output: mpsc::Receiver<EncodedVideoAccessUnit>,
}

impl Consumer {
    pub fn start(
        socket: &Path,
        node_name: String,
        object_serial: NonZeroU64,
        caps: String,
        render_node: &Path,
        generation: NonZeroU64,
        output_size: OutputSize,
    ) -> Result<Self> {
        let remote = UnixStream::connect(socket)?;
        let encoder = VideoEncoder::va_h264(render_node);
        let cadence = VideoCadence::new(nz(30), nz(1));
        ensure!(
            encoder.supported_dimensions(&[(output_size.width, output_size.height)], cadence)?
                == [true],
            "selected VA converter and encoder do not accept the fixture picture size"
        );
        let (actor, output) = MediaGraphActor::spawn_with_output(FRAMES as usize)?;
        let configuration = MediaGraphConfiguration {
            media_generation: generation,
            video: PipeWireVideoInput {
                remote: remote.into(),
                node_name,
                object_serial,
                caps,
            },
            audio: None,
            video_encoder: encoder,
            video_cadence: cadence,
            video_bitrate: NonZeroU64::new(20_000_000).unwrap(),
        };
        let activation = tokio::spawn(async move {
            actor.configure(configuration).await?;
            actor.start(generation).await?;
            Ok(actor)
        });
        Ok(Self {
            generation,
            activation: Some(activation),
            actor: None,
            output,
        })
    }

    pub async fn next(&mut self) -> Result<Event> {
        if let Some(activation) = self.activation.as_mut() {
            tokio::select! {
                biased;
                result = activation => {
                    self.actor = Some(result.context("join production media activation")??);
                    self.activation = None;
                    Ok(Event::Activated)
                }
                output = self.output.recv() => output
                    .map(Event::Encoded)
                    .context("production encoded output closed during activation"),
            }
        } else {
            self.output
                .recv()
                .await
                .map(Event::Encoded)
                .context("production encoded output closed")
        }
    }

    pub async fn finish(
        mut self,
        render_node: &Path,
    ) -> Result<(MediaGraphStatistics, Vec<EncodedVideoAccessUnit>)> {
        let actor = match self.actor.take() {
            Some(actor) => actor,
            None => self
                .activation
                .take()
                .context("production media activation is missing")?
                .await
                .context("join production media activation")??,
        };
        let statistics = actor.stop(self.generation).await?;
        actor.shutdown().await?;
        ensure!(
            (MINIMUM_ENCODED_FRAMES as u64..=u64::from(FRAMES)).contains(&statistics.frames),
            "production encoder reported {} useful frames",
            statistics.frames
        );
        ensure!(
            statistics
                .encoder_name
                .as_deref()
                .is_some_and(|name| name.starts_with("va") && name.ends_with("h264enc"))
                && statistics.video_memory_path.as_deref()
                    == Some("DMA-BUF DMA_DRM to VA-memory NV12"),
            "production encoder did not report the qualified VA path: {statistics:?}"
        );
        let reported = statistics
            .render_device
            .as_deref()
            .context("production encoder did not report its render device")?;
        ensure!(
            std::fs::metadata(reported)?.rdev() == std::fs::metadata(render_node)?.rdev(),
            "production encoder used {reported}, not {}",
            render_node.display()
        );
        let mut remaining = Vec::new();
        while let Ok(unit) = self.output.try_recv() {
            remaining.push(unit);
        }
        Ok((statistics, remaining))
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        if let Some(activation) = self.activation.take() {
            activation.abort();
        }
    }
}

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}
