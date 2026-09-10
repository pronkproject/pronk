//! Optional native graphics backend, independent of transport and capture policy.

mod clear;
mod device;
mod image;
mod submission;

pub use device::Device;
pub use image::{Image, ImageLayout};
