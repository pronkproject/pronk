//! Ordered output color processing for a completed private scene.

use std::io;
use std::sync::Arc;

use drm_display_executor::scene::{
    color::{ColorOperation, ColorPipeline, OutputColor},
    geometry::Extent,
};

use super::{
    transfer::{Function, Transfer},
    Gamma, OutputMatrix, PrivateImage,
};
use crate::vulkan::Device;

/// Immutable native implementation of a complete output color pipeline.
#[derive(Clone)]
pub struct OutputColorProgram {
    pipeline: ColorPipelineProgram,
}

#[derive(Clone)]
enum ColorStage {
    Lookup(Gamma),
    Matrix(OutputMatrix),
    Transfer(Transfer),
}

/// Immutable native implementation of an ordered RGB color pipeline.
#[derive(Clone)]
pub struct ColorPipelineProgram {
    device: Arc<super::super::device::DeviceInner>,
    extent: Extent,
    stages: Vec<ColorStage>,
}

impl Device {
    /// Check ordered color operations without allocating native data.
    pub fn check_color_pipeline(&self, extent: Extent, color: ColorPipeline<'_>) -> io::Result<()> {
        for operation in color.operations() {
            match operation {
                ColorOperation::Bypass => {}
                ColorOperation::SrgbEotf | ColorOperation::SrgbInverseEotf => {
                    super::transfer::check_support(&self.inner, extent)?;
                }
                ColorOperation::Matrix(_) => {
                    super::matrix::check_support(&self.inner, extent)?;
                }
                ColorOperation::Lut(table) => {
                    super::gamma::check_support(&self.inner, table.entries().len(), extent)?;
                }
            }
        }
        Ok(())
    }

    /// Prepare every non-bypass operation in an ordered color pipeline.
    pub fn create_color_pipeline(
        &self,
        extent: Extent,
        color: ColorPipeline<'_>,
    ) -> io::Result<ColorPipelineProgram> {
        self.check_color_pipeline(extent, color)?;
        let mut stages = Vec::new();
        stages
            .try_reserve_exact(color.operations().len())
            .map_err(io::Error::other)?;
        for operation in color.operations() {
            let stage = match *operation {
                ColorOperation::Bypass => continue,
                ColorOperation::SrgbEotf => {
                    ColorStage::Transfer(Transfer::new(self, Function::Eotf)?)
                }
                ColorOperation::SrgbInverseEotf => {
                    ColorStage::Transfer(Transfer::new(self, Function::InverseEotf)?)
                }
                ColorOperation::Matrix(matrix) => {
                    ColorStage::Matrix(self.create_output_matrix(matrix)?)
                }
                ColorOperation::Lut(table) => {
                    ColorStage::Lookup(self.create_gamma(table.entries())?)
                }
            };
            stages.push(stage);
        }
        Ok(ColorPipelineProgram {
            device: Arc::clone(&self.inner),
            extent,
            stages,
        })
    }

    /// Check every requested output color stage without allocating native data.
    pub fn check_output_color(&self, extent: Extent, color: OutputColor<'_>) -> io::Result<()> {
        if let Some(table) = color.degamma {
            super::gamma::check_support(&self.inner, table.entries().len(), extent)?;
        }
        if color.matrix.is_some() {
            super::matrix::check_support(&self.inner, extent)?;
        }
        if let Some(table) = color.gamma {
            super::gamma::check_support(&self.inner, table.entries().len(), extent)?;
        }
        Ok(())
    }

    /// Prepare every requested output color stage in display order.
    pub fn create_output_color(
        &self,
        extent: Extent,
        color: OutputColor<'_>,
    ) -> io::Result<OutputColorProgram> {
        self.check_output_color(extent, color)?;
        let mut stages = Vec::new();
        stages.try_reserve_exact(3).map_err(io::Error::other)?;
        if let Some(table) = color.degamma {
            stages.push(ColorStage::Lookup(self.create_gamma(table.entries())?));
        }
        if let Some(matrix) = color.matrix {
            stages.push(ColorStage::Matrix(self.create_output_matrix(matrix)?));
        }
        if let Some(table) = color.gamma {
            stages.push(ColorStage::Lookup(self.create_gamma(table.entries())?));
        }
        Ok(OutputColorProgram {
            pipeline: ColorPipelineProgram {
                device: Arc::clone(&self.inner),
                extent,
                stages,
            },
        })
    }
}

impl ColorPipelineProgram {
    /// Apply every non-bypass operation to one matching private image.
    pub fn apply_and_wait(&self, mut image: PrivateImage) -> io::Result<PrivateImage> {
        check_image(&image, &self.device, self.extent)?;
        for stage in &self.stages {
            image = match stage {
                ColorStage::Lookup(stage) => stage.apply_waited(image)?,
                ColorStage::Matrix(stage) => stage.apply_waited(image)?,
                ColorStage::Transfer(stage) => stage.apply_and_wait(image)?,
            };
        }
        Ok(image)
    }
}

impl OutputColorProgram {
    /// Apply degamma, matrix and gamma to a completed private image.
    ///
    /// The image must match the output dimensions and be initialized on the
    /// program's logical device. Every requested stage is prepared during
    /// construction, so an unsupported pipeline fails before any image is
    /// consumed. Execution waits for each native stage in turn. Errors return
    /// no image for reuse.
    pub fn apply_and_wait(&self, image: PrivateImage) -> io::Result<PrivateImage> {
        self.pipeline.apply_and_wait(image)
    }
}

fn check_image(
    image: &PrivateImage,
    device: &Arc<super::super::device::DeviceInner>,
    extent: Extent,
) -> io::Result<()> {
    if !image.initialized
        || !Arc::ptr_eq(&image.device, device)
        || image.width.get() != extent.width()
        || image.height.get() != extent.height()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "color pipeline needs a matching initialized image on its program's device",
        ));
    }
    Ok(())
}
