use drm_display_executor::scene::{
    blend::Blend,
    geometry::{DestinationRect, Extent, SourceRect},
    transform::Transform,
};

use super::*;
use crate::vulkan::{LayerRequirements, PackedFormat, SceneRequirements, SourceRequirements};

fn extent(width: u32, height: u32) -> Extent {
    Extent::new(width, height).unwrap()
}

#[test]
#[ignore = "requires explicit Vulkan GPU and modifier selection"]
fn complete_scene_support_is_checked_without_images() {
    let (device, modifier) = device();
    let blender = device.create_blender().unwrap();
    let source = SourceRequirements {
        format: PackedFormat::Bgra8,
        extent: extent(1920, 1080),
        modifier,
    };
    let layers = [LayerRequirements {
        source,
        crop: SourceRect::new(source.extent, [16, 8], extent(1280, 720)).unwrap(),
        destination: DestinationRect {
            position: [-20, 12],
            extent: extent(640, 360),
        },
        transform: Transform::default(),
        blend: Blend::default(),
    }];
    blender
        .check_scene(SceneRequirements {
            output: extent(1280, 720),
            layers: &layers,
        })
        .unwrap();

    let mut bad_modifier = layers;
    bad_modifier[0].source.modifier = u64::MAX;
    assert_eq!(
        blender
            .check_scene(SceneRequirements {
                output: extent(1280, 720),
                layers: &bad_modifier,
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );

    let mut wrong_source = layers;
    wrong_source[0].source.extent = extent(1281, 720);
    assert_eq!(
        blender
            .check_scene(SceneRequirements {
                output: extent(1280, 720),
                layers: &wrong_source,
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
#[ignore = "requires explicit Vulkan GPU selection"]
fn background_only_scene_still_checks_private_output_support() {
    let (device, _) = device();
    let blender = device.create_blender().unwrap();
    assert!(blender
        .check_scene(SceneRequirements {
            output: extent(64, 64),
            layers: &[],
        })
        .is_ok());
    assert_eq!(
        blender
            .check_scene(SceneRequirements {
                output: extent(u32::MAX, 1),
                layers: &[],
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}
