//! Ordered output color processing for a completed private scene.

use std::io;
use std::sync::Arc;

use drm_display_executor::scene::color::OutputColor;

use super::{Gamma, OutputMatrix, PrivateImage};
use crate::vulkan::Device;

/// Immutable native implementation of a complete output color pipeline.
#[derive(Clone)]
pub struct OutputColorProgram {
    device: Arc<super::super::device::DeviceInner>,
    degamma: Option<Gamma>,
    matrix: Option<OutputMatrix>,
    gamma: Option<Gamma>,
}

impl Device {
    /// Prepare every requested output color stage in display order.
    pub fn create_output_color(&self, color: OutputColor<'_>) -> io::Result<OutputColorProgram> {
        Ok(OutputColorProgram {
            device: Arc::clone(&self.inner),
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
    /// The image must be initialized on the program's logical device. Every
    /// requested stage is prepared during construction, so an unsupported
    /// pipeline fails before any image is consumed. Execution waits for each
    /// native stage in turn. Errors return no image for reuse.
    pub fn apply_waited(&self, mut image: PrivateImage) -> io::Result<PrivateImage> {
        if !image.initialized || !Arc::ptr_eq(&image.device, &self.device) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output color needs an initialized private image on its program's device",
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
