//! Ordered output color processing for a completed private scene.

use std::io;
use std::sync::Arc;

use drm_display_executor::scene::{color::OutputColor, geometry::Extent};

use super::{Gamma, OutputMatrix, PrivateImage};
use crate::vulkan::Device;

/// Immutable native implementation of a complete output color pipeline.
#[derive(Clone)]
pub struct OutputColorProgram {
    device: Arc<super::super::device::DeviceInner>,
    extent: Extent,
    degamma: Option<Gamma>,
    matrix: Option<OutputMatrix>,
    gamma: Option<Gamma>,
}

impl Device {
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
        Ok(OutputColorProgram {
            device: Arc::clone(&self.inner),
            extent,
            degamma: color
                .degamma
                .map(|table| self.create_gamma(table.entries()))
                .transpose()?,
            matrix: color
                .matrix
                .map(|matrix| self.create_output_matrix(matrix))
                .transpose()?,
            gamma: color
                .gamma
                .map(|table| self.create_gamma(table.entries()))
                .transpose()?,
        })
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
    pub fn apply_waited(&self, mut image: PrivateImage) -> io::Result<PrivateImage> {
        if !image.initialized
            || !Arc::ptr_eq(&image.device, &self.device)
            || image.width.get() != self.extent.width()
            || image.height.get() != self.extent.height()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output color needs a matching initialized image on its program's device",
            ));
        }
        if let Some(degamma) = &self.degamma {
            image = degamma.apply_waited(image)?;
        }
        if let Some(matrix) = &self.matrix {
            image = matrix.apply_waited(image)?;
        }
        if let Some(gamma) = &self.gamma {
            image = gamma.apply_waited(image)?;
        }
        Ok(image)
    }
}
