//! Explicit normalized lookup tables, independent of native color properties.

/// Invalid storage for the reference lookup-table profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LutError {
    Empty,
    TooLarge,
}

impl std::fmt::Display for LutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "a color lookup table must contain an entry",
            Self::TooLarge => "a color lookup table exceeds 65536 entries",
        })
    }
}

impl std::error::Error for LutError {}

/// Borrowed RGB entries spanning the normalized input domain uniformly.
///
/// One entry is a constant function, not identity. There may be at most 65536
/// entries. Sampling linearly interpolates each channel with exact rational
/// position and rounds once to the nearest u16. Half steps round upward.
/// This defines reference arithmetic, not a hardware LUT precision claim.
#[derive(Clone, Copy, Debug)]
pub struct Lut<'a> {
    entries: &'a [[u16; 3]],
}

impl<'a> Lut<'a> {
    pub fn new(entries: &'a [[u16; 3]]) -> Result<Self, LutError> {
        match entries.len() {
            0 => Err(LutError::Empty),
            1..=65536 => Ok(Self { entries }),
            _ => Err(LutError::TooLarge),
        }
    }

    pub fn sample(self, input: [u16; 3]) -> [u16; 3] {
        const MAX: u64 = u16::MAX as u64;
        std::array::from_fn(|channel| {
            let position = u64::from(input[channel]) * (self.entries.len() as u64 - 1);
            let index = (position / MAX) as usize;
            let fraction = position % MAX;
            let next = (index + 1).min(self.entries.len() - 1);
            let a = u64::from(self.entries[index][channel]);
            let b = u64::from(self.entries[next][channel]);
            ((a * (MAX - fraction) + b * fraction + MAX / 2) / MAX) as u16
        })
    }

    /// Return the uniformly spaced entries in native channel order.
    pub const fn entries(self) -> &'a [[u16; 3]] {
        self.entries
    }

    fn sample_extended(self, input: [i32; 3]) -> [i32; 3] {
        self.sample(input.map(|value| value.clamp(0, i32::from(u16::MAX)) as u16))
            .map(i32::from)
    }
}

/// A three-by-four matrix of DRM S31.32 sign-magnitude coefficients.
///
/// Each row transforms RGB and adds its fourth coefficient in normalized channel
/// units. Results retain signed extended range until their pipeline boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorMatrix {
    coefficients: [u64; 12],
}

impl ColorMatrix {
    pub const fn from_sign_magnitude(coefficients: [u64; 12]) -> Self {
        Self { coefficients }
    }

    pub const fn sign_magnitude(self) -> [u64; 12] {
        self.coefficients
    }

    fn apply(self, input: [i32; 3]) -> [i32; 3] {
        let signed = |raw: u64| {
            let magnitude = i128::from(raw & !(1 << 63));
            if raw >> 63 == 0 {
                magnitude
            } else {
                -magnitude
            }
        };
        std::array::from_fn(|row| {
            let mut sum = signed(self.coefficients[row * 4 + 3]);
            for (channel, input) in input.iter().enumerate() {
                sum += signed(self.coefficients[row * 4 + channel]) * i128::from(*input);
            }
            // The wide accumulator covers all coefficients and i32 inputs.
            // Saturation preserves a defined extended range for hostile values.
            ((sum + (1 << 31)) >> 32).clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
        })
    }
}

/// Post-composition degamma, matrix and gamma before output quantization.
///
/// Lookup tables clamp their input. The matrix uses signed extended-range
/// arithmetic; its result is clamped before the final table or output.
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputColor<'a> {
    pub degamma: Option<Lut<'a>>,
    pub matrix: Option<ColorMatrix>,
    pub gamma: Option<Lut<'a>>,
}

impl OutputColor<'_> {
    pub fn apply(self, input: [u16; 3]) -> [u16; 3] {
        let mut channels = input.map(i32::from);
        if let Some(degamma) = self.degamma {
            channels = degamma.sample_extended(channels);
        }
        if let Some(matrix) = self.matrix {
            channels = matrix.apply(channels);
        }
        channels = channels.map(|value| value.clamp(0, i32::from(u16::MAX)));
        if let Some(gamma) = self.gamma {
            channels = gamma.sample_extended(channels);
        }
        channels.map(|value| value as u16)
    }
}
