//! Optional native graphics backend, independent of transport and capture policy.

mod clear;
mod copy;
mod device;
mod image;
mod private;
mod source;
mod stage;
mod submission;

#[cfg(test)]
mod test_support;

pub use copy::CopiedImages;
pub use device::{Device, DeviceIdentity};
pub use image::{Image, ImageLayout, PackedFormat};
pub use private::{
    BlendedImages, Blender, ComposedScene, Gamma, LayerRequirements, OutputMatrix,
    PendingPrivateRead, PrivateCopy, PrivateImage, PrivateLayer, SceneRequirements,
    SourceRequirements,
};
pub use source::SourceImage;
pub use stage::{OpaqueLayer, PendingStage};
