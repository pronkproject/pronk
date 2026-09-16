//! Portable RGB output, separate from scene decoding and composition.

use std::io::{self, Write};

impl crate::Rendered {
    /// Write a binary PPM image. The renderer's opaque alpha is omitted.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    pub fn write_ppm(&self, output: &mut impl Write) -> io::Result<()> {
        write!(output, "P6\n{} {}\n255\n", self.width, self.height)?;
        for pixel in self.pixels.chunks_exact(4) {
            output.write_all(&pixel[..3])?;
        }
        Ok(())
    }
}
