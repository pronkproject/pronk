//! Pure connector-to-ALSA-sink identity resolution.
//!
//! The foreign PipeWire loop records only a bounded set of properties. This
//! module owns all interpretation so transport callbacks never acquire policy
//! or CastKMS identity rules.

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::{CastKmsAudioSinkRequest, CastKmsAudioSinkTarget};

pub(crate) const CASTKMS_ALSA_ID_PREFIX: &str = "CastKMS";
pub(crate) const CASTKMS_AUDIO_SINK_PROPERTY: &str = "api.pronk.castkms.audio-sink";
pub(crate) const CASTKMS_AUDIO_OUTPUT_INDEX_PROPERTY: &str = "api.pronk.castkms.output-index";
pub(crate) const CASTKMS_AUDIO_POLICY_VERSION: &str = "v1";
const AUDIO_DEVICE_CLASS: &str = "Audio/Device";
const AUDIO_SINK_CLASS: &str = "Audio/Sink";
const PLAYBACK_STREAM: &str = "playback";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioObjectKind {
    Device,
    Node,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioObjectObservation {
    pub object_id: u32,
    pub kind: AudioObjectKind,
    pub media_class: Option<String>,
    pub card_id: Option<String>,
    pub policy_marker: Option<String>,
    pub device_bus_path: Option<String>,
    pub device_id: Option<String>,
    pub output_index: Option<String>,
    pub pcm_stream: Option<String>,
    pub node_name: Option<String>,
    pub object_serial: Option<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum AudioSinkResolutionError {
    #[error("CastKMS audio object {object_id} has invalid or missing {property}")]
    Malformed {
        object_id: u32,
        property: &'static str,
    },
    #[error("no CastKMS audio sink matches device {device_path:?}, output index {output_index}")]
    NotFound {
        device_path: PathBuf,
        output_index: u32,
    },
    #[error(
        "more than one CastKMS audio sink matches device {device_path:?}, output index {output_index}"
    )]
    Ambiguous {
        device_path: PathBuf,
        output_index: u32,
    },
}

pub(crate) fn resolve_audio_sink(
    request: &CastKmsAudioSinkRequest,
    observations: impl IntoIterator<Item = AudioObjectObservation>,
) -> Result<CastKmsAudioSinkTarget, AudioSinkResolutionError> {
    let observations: Vec<_> = observations.into_iter().collect();
    let mut devices = HashMap::new();
    for observation in observations
        .iter()
        .filter(|observation| observation.kind == AudioObjectKind::Device)
    {
        if observation.media_class.as_deref() != Some(AUDIO_DEVICE_CLASS)
            || !observation
                .card_id
                .as_deref()
                .is_some_and(is_castkms_card_id)
        {
            continue;
        }
        let bus_path = required(observation, "device.bus-path", &observation.device_bus_path)?;
        let parent = castkms_parent_path(bus_path).ok_or(AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property: "device.bus-path",
        })?;
        let output_index = parse_u32(
            observation,
            CASTKMS_AUDIO_OUTPUT_INDEX_PROPERTY,
            &observation.output_index,
        )?;
        devices.insert(observation.object_id, (parent, output_index));
    }

    let matching_devices: Vec<_> = devices
        .iter()
        .filter_map(|(id, (path, output_index))| {
            (path == &request.device_path && *output_index == request.output_index).then_some(*id)
        })
        .collect();
    let device_id = match matching_devices.as_slice() {
        [device_id] => *device_id,
        [] => return Err(not_found(request)),
        _ => return Err(ambiguous(request)),
    };

    let mut matches = Vec::new();
    for observation in observations
        .iter()
        .filter(|observation| observation.kind == AudioObjectKind::Node)
    {
        if observation.policy_marker.as_deref() != Some(CASTKMS_AUDIO_POLICY_VERSION) {
            continue;
        }
        require_property(observation, "media.class", AUDIO_SINK_CLASS)?;
        require_castkms_card_id(observation)?;
        require_property(observation, "api.alsa.pcm.stream", PLAYBACK_STREAM)?;
        let observed_device_id = parse_u32(observation, "device.id", &observation.device_id)?;
        let output_index = parse_u32(
            observation,
            CASTKMS_AUDIO_OUTPUT_INDEX_PROPERTY,
            &observation.output_index,
        )?;
        let node_name = required(observation, "node.name", &observation.node_name)?;
        if node_name.is_empty() || node_name.contains('\0') {
            return Err(AudioSinkResolutionError::Malformed {
                object_id: observation.object_id,
                property: "node.name",
            });
        }
        let object_id =
            NonZeroU32::new(observation.object_id).ok_or(AudioSinkResolutionError::Malformed {
                object_id: observation.object_id,
                property: "object.id",
            })?;
        let object_serial = parse_u64(observation, "object.serial", &observation.object_serial)
            .and_then(|value| {
                NonZeroU64::new(value).ok_or(AudioSinkResolutionError::Malformed {
                    object_id: observation.object_id,
                    property: "object.serial",
                })
            })?;
        if observed_device_id == device_id && output_index == request.output_index {
            matches.push(CastKmsAudioSinkTarget {
                node_name: node_name.to_string(),
                object_id,
                object_serial,
            });
        }
    }

    match matches.as_slice() {
        [target] => Ok(target.clone()),
        [] => Err(not_found(request)),
        _ => Err(ambiguous(request)),
    }
}

fn require_property(
    observation: &AudioObjectObservation,
    property: &'static str,
    expected: &str,
) -> Result<(), AudioSinkResolutionError> {
    let actual = match property {
        "media.class" => observation.media_class.as_deref(),
        "api.alsa.pcm.stream" => observation.pcm_stream.as_deref(),
        _ => unreachable!("all exact properties are enumerated"),
    };
    if actual != Some(expected) {
        return Err(AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property,
        });
    }
    Ok(())
}

fn require_castkms_card_id(
    observation: &AudioObjectObservation,
) -> Result<(), AudioSinkResolutionError> {
    if !observation
        .card_id
        .as_deref()
        .is_some_and(is_castkms_card_id)
    {
        return Err(AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property: "api.alsa.card.id",
        });
    }
    Ok(())
}

fn is_castkms_card_id(value: &str) -> bool {
    value
        .strip_prefix(CASTKMS_ALSA_ID_PREFIX)
        .and_then(|suffix| suffix.bytes().next())
        .is_some_and(|first| first.is_ascii_digit())
}

fn required<'a>(
    observation: &AudioObjectObservation,
    property: &'static str,
    value: &'a Option<String>,
) -> Result<&'a str, AudioSinkResolutionError> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property,
        })
}

fn parse_u32(
    observation: &AudioObjectObservation,
    property: &'static str,
    value: &Option<String>,
) -> Result<u32, AudioSinkResolutionError> {
    required(observation, property, value)?
        .parse()
        .map_err(|_| AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property,
        })
}

fn parse_u64(
    observation: &AudioObjectObservation,
    property: &'static str,
    value: &Option<String>,
) -> Result<u64, AudioSinkResolutionError> {
    required(observation, property, value)?
        .parse()
        .map_err(|_| AudioSinkResolutionError::Malformed {
            object_id: observation.object_id,
            property,
        })
}

fn castkms_parent_path(bus_path: &str) -> Option<PathBuf> {
    let path = Path::new(bus_path);
    if !is_normal_absolute_path(path) {
        return None;
    }
    let card = path.file_name()?.to_str()?.strip_prefix("card")?;
    if card.is_empty() || !card.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let sound = path.parent()?;
    if sound.file_name()?.to_str()? != "sound" {
        return None;
    }
    sound.parent().map(Path::to_path_buf)
}

pub(crate) fn is_normal_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn not_found(request: &CastKmsAudioSinkRequest) -> AudioSinkResolutionError {
    AudioSinkResolutionError::NotFound {
        device_path: request.device_path.clone(),
        output_index: request.output_index,
    }
}

fn ambiguous(request: &CastKmsAudioSinkRequest) -> AudioSinkResolutionError {
    AudioSinkResolutionError::Ambiguous {
        device_path: request.device_path.clone(),
        output_index: request.output_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CastKmsAudioSinkRequest {
        CastKmsAudioSinkRequest {
            device_path: "/sys/devices/faux/castkms".into(),
            output_index: 1,
        }
    }

    fn device(id: u32, path: &str, output_index: u32) -> AudioObjectObservation {
        AudioObjectObservation {
            object_id: id,
            kind: AudioObjectKind::Device,
            media_class: Some(AUDIO_DEVICE_CLASS.into()),
            card_id: Some("CastKMS1".into()),
            policy_marker: Some(CASTKMS_AUDIO_POLICY_VERSION.into()),
            device_bus_path: Some(path.into()),
            device_id: None,
            output_index: Some(output_index.to_string()),
            pcm_stream: None,
            node_name: None,
            object_serial: None,
        }
    }

    fn sink(id: u32, device_id: u32, output_index: u32) -> AudioObjectObservation {
        AudioObjectObservation {
            object_id: id,
            kind: AudioObjectKind::Node,
            media_class: Some(AUDIO_SINK_CLASS.into()),
            card_id: Some(format!("CastKMS{output_index}")),
            policy_marker: Some(CASTKMS_AUDIO_POLICY_VERSION.into()),
            device_bus_path: None,
            device_id: Some(device_id.to_string()),
            output_index: Some(output_index.to_string()),
            pcm_stream: Some(PLAYBACK_STREAM.into()),
            node_name: Some(format!("alsa_output.castkms.{output_index}")),
            object_serial: Some((1_000 + u64::from(id)).to_string()),
        }
    }

    #[test]
    fn joins_standard_alsa_identity_without_parsing_node_name() {
        let expected = CastKmsAudioSinkTarget {
            node_name: "this name is deliberately opaque".into(),
            object_id: NonZeroU32::new(52).unwrap(),
            object_serial: NonZeroU64::new(1_052).unwrap(),
        };
        let mut matching_sink = sink(52, 51, 1);
        matching_sink.node_name = Some(expected.node_name.clone());
        let resolved = resolve_audio_sink(
            &request(),
            [
                device(51, "/sys/devices/faux/castkms/sound/card0", 1),
                sink(53, 51, 0),
                matching_sink,
                device(54, "/sys/devices/faux/castkms/sound/card1", 0),
                sink(55, 54, 0),
                device(60, "/sys/devices/pci0000:00/other/sound/card2", 1),
                sink(61, 60, 1),
            ],
        )
        .unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn ignores_unrecognized_policy_versions() {
        let mut impostor = sink(53, 51, 1);
        impostor.policy_marker = Some("v2".into());
        assert!(resolve_audio_sink(
            &request(),
            [
                device(51, "/sys/devices/faux/castkms/sound/card0", 1),
                sink(52, 51, 1),
                impostor,
            ],
        )
        .is_ok());
    }

    #[test]
    fn rejects_ambiguity_and_malformed_marked_objects() {
        let observations = [
            device(51, "/sys/devices/faux/castkms/sound/card0", 1),
            sink(52, 51, 1),
            sink(53, 51, 1),
        ];
        assert!(matches!(
            resolve_audio_sink(&request(), observations),
            Err(AudioSinkResolutionError::Ambiguous { .. })
        ));

        let mut malformed = sink(52, 51, 1);
        malformed.object_serial = Some("not-a-number".into());
        assert_eq!(
            resolve_audio_sink(
                &request(),
                [
                    device(51, "/sys/devices/faux/castkms/sound/card0", 1),
                    malformed,
                ],
            ),
            Err(AudioSinkResolutionError::Malformed {
                object_id: 52,
                property: "object.serial",
            })
        );
    }

    #[test]
    fn bus_path_must_have_an_exact_sound_card_suffix() {
        for invalid in [
            "/sys/devices/faux/castkms/card0",
            "/sys/devices/faux/castkms/sound/card",
            "/sys/devices/faux/castkms/sound/cardx",
            "/sys/devices/faux/castkms/sound/../sound/card0",
            "sys/devices/faux/castkms/sound/card0",
        ] {
            assert!(castkms_parent_path(invalid).is_none(), "{invalid}");
        }
        assert_eq!(
            castkms_parent_path("/sys/devices/faux/castkms/sound/card19"),
            Some(PathBuf::from("/sys/devices/faux/castkms"))
        );
    }
}
