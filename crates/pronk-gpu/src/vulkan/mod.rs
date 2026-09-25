//! Optional native graphics backend, independent of transport and capture policy.

mod clear;
mod copy;
mod destination;
mod device;
mod external;
mod image;
mod private;
mod source;
mod stage;
mod submission;

#[cfg(any(test, feature = "test-oracle"))]
pub mod test_support;

pub use copy::CopiedImages;
pub use destination::{DestinationCopy, DestinationImage};
pub use device::{Device, DeviceIdentity, RenderNodeIdentity};
pub use image::{Image, ImageLayout, PackedFormat};
pub use private::{
    BlendedImages, Blender, ColorPipelineProgram, ComposedScene, Gamma, LayerRequirements,
    OutputColorProgram, OutputMatrix, PendingPrivateRead, PrivateCopy, PrivateImage, PrivateLayer,
    SceneRequirements, SourceRequirements,
};
pub use source::SourceImage;
pub use stage::{OpaqueLayer, PendingStage};
