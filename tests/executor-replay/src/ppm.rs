//! Portable RGB output, separate from scene decoding and composition.

use std::io::{self, Write};

impl crate::Rendered {
    /// Write a binary PPM image. The renderer's opaque alpha is omitted.
    pub fn write_ppm(&self, output: &mut impl Write) -> io::Result<()> {
        write!(output, "P6\n{} {}\n255\n", self.width, self.height)?;
        for offset in (0..self.pixels.len()).step_by(4) {
            output.write_all(&self.pixels[offset..offset + 3])?;
        }
        Ok(())
    }
}
