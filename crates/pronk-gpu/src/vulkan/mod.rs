//! Optional native graphics backend, independent of transport and capture policy.

mod clear;
mod copy;
mod device;
mod image;
mod submission;

#[cfg(test)]
mod test_support;

pub use copy::CopiedImages;
pub use device::Device;
pub use image::{Image, ImageLayout};
