//! Shared generation timing for independently packetized audio and video.

use std::time::{Duration, Instant};

use crate::MediaGraphError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaStreamKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GenerationMediaTimeline {
    segment_pts_origin: u64,
    segment_reference_origin: Instant,
    reference_time_floor: Instant,
    video: StreamTimeline,
    audio: Option<StreamTimeline>,
}

#[derive(Debug, Clone, Copy)]
struct StreamTimeline {
    segment_pts_origin: u64,
    segment_media_origin: Duration,
    next_media_timestamp: Duration,
    last_pts: Option<u64>,
}

impl GenerationMediaTimeline {
    pub(crate) fn new(
        video_pts_origin: u64,
        audio_pts_origin: Option<u64>,
        generation_reference_origin: Instant,
    ) -> Self {
        let generation_pts_origin = audio_pts_origin
            .map(|audio| audio.min(video_pts_origin))
            .unwrap_or(video_pts_origin);
        Self {
            segment_pts_origin: generation_pts_origin,
            segment_reference_origin: generation_reference_origin,
            reference_time_floor: generation_reference_origin,
            video: StreamTimeline::new(video_pts_origin),
            audio: audio_pts_origin.map(StreamTimeline::new),
        }
    }

    pub(crate) fn reanchor(
        &mut self,
        video_pts_origin: u64,
        audio_pts_origin: Option<u64>,
        reference_origin: Instant,
    ) -> Result<(), MediaGraphError> {
        if self.audio.is_some() != audio_pts_origin.is_some() {
            return Err(MediaGraphError::new(
                "audio presence changed while reanchoring a media generation",
            ));
        }
        self.segment_pts_origin = audio_pts_origin
            .map(|audio| audio.min(video_pts_origin))
            .unwrap_or(video_pts_origin);
        self.segment_reference_origin = reference_origin.max(self.reference_time_floor);
        self.video.reanchor(video_pts_origin);
        if let (Some(audio), Some(origin)) = (&mut self.audio, audio_pts_origin) {
            audio.reanchor(origin);
        }
        Ok(())
    }

    pub(crate) fn timing(
        &mut self,
        kind: MediaStreamKind,
        pts: u64,
        duration: Duration,
    ) -> Result<(Duration, Instant), MediaGraphError> {
        let stream = match kind {
            MediaStreamKind::Video => &self.video,
            MediaStreamKind::Audio => self.audio.as_ref().ok_or_else(|| {
                MediaGraphError::new("audio timing requested for a video-only generation")
            })?,
        };
        if let Some(previous) = stream.last_pts {
            if pts <= previous {
                return Err(MediaGraphError::new(format!(
                    "encoded {} presentation timestamp did not advance from {previous} to {pts}",
                    kind.name()
                )));
            }
        }
        let media_elapsed = pts.checked_sub(stream.segment_pts_origin).ok_or_else(|| {
            MediaGraphError::new(format!(
                "encoded {} timestamp precedes its stream anchor",
                kind.name()
            ))
        })?;
        let segment_elapsed = pts.checked_sub(self.segment_pts_origin).ok_or_else(|| {
            MediaGraphError::new(format!(
                "encoded {} timestamp precedes its segment anchor",
                kind.name()
            ))
        })?;
        let media_timestamp = stream
            .segment_media_origin
            .checked_add(Duration::from_nanos(media_elapsed))
            .ok_or_else(|| MediaGraphError::new("encoded media timestamp overflowed"))?;
        let reference_time = self
            .segment_reference_origin
            .checked_add(Duration::from_nanos(segment_elapsed))
            .ok_or_else(|| MediaGraphError::new("encoded media reference time overflowed"))?;
        let next_media_timestamp = media_timestamp
            .checked_add(duration)
            .ok_or_else(|| MediaGraphError::new("encoded media end timestamp overflowed"))?;
        let reference_time_end = reference_time
            .checked_add(duration)
            .ok_or_else(|| MediaGraphError::new("encoded media reference end time overflowed"))?;

        let stream = match kind {
            MediaStreamKind::Video => &mut self.video,
            MediaStreamKind::Audio => self
                .audio
                .as_mut()
                .expect("audio presence was validated above"),
        };
        stream.last_pts = Some(pts);
        stream.next_media_timestamp = stream.next_media_timestamp.max(next_media_timestamp);
        self.reference_time_floor = self.reference_time_floor.max(reference_time_end);
        Ok((media_timestamp, reference_time))
    }
}

impl StreamTimeline {
    fn new(pts_origin: u64) -> Self {
        Self {
            segment_pts_origin: pts_origin,
            segment_media_origin: Duration::ZERO,
            next_media_timestamp: Duration::ZERO,
            last_pts: None,
        }
    }

    fn reanchor(&mut self, pts_origin: u64) {
        self.segment_pts_origin = pts_origin;
        self.segment_media_origin = self.next_media_timestamp;
        self.last_pts = None;
    }
}

impl MediaStreamKind {
    fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_stream_starts_at_zero_while_reference_time_preserves_av_offset() {
        let reference = Instant::now();
        let mut timeline = GenerationMediaTimeline::new(1_020, Some(1_000), reference);
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Audio, 1_000, Duration::from_nanos(20))
                .unwrap(),
            (Duration::ZERO, reference)
        );
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Video, 1_020, Duration::from_nanos(20))
                .unwrap(),
            (Duration::ZERO, reference + Duration::from_nanos(20))
        );
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Audio, 1_040, Duration::from_nanos(20))
                .unwrap(),
            (
                Duration::from_nanos(40),
                reference + Duration::from_nanos(40)
            )
        );
        assert!(timeline
            .timing(MediaStreamKind::Video, 1_019, Duration::from_nanos(20))
            .is_err());
    }

    #[test]
    fn video_only_timing_keeps_the_existing_zero_origin_contract() {
        let reference = Instant::now();
        let mut timeline = GenerationMediaTimeline::new(20_861_000_000_000, None, reference);
        assert_eq!(
            timeline
                .timing(
                    MediaStreamKind::Video,
                    20_861_000_000_000,
                    Duration::from_nanos(16_666_667),
                )
                .unwrap(),
            (Duration::ZERO, reference)
        );
        assert_eq!(
            timeline
                .timing(
                    MediaStreamKind::Video,
                    20_861_016_666_667,
                    Duration::from_nanos(16_666_667),
                )
                .unwrap(),
            (
                Duration::from_nanos(16_666_667),
                reference + Duration::from_nanos(16_666_667)
            )
        );
    }

    #[test]
    fn reanchor_preserves_monotonic_stream_time_across_source_epoch_changes() {
        let reference = Instant::now();
        let frame = Duration::from_nanos(20);
        let mut timeline = GenerationMediaTimeline::new(1_020, Some(1_000), reference);
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Video, 1_020, frame)
                .unwrap()
                .0,
            Duration::ZERO
        );
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Audio, 1_000, frame)
                .unwrap()
                .0,
            Duration::ZERO
        );
        assert_eq!(
            timeline
                .timing(MediaStreamKind::Video, 1_040, frame)
                .unwrap()
                .0,
            frame
        );

        let resumed_reference = reference + Duration::from_secs(1);
        timeline.reanchor(20, Some(10), resumed_reference).unwrap();
        assert_eq!(
            timeline.timing(MediaStreamKind::Audio, 10, frame).unwrap(),
            (frame, resumed_reference)
        );
        assert_eq!(
            timeline.timing(MediaStreamKind::Video, 20, frame).unwrap(),
            (frame * 2, resumed_reference + Duration::from_nanos(10))
        );
    }
}
