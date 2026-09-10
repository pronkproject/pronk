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
}

/// Post-composition color selection; identity unless a gamma table is supplied.
///
/// No degamma, matrix, transfer-function inference or per-plane color operations
/// are implied. The table operates after blending, before output byte encoding.
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputColor<'a> {
    pub gamma: Option<Lut<'a>>,
}

impl OutputColor<'_> {
    pub fn apply(self, input: [u16; 3]) -> [u16; 3] {
        self.gamma.map_or(input, |table| table.sample(input))
    }
}
