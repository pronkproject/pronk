use std::num::NonZeroU64;
use std::os::fd::OwnedFd as StdOwnedFd;

use pronk_backend_protocol::{MediaConfiguration, MediaKind, PipeWireTarget, SessionStatistics};
use pronk_media::{
    MediaGraphActor, MediaGraphConfiguration, MediaGraphError, MediaGraphStatistics,
    PipeWireVideoInput, ValidatedVideoCaps,
};
use thiserror::Error;
use zbus::zvariant::OwnedFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MockMediaMode {
    GStreamer,
    RetainForProtocolTest,
}

impl MockMediaMode {
    pub(crate) fn from_environment() -> Result<Self, MockMediaError> {
        match std::env::var("PRONK_BACKEND_MOCK_MEDIA_MODE")
            .as_deref()
            .unwrap_or("gstreamer")
        {
            "gstreamer" => Ok(Self::GStreamer),
            "retain-for-protocol-test" => Ok(Self::RetainForProtocolTest),
            value => Err(MockMediaError::new(format!(
                "unknown mock media mode {value:?}"
            ))),
        }
    }

    pub(crate) fn supports_audio(self) -> bool {
        matches!(self, Self::RetainForProtocolTest)
    }
}

#[derive(Debug)]
pub(crate) enum MockMediaEngine {
    GStreamer {
        actor: Option<MediaGraphActor>,
        media_generation: Option<NonZeroU64>,
        actor_received_generation: bool,
        video_bitrate: u64,
    },
    RetainForProtocolTest {
        media_generation: Option<NonZeroU64>,
        remotes: Vec<StdOwnedFd>,
        video_bitrate: u64,
    },
}

impl MockMediaEngine {
    pub(crate) fn new(mode: MockMediaMode) -> Result<Self, MockMediaError> {
        match mode {
            MockMediaMode::GStreamer => Ok(Self::GStreamer {
                actor: Some(MediaGraphActor::spawn()?),
                media_generation: None,
                actor_received_generation: false,
                video_bitrate: 0,
            }),
            MockMediaMode::RetainForProtocolTest => Ok(Self::RetainForProtocolTest {
                media_generation: None,
                remotes: Vec::new(),
                video_bitrate: 0,
            }),
        }
    }

    pub(crate) async fn configure(
        &mut self,
        remotes: Vec<OwnedFd>,
        targets: Vec<PipeWireTarget>,
        configuration: &MediaConfiguration,
        media_generation: NonZeroU64,
    ) -> Result<(), MockMediaError> {
        match self {
            Self::RetainForProtocolTest {
                media_generation: active,
                remotes: retained,
                video_bitrate,
            } => {
                *active = Some(media_generation);
                *retained = remotes.into_iter().map(Into::into).collect();
                *video_bitrate = configuration.video_bitrate;
                Ok(())
            }
            Self::GStreamer {
                actor,
                media_generation: active,
                actor_received_generation,
                video_bitrate,
            } => {
                if active.is_some() {
                    return Err(MockMediaError::new(
                        "mock GStreamer media already has an active generation",
                    ));
                }
                // Record ownership before validation or the actor await. The
                // D-Bus request may be cancelled after its FDs have crossed
                // the boundary, and StopMedia must still be able to clean up
                // that exact generation.
                *active = Some(media_generation);
                *actor_received_generation = false;
                *video_bitrate = configuration.video_bitrate;
                if remotes.len() != 1 || targets.len() != 1 {
                    return Err(MockMediaError::new(
                        "mock GStreamer media currently supports video-only configuration",
                    ));
                }
                let remote: StdOwnedFd = remotes
                    .into_iter()
                    .next()
                    .expect("one remote checked")
                    .into();
                let target = targets.into_iter().next().expect("one target checked");
                validate_video_target(&target, configuration)?;
                let graph = actor
                    .as_ref()
                    .ok_or_else(|| MockMediaError::new("mock media engine is shut down"))?;
                *actor_received_generation = true;
                graph
                    .configure(MediaGraphConfiguration {
                        media_generation,
                        video: PipeWireVideoInput {
                            remote,
                            node_name: target.node_name,
                            object_serial: NonZeroU64::new(target.object_serial)
                                .expect("wire validation rejected zero object serial"),
                            caps: target.caps,
                        },
                        audio: None,
                        video_codec: pronk_media::VideoCodec::H264,
                        video_bitrate: NonZeroU64::new(configuration.video_bitrate)
                            .expect("wire validation rejected zero bitrate"),
                    })
                    .await?;
                Ok(())
            }
        }
    }

    pub(crate) async fn start(&self, media_generation: NonZeroU64) -> Result<(), MockMediaError> {
        match self {
            Self::GStreamer {
                actor,
                media_generation: active,
                ..
            } => {
                require_generation(*active, media_generation)?;
                actor_ref(actor)?.start(media_generation).await?;
            }
            Self::RetainForProtocolTest {
                media_generation: active,
                ..
            } => require_generation(*active, media_generation)?,
        }
        Ok(())
    }

    pub(crate) async fn suspend(&self, media_generation: NonZeroU64) -> Result<(), MockMediaError> {
        match self {
            Self::GStreamer {
                actor,
                media_generation: active,
                ..
            } => {
                require_generation(*active, media_generation)?;
                actor_ref(actor)?.suspend(media_generation).await?;
            }
            Self::RetainForProtocolTest {
                media_generation: active,
                ..
            } => require_generation(*active, media_generation)?,
        }
        Ok(())
    }

    pub(crate) async fn resume(&self, media_generation: NonZeroU64) -> Result<(), MockMediaError> {
        match self {
            Self::GStreamer {
                actor,
                media_generation: active,
                ..
            } => {
                require_generation(*active, media_generation)?;
                actor_ref(actor)?.resume(media_generation).await?;
            }
            Self::RetainForProtocolTest {
                media_generation: active,
                ..
            } => require_generation(*active, media_generation)?,
        }
        Ok(())
    }

    pub(crate) async fn stop(
        &mut self,
        media_generation: NonZeroU64,
    ) -> Result<(), MockMediaError> {
        match self {
            Self::GStreamer {
                actor,
                media_generation: active,
                actor_received_generation,
                video_bitrate,
            } => {
                require_generation(*active, media_generation)?;
                let result = if *actor_received_generation {
                    actor_ref(actor)?.stop(media_generation).await.map(|_| ())
                } else {
                    Ok(())
                };
                *active = None;
                *actor_received_generation = false;
                *video_bitrate = 0;
                result?;
            }
            Self::RetainForProtocolTest {
                media_generation: active,
                remotes,
                ..
            } => {
                require_generation(*active, media_generation)?;
                remotes.clear();
                *active = None;
            }
        }
        Ok(())
    }

    pub(crate) async fn statistics(
        &self,
        session_generation: u64,
        media_generation: NonZeroU64,
    ) -> Result<SessionStatistics, MockMediaError> {
        let (statistics, video_bitrate) = match self {
            Self::GStreamer {
                actor,
                media_generation: active,
                video_bitrate,
                ..
            } => {
                require_generation(*active, media_generation)?;
                (
                    actor_ref(actor)?.statistics(media_generation).await?,
                    *video_bitrate,
                )
            }
            Self::RetainForProtocolTest {
                media_generation: active,
                video_bitrate,
                ..
            } => {
                require_generation(*active, media_generation)?;
                (MediaGraphStatistics::default(), *video_bitrate)
            }
        };
        Ok(SessionStatistics {
            session_generation,
            media_generation: media_generation.get(),
            video_bitrate,
            encoded_frames: statistics.frames,
            dropped_frames: statistics.dropped_frames,
            queue_delay_micros: 0,
        })
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), MockMediaError> {
        match self {
            Self::GStreamer {
                actor,
                media_generation,
                actor_received_generation,
                video_bitrate,
            } => {
                if let Some(actor) = actor.take() {
                    actor.shutdown().await?;
                }
                *media_generation = None;
                *actor_received_generation = false;
                *video_bitrate = 0;
            }
            Self::RetainForProtocolTest {
                media_generation,
                remotes,
                ..
            } => {
                remotes.clear();
                *media_generation = None;
            }
        }
        Ok(())
    }
}

fn validate_video_target(
    target: &PipeWireTarget,
    configuration: &MediaConfiguration,
) -> Result<(), MockMediaError> {
    if target.kind != MediaKind::Video {
        return Err(MockMediaError::new("first PipeWire target is not video"));
    }
    let caps = ValidatedVideoCaps::parse(&target.caps)?;
    if caps.width.get() != configuration.mode.width
        || caps.height.get() != configuration.mode.height
    {
        return Err(MockMediaError::new(format!(
            "video caps are {}x{} but configured mode is {}x{}",
            caps.width, caps.height, configuration.mode.width, configuration.mode.height
        )));
    }
    let caps_millihz = u64::from(caps.framerate_numerator.get())
        .checked_mul(1000)
        .ok_or_else(|| MockMediaError::new("video caps refresh overflows"))?;
    let mode_millihz = u64::from(configuration.mode.refresh_millihz)
        .checked_mul(u64::from(caps.framerate_denominator.get()))
        .ok_or_else(|| MockMediaError::new("configured refresh overflows"))?;
    if caps_millihz != mode_millihz {
        return Err(MockMediaError::new(format!(
            "video caps refresh {}/{} differs from configured {} mHz",
            caps.framerate_numerator,
            caps.framerate_denominator,
            configuration.mode.refresh_millihz
        )));
    }
    Ok(())
}

fn actor_ref(actor: &Option<MediaGraphActor>) -> Result<&MediaGraphActor, MockMediaError> {
    actor
        .as_ref()
        .ok_or_else(|| MockMediaError::new("mock media engine is shut down"))
}

fn require_generation(
    active: Option<NonZeroU64>,
    requested: NonZeroU64,
) -> Result<(), MockMediaError> {
    if active == Some(requested) {
        Ok(())
    } else {
        Err(MockMediaError::new(format!(
            "requested media generation {requested}; retained generation is {active:?}"
        )))
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct MockMediaError(String);

impl MockMediaError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<MediaGraphError> for MockMediaError {
    fn from(error: MediaGraphError) -> Self {
        Self::new(error.to_string())
    }
}
