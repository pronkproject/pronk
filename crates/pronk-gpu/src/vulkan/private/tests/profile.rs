use drm_display_executor::scene::{
    blend::Blend,
    color::{ColorMatrix, Lut, OutputColor},
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
    let table = [[0; 3], [65535; 3]];
    let mut coefficients = [0; 12];
    coefficients[0] = 1 << 32;
    coefficients[5] = 1 << 32;
    coefficients[10] = 1 << 32;
    let color = OutputColor {
        degamma: Some(Lut::new(&table).unwrap()),
        matrix: Some(ColorMatrix::from_sign_magnitude(coefficients)),
        gamma: Some(Lut::new(&table).unwrap()),
    };
    let result = blender.check_scene(SceneRequirements {
        output: extent(1280, 720),
        layers: &layers,
        color,
    });
    assert_eq!(result.is_ok(), device.supports_shader_int64());

    let mut bad_modifier = layers;
    bad_modifier[0].source.modifier = u64::MAX;
    assert_eq!(
        blender
            .check_scene(SceneRequirements {
                output: extent(1280, 720),
                layers: &bad_modifier,
                color: OutputColor::default(),
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
                color: OutputColor::default(),
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
            color: OutputColor::default(),
        })
        .is_ok());
    assert_eq!(
        blender
            .check_scene(SceneRequirements {
                output: extent(u32::MAX, 1),
                layers: &[],
                color: OutputColor::default(),
            })
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}
