//! Deterministic EDID construction for one explicitly selected cast display.

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::castkms::{EdidError, ValidatedEdid, EDID_BLOCK_SIZE};
use crate::identity::{normalize_manufacturer_name, PnpId};

pub const EDID_PRODUCT_NAME_MAX_BYTES: usize = 106;
pub const MAX_INITIAL_EDID_MODES: usize = 4;

const DISPLAYID_EXTENSION_TAG: u8 = 0x70;
const DISPLAYID_VERSION_1_3: u8 = 0x13;
const DISPLAYID_PRODUCT_TYPE_TV: u8 = 4;
const DISPLAYID_PRODUCT_IDENTIFICATION_TAG: u8 = 0x00;
const CTA_EXTENSION_TAG: u8 = 0x02;
const CTA_REVISION_3: u8 = 0x03;
const DISPLAYID_MAX_SECTION_BYTES: usize = 121;
const DISPLAYID_PARAMETERS_TAG: u8 = 0x01;
const DISPLAYID_TYPE_I_TIMING_TAG: u8 = 0x03;

const PRODUCT_CODE_DOMAIN: &[u8] = b"io.github.pronkproject.Pronk.edid-product-code.v1\0";
const SERIAL_DOMAIN: &[u8] = b"io.github.pronkproject.Pronk.edid-serial.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EdidMode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

impl EdidMode {
    pub fn new(
        width: u32,
        height: u32,
        refresh_millihz: u32,
    ) -> Result<Self, CastDisplayEdidError> {
        let mode = Self {
            width,
            height,
            refresh_millihz,
        };
        timing_for(mode).ok_or(CastDisplayEdidError::UnsupportedMode(mode))?;
        Ok(mode)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastDisplayEdidRequest {
    pub pnp_id: PnpId,
    pub manufacturer_name: Option<String>,
    /// Stable product identity used to derive the numeric EDID product code.
    pub product_name: Option<String>,
    /// User-visible name advertised by the DisplayID product block.
    pub display_name: Option<String>,
    pub backend_id: String,
    pub device_id: String,
    /// Preferred mode first, followed by at most three fallback modes.
    pub modes: Vec<EdidMode>,
    pub audio: bool,
    /// HDMI-CEC physical address advertised through the HDMI VSDB.
    pub cec_physical_address: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdidNumericIdentity {
    pub pnp_id: PnpId,
    pub product_code: u16,
    pub serial: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedCastDisplayEdid {
    identity: EdidNumericIdentity,
    display_name: Option<String>,
    modes: Vec<EdidMode>,
    audio: bool,
    cec_physical_address: Option<u16>,
    edid: ValidatedEdid,
}

impl GeneratedCastDisplayEdid {
    pub fn identity(&self) -> EdidNumericIdentity {
        self.identity
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    pub fn edid(&self) -> &ValidatedEdid {
        &self.edid
    }

    /// Whether two generated EDIDs describe the same monitor configuration,
    /// allowing only their presentation names to differ.
    pub fn has_same_monitor_configuration(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.modes == other.modes
            && self.audio == other.audio
            && self.cec_physical_address == other.cec_physical_address
    }

    pub fn into_edid(self) -> ValidatedEdid {
        self.edid
    }
}

pub fn build_cast_display_edid(
    request: CastDisplayEdidRequest,
) -> Result<GeneratedCastDisplayEdid, CastDisplayEdidError> {
    validate_stable_id("backend ID", &request.backend_id, 64)?;
    validate_stable_id("device ID", &request.device_id, 256)?;
    let manufacturer_name = validate_manufacturer_name(request.manufacturer_name.as_deref())?;
    let product_name = validate_product_name(request.product_name.as_deref())?;
    let display_name = validate_product_name(request.display_name.as_deref())?;
    validate_modes(&request.modes)?;
    if let Some(address) = request.cec_physical_address {
        validate_cec_physical_address(address)?;
    }

    let manufacturer_key = manufacturer_name
        .as_deref()
        .map(normalize_manufacturer_name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| request.pnp_id.as_str().to_ascii_lowercase());
    let product_key = product_name
        .as_deref()
        .map(normalize_hash_field)
        .unwrap_or_default();
    let product_code = nonzero_u16(derive_hash(
        PRODUCT_CODE_DOMAIN,
        &[manufacturer_key.as_bytes(), product_key.as_bytes()],
    ));
    let serial = nonzero_u32(derive_hash(
        SERIAL_DOMAIN,
        &[request.backend_id.as_bytes(), request.device_id.as_bytes()],
    ));
    let identity = EdidNumericIdentity {
        pnp_id: request.pnp_id,
        product_code,
        serial,
    };

    let product_block = displayid_product_block(identity, display_name.as_deref());
    let parameters_block = displayid_parameters_block(request.modes[0], request.audio);
    let timing_block = displayid_type_i_timing_block(request.modes[0]);
    let required_length = parameters_block.len() + timing_block.len();
    let needs_continuation = product_block.len() + required_length > DISPLAYID_MAX_SECTION_BYTES;
    let displayid_block_count = if needs_continuation { 2 } else { 1 };
    let total_block_count = 1 + displayid_block_count + 1;
    let mut bytes = vec![0_u8; total_block_count * EDID_BLOCK_SIZE];
    write_base_block(
        &mut bytes[..EDID_BLOCK_SIZE],
        identity,
        &request.modes,
        (total_block_count - 1) as u8,
    );

    // An HDMI VSDB requires CTA to be the first non-Block-Map extension.
    // Keep CTA first for every generated display so enabling CEC does not
    // introduce a second extension layout.
    write_cta_extension(
        &mut bytes[EDID_BLOCK_SIZE..EDID_BLOCK_SIZE * 2],
        &request.modes,
        request.audio,
        request.cec_physical_address,
    );

    let first_displayid = &mut bytes[EDID_BLOCK_SIZE * 2..EDID_BLOCK_SIZE * 3];
    if needs_continuation {
        write_displayid_extension(
            first_displayid,
            DISPLAYID_PRODUCT_TYPE_TV,
            1,
            &product_block,
        );
        let mut continuation_data = parameters_block;
        continuation_data.extend_from_slice(&timing_block);
        write_displayid_extension(
            &mut bytes[EDID_BLOCK_SIZE * 3..EDID_BLOCK_SIZE * 4],
            0,
            0,
            &continuation_data,
        );
    } else {
        let mut displayid_data = product_block;
        displayid_data.extend_from_slice(&parameters_block);
        displayid_data.extend_from_slice(&timing_block);
        write_displayid_extension(
            first_displayid,
            DISPLAYID_PRODUCT_TYPE_TV,
            0,
            &displayid_data,
        );
    }
    set_block_checksums(&mut bytes);
    let edid = ValidatedEdid::new(bytes).map_err(CastDisplayEdidError::Framing)?;

    Ok(GeneratedCastDisplayEdid {
        identity,
        display_name,
        modes: request.modes,
        audio: request.audio,
        cec_physical_address: request.cec_physical_address,
        edid,
    })
}

fn validate_stable_id(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), CastDisplayEdidError> {
    if value.is_empty() {
        return Err(CastDisplayEdidError::Empty { field });
    }
    if value.len() > maximum {
        return Err(CastDisplayEdidError::TooLong {
            field,
            actual: value.len(),
            maximum,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(CastDisplayEdidError::ControlCharacter { field });
    }
    Ok(())
}

fn validate_manufacturer_name(value: Option<&str>) -> Result<Option<String>, CastDisplayEdidError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 128 {
        return Err(CastDisplayEdidError::TooLong {
            field: "manufacturer name",
            actual: value.len(),
            maximum: 128,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(CastDisplayEdidError::ControlCharacter {
            field: "manufacturer name",
        });
    }
    Ok(Some(value.into()))
}

fn validate_product_name(value: Option<&str>) -> Result<Option<String>, CastDisplayEdidError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > EDID_PRODUCT_NAME_MAX_BYTES {
        return Err(CastDisplayEdidError::TooLong {
            field: "product name",
            actual: value.len(),
            maximum: EDID_PRODUCT_NAME_MAX_BYTES,
        });
    }
    if !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(CastDisplayEdidError::NonPrintableProductName);
    }
    Ok(Some(value.into()))
}

fn validate_modes(modes: &[EdidMode]) -> Result<(), CastDisplayEdidError> {
    if modes.is_empty() {
        return Err(CastDisplayEdidError::NoModes);
    }
    if modes.len() > MAX_INITIAL_EDID_MODES {
        return Err(CastDisplayEdidError::TooManyModes(modes.len()));
    }
    let mut unique = HashSet::with_capacity(modes.len());
    for mode in modes {
        if timing_for(*mode).is_none() {
            return Err(CastDisplayEdidError::UnsupportedMode(*mode));
        }
        if !unique.insert(*mode) {
            return Err(CastDisplayEdidError::DuplicateMode(*mode));
        }
    }
    if !modes
        .iter()
        .any(|mode| mode.width == 640 && mode.height == 480 && mode.refresh_millihz == 60_000)
    {
        return Err(CastDisplayEdidError::MissingRequired640x480);
    }
    Ok(())
}

fn validate_cec_physical_address(address: u16) -> Result<(), CastDisplayEdidError> {
    if address == 0 || address == u16::MAX {
        return Err(CastDisplayEdidError::InvalidCecPhysicalAddress(address));
    }
    let nibbles = [
        (address >> 12) as u8,
        ((address >> 8) & 0x0f) as u8,
        ((address >> 4) & 0x0f) as u8,
        (address & 0x0f) as u8,
    ];
    let mut zero_seen = false;
    for nibble in nibbles {
        if nibble == 0 {
            zero_seen = true;
        } else if zero_seen {
            return Err(CastDisplayEdidError::InvalidCecPhysicalAddress(address));
        }
    }
    Ok(())
}

fn normalize_hash_field(value: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in value.nfkc().flat_map(char::to_lowercase) {
        if character.is_whitespace() {
            pending_space = !result.is_empty();
        } else {
            if pending_space {
                result.push(' ');
                pending_space = false;
            }
            result.push(character);
        }
    }
    result
}

fn derive_hash(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for field in fields {
        hasher.update((field.len() as u32).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

fn nonzero_u16(hash: [u8; 32]) -> u16 {
    match u16::from_le_bytes(hash[..2].try_into().expect("hash prefix has two bytes")) {
        0 => 1,
        value => value,
    }
}

fn nonzero_u32(hash: [u8; 32]) -> u32 {
    match u32::from_le_bytes(hash[..4].try_into().expect("hash prefix has four bytes")) {
        0 => 1,
        value => value,
    }
}

fn write_base_block(
    block: &mut [u8],
    identity: EdidNumericIdentity,
    modes: &[EdidMode],
    extension_count: u8,
) {
    debug_assert_eq!(block.len(), EDID_BLOCK_SIZE);
    block[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    block[8..10].copy_from_slice(&encode_pnp_id(identity.pnp_id));
    block[10..12].copy_from_slice(&identity.product_code.to_le_bytes());
    block[12..16].copy_from_slice(&identity.serial.to_le_bytes());
    // Unspecified week, stable model year 2020. EDID has no fully unspecified
    // base-block year representation, and this field is not device identity.
    block[16] = 0;
    block[17] = 30;
    block[18] = 1;
    // EDID 1.4 lets the variable-length DisplayID product name be the only
    // product-name field and permits this bounded multi-extension layout.
    block[19] = 4;
    block[20] = 0x80; // Digital input, undefined bit depth/interface.
    block[23] = 120; // Gamma 2.20.
    block[24] = 0x0e; // sRGB, RGB, preferred timing in the first descriptor.
    block[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    for timing in block[38..54].chunks_exact_mut(2) {
        timing.copy_from_slice(&[0x01, 0x01]);
    }

    for (index, descriptor) in block[54..126].chunks_exact_mut(18).enumerate() {
        if let Some(mode) = modes.get(index) {
            write_detailed_timing(descriptor, timing_for(*mode).expect("modes were validated"));
        } else {
            descriptor[3] = 0x10; // Dummy descriptor, never product name (0xfc).
        }
    }
    block[126] = extension_count;
}

fn displayid_product_block(identity: EdidNumericIdentity, product_name: Option<&str>) -> Vec<u8> {
    let product_name = product_name.unwrap_or_default().as_bytes();
    let product_payload_length = 12 + product_name.len();
    let mut block = vec![0_u8; 3 + product_payload_length];
    block[0] = DISPLAYID_PRODUCT_IDENTIFICATION_TAG;
    block[1] = 0;
    block[2] = product_payload_length as u8;
    block[3..6].copy_from_slice(&identity.pnp_id.bytes());
    block[6..8].copy_from_slice(&identity.product_code.to_le_bytes());
    block[8..12].copy_from_slice(&identity.serial.to_le_bytes());
    block[12] = 0; // Unspecified manufacture week.
    block[13] = 0; // Unspecified manufacture year.
    block[14] = product_name.len() as u8;
    block[15..].copy_from_slice(product_name);
    block
}

fn displayid_parameters_block(preferred: EdidMode, audio: bool) -> Vec<u8> {
    let mut block = vec![0_u8; 15];
    block[0] = DISPLAYID_PARAMETERS_TAG;
    block[1] = 0;
    block[2] = 12;
    // Image dimensions are variable/unspecified for a network display.
    block[7..9].copy_from_slice(&(preferred.width as u16).to_le_bytes());
    block[9..11].copy_from_slice(&(preferred.height as u16).to_le_bytes());
    if audio {
        block[11] = 0x80;
    }
    block[12] = 120; // Gamma 2.20.
    block[13] = aspect_ratio_value(preferred);
    block[14] = 0x77; // Native and overall dynamic range are both 8 bpc.
    block
}

fn displayid_type_i_timing_block(preferred: EdidMode) -> Vec<u8> {
    let timing = timing_for(preferred).expect("modes were validated");
    let mut block = vec![0_u8; 23];
    block[0] = DISPLAYID_TYPE_I_TIMING_TAG;
    block[1] = 1;
    block[2] = 20;
    let timing_bytes = &mut block[3..];
    let clock = u32::from(timing.clock_10khz) - 1;
    timing_bytes[0..3].copy_from_slice(&clock.to_le_bytes()[..3]);
    timing_bytes[3] = 0x80 | aspect_ratio_code(preferred);
    write_minus_one_u16(&mut timing_bytes[4..6], timing.hactive);
    write_minus_one_u16(&mut timing_bytes[6..8], timing.hblank);
    write_minus_one_sync(
        &mut timing_bytes[8..10],
        timing.hfront,
        timing.features & 0x02 != 0,
    );
    write_minus_one_u16(&mut timing_bytes[10..12], timing.hsync);
    write_minus_one_u16(&mut timing_bytes[12..14], timing.vactive);
    write_minus_one_u16(&mut timing_bytes[14..16], timing.vblank);
    write_minus_one_sync(
        &mut timing_bytes[16..18],
        u16::from(timing.vfront),
        timing.features & 0x04 != 0,
    );
    write_minus_one_u16(&mut timing_bytes[18..20], u16::from(timing.vsync));
    block
}

fn write_displayid_extension(block: &mut [u8], product_type: u8, extension_count: u8, data: &[u8]) {
    debug_assert_eq!(block.len(), EDID_BLOCK_SIZE);
    debug_assert!(data.len() <= DISPLAYID_MAX_SECTION_BYTES);

    block[0] = DISPLAYID_EXTENSION_TAG;
    block[1] = DISPLAYID_VERSION_1_3;
    block[2] = data.len() as u8;
    block[3] = product_type;
    block[4] = extension_count;
    block[5..5 + data.len()].copy_from_slice(data);

    let structure_checksum_index = 5 + data.len();
    block[structure_checksum_index] = checksum(&block[1..structure_checksum_index]);
}

fn write_cta_extension(
    block: &mut [u8],
    modes: &[EdidMode],
    audio: bool,
    cec_physical_address: Option<u16>,
) {
    debug_assert_eq!(block.len(), EDID_BLOCK_SIZE);
    block[0] = CTA_EXTENSION_TAG;
    block[1] = CTA_REVISION_3;
    let mut position = 4;
    block[position] = 0x40 | modes.len() as u8; // Video data block.
    position += 1;
    for mode in modes {
        let vic = video_identification_code(*mode).expect("modes were validated");
        block[position] = vic;
        position += 1;
    }
    // Video Capability Data Block with selectable RGB quantization.
    block[position..position + 3].copy_from_slice(&[
        0xe2, // Extended data block, two payload bytes.
        0x00, // Video Capability Data Block.
        0x4a, // Selectable RGB; IT and CE are always underscanned.
    ]);
    position += 3;
    if let Some(address) = cec_physical_address {
        block[position..position + 6].copy_from_slice(&[
            0x65, // Vendor-specific data block, five payload bytes.
            0x03,
            0x0c,
            0x00, // HDMI Licensing, LLC OUI in CTA byte order.
            (address >> 8) as u8,
            address as u8,
        ]);
        position += 6;
    }
    if audio {
        block[position..position + 4].copy_from_slice(&[
            0x23, // Audio data block, one three-byte SAD.
            0x09, // LPCM, two channels.
            0x04, // 48 kHz.
            0x01, // 16-bit samples.
        ]);
        position += 4;
        block[position..position + 4].copy_from_slice(&[
            0x83, // Speaker allocation data block.
            0x01, // Front left/right.
            0x00, 0x00,
        ]);
        position += 4;
        block[3] |= 0x40; // Basic audio.
    }
    block[3] |= 0x81; // Underscan IT formats; one native CTA DTD.
    block[2] = position as u8;
    write_detailed_timing(
        &mut block[position..position + 18],
        timing_for(modes[0]).expect("modes were validated"),
    );
}

fn aspect_ratio_code(mode: EdidMode) -> u8 {
    match (mode.width, mode.height) {
        (640, 480) => 2,
        _ => 4,
    }
}

fn aspect_ratio_value(mode: EdidMode) -> u8 {
    match (mode.width, mode.height) {
        (640, 480) => 33,
        _ => 78,
    }
}

fn write_minus_one_u16(output: &mut [u8], value: u16) {
    output.copy_from_slice(&(value - 1).to_le_bytes());
}

fn write_minus_one_sync(output: &mut [u8], value: u16, positive: bool) {
    let value = value - 1;
    output[0] = value as u8;
    output[1] = ((value >> 8) as u8 & 0x7f) | if positive { 0x80 } else { 0 };
}

fn set_block_checksums(edid: &mut [u8]) {
    for block in edid.chunks_exact_mut(EDID_BLOCK_SIZE) {
        block[EDID_BLOCK_SIZE - 1] = checksum(&block[..EDID_BLOCK_SIZE - 1]);
    }
}

fn checksum(bytes: &[u8]) -> u8 {
    0_u8.wrapping_sub(bytes.iter().copied().fold(0_u8, u8::wrapping_add))
}

fn encode_pnp_id(pnp_id: PnpId) -> [u8; 2] {
    let bytes = pnp_id.bytes();
    let value = (u16::from(bytes[0] - b'A' + 1) << 10)
        | (u16::from(bytes[1] - b'A' + 1) << 5)
        | u16::from(bytes[2] - b'A' + 1);
    value.to_be_bytes()
}

#[derive(Debug, Clone, Copy)]
struct DetailedTiming {
    clock_10khz: u16,
    hactive: u16,
    hblank: u16,
    vactive: u16,
    vblank: u16,
    hfront: u16,
    hsync: u16,
    vfront: u8,
    vsync: u8,
    features: u8,
}

fn timing_for(mode: EdidMode) -> Option<DetailedTiming> {
    let timing = match (mode.width, mode.height, mode.refresh_millihz) {
        (640, 480, 60_000) => DetailedTiming {
            clock_10khz: 2_518,
            hactive: 640,
            hblank: 160,
            vactive: 480,
            vblank: 45,
            hfront: 16,
            hsync: 96,
            vfront: 10,
            vsync: 2,
            features: 0x18,
        },
        (1280, 720, 60_000) => DetailedTiming {
            clock_10khz: 7_425,
            hactive: 1280,
            hblank: 370,
            vactive: 720,
            vblank: 30,
            hfront: 110,
            hsync: 40,
            vfront: 5,
            vsync: 5,
            features: 0x1e,
        },
        (1920, 1080, 60_000) => DetailedTiming {
            clock_10khz: 14_850,
            hactive: 1920,
            hblank: 280,
            vactive: 1080,
            vblank: 45,
            hfront: 88,
            hsync: 44,
            vfront: 4,
            vsync: 5,
            features: 0x1e,
        },
        (3840, 2160, 30_000) => DetailedTiming {
            clock_10khz: 29_700,
            hactive: 3840,
            hblank: 560,
            vactive: 2160,
            vblank: 90,
            hfront: 176,
            hsync: 88,
            vfront: 8,
            vsync: 10,
            features: 0x1e,
        },
        (3840, 2160, 60_000) => DetailedTiming {
            clock_10khz: 59_400,
            hactive: 3840,
            hblank: 560,
            vactive: 2160,
            vblank: 90,
            hfront: 176,
            hsync: 88,
            vfront: 8,
            vsync: 10,
            features: 0x1e,
        },
        _ => return None,
    };
    Some(timing)
}

fn video_identification_code(mode: EdidMode) -> Option<u8> {
    match (mode.width, mode.height, mode.refresh_millihz) {
        (640, 480, 60_000) => Some(1),
        (1280, 720, 60_000) => Some(4),
        (1920, 1080, 60_000) => Some(16),
        (3840, 2160, 30_000) => Some(95),
        (3840, 2160, 60_000) => Some(97),
        _ => None,
    }
}

fn write_detailed_timing(descriptor: &mut [u8], timing: DetailedTiming) {
    descriptor[0..2].copy_from_slice(&timing.clock_10khz.to_le_bytes());
    descriptor[2] = timing.hactive as u8;
    descriptor[3] = timing.hblank as u8;
    descriptor[4] = ((timing.hactive >> 4) as u8 & 0xf0) | ((timing.hblank >> 8) as u8 & 0x0f);
    descriptor[5] = timing.vactive as u8;
    descriptor[6] = timing.vblank as u8;
    descriptor[7] = ((timing.vactive >> 4) as u8 & 0xf0) | ((timing.vblank >> 8) as u8 & 0x0f);
    descriptor[8] = timing.hfront as u8;
    descriptor[9] = timing.hsync as u8;
    descriptor[10] = ((timing.vfront & 0x0f) << 4) | (timing.vsync & 0x0f);
    descriptor[11] = (((timing.hfront >> 8) as u8 & 0x03) << 6)
        | (((timing.hsync >> 8) as u8 & 0x03) << 4)
        | ((timing.vfront >> 4) & 0x03) << 2
        | ((timing.vsync >> 4) & 0x03);
    descriptor[17] = timing.features;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CastDisplayEdidError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} is {actual} bytes; limit is {maximum}")]
    TooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} contains a control character")]
    ControlCharacter { field: &'static str },
    #[error("product name must contain only printable ASCII")]
    NonPrintableProductName,
    #[error("EDID requires at least one mode")]
    NoModes,
    #[error("EDID has {0} modes; limit is {MAX_INITIAL_EDID_MODES}")]
    TooManyModes(usize),
    #[error("CEC physical address 0x{0:04x} is not a valid non-root topology address")]
    InvalidCecPhysicalAddress(u16),
    #[error("EDID mode {0:?} is outside the conservative timing set")]
    UnsupportedMode(EdidMode),
    #[error("EDID mode {0:?} is duplicated")]
    DuplicateMode(EdidMode),
    #[error("CTA-861 requires 640x480 at 60 Hz in the negotiated mode set")]
    MissingRequired640x480,
    #[error("generated EDID framing is invalid: {0}")]
    Framing(EdidError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(product_name: Option<String>) -> CastDisplayEdidRequest {
        CastDisplayEdidRequest {
            pnp_id: PnpId::parse("GGL").unwrap(),
            manufacturer_name: Some("Google, Inc.".into()),
            display_name: product_name.clone(),
            product_name,
            backend_id: "chromiacast".into(),
            device_id: "device-1234".into(),
            modes: vec![
                EdidMode::new(1920, 1080, 60_000).unwrap(),
                EdidMode::new(640, 480, 60_000).unwrap(),
            ],
            audio: true,
            cec_physical_address: None,
        }
    }

    fn displayid_product(edid: &[u8]) -> (&[u8], u16, u32, &[u8]) {
        let block = edid
            .chunks_exact(EDID_BLOCK_SIZE)
            .skip(1)
            .find(|block| block[0] == DISPLAYID_EXTENSION_TAG && block[5] == 0)
            .expect("EDID has a DisplayID product block");
        assert_eq!(block[5], DISPLAYID_PRODUCT_IDENTIFICATION_TAG);
        let name_length = usize::from(block[19]);
        (
            &block[8..11],
            u16::from_le_bytes(block[11..13].try_into().unwrap()),
            u32::from_le_bytes(block[13..17].try_into().unwrap()),
            &block[20..20 + name_length],
        )
    }

    #[test]
    fn embeds_long_product_identity_without_a_base_name_descriptor() {
        let product = "Chromecast with Google TV Living Room";
        let generated = build_cast_display_edid(request(Some(product.into()))).unwrap();
        let bytes = generated.edid().as_bytes();
        assert_eq!(bytes.len(), EDID_BLOCK_SIZE * 3);
        assert!(bytes.chunks_exact(EDID_BLOCK_SIZE).all(|block| block
            .iter()
            .copied()
            .fold(0_u8, u8::wrapping_add)
            == 0));
        assert!(bytes[54..126]
            .chunks_exact(18)
            .filter(|descriptor| descriptor[0] == 0 && descriptor[1] == 0)
            .all(|descriptor| descriptor[3] != 0xfc));

        let (pnp, product_code, serial, name) = displayid_product(bytes);
        assert_eq!(pnp, b"GGL");
        assert_eq!(product_code, generated.identity().product_code);
        assert_eq!(serial, generated.identity().serial);
        assert_eq!(name, product.as_bytes());
        assert_eq!(&bytes[8..10], &encode_pnp_id(generated.identity().pnp_id));
        assert_eq!(
            u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
            product_code
        );
        assert_eq!(
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            serial
        );
        assert_eq!(bytes[126], 2);
    }

    #[test]
    fn accepts_the_exact_displayid_name_capacity() {
        let maximum = "X".repeat(EDID_PRODUCT_NAME_MAX_BYTES);
        let generated = build_cast_display_edid(request(Some(maximum.clone()))).unwrap();
        assert_eq!(generated.display_name(), Some(maximum.as_str()));
        let block = generated
            .edid()
            .as_bytes()
            .chunks_exact(EDID_BLOCK_SIZE)
            .skip(1)
            .find(|block| block[0] == DISPLAYID_EXTENSION_TAG && block[5] == 0)
            .expect("EDID has a DisplayID product block");
        assert_eq!(block[2], 121);
        assert_eq!(block[7], 118);
        assert_eq!(block[19], EDID_PRODUCT_NAME_MAX_BYTES as u8);
        assert_eq!(&block[20..126], maximum.as_bytes());
        assert_eq!(generated.edid().len(), EDID_BLOCK_SIZE * 4);
        assert_eq!(generated.edid().as_bytes()[126], 3);
    }

    #[test]
    fn trims_but_does_not_rewrite_a_printable_product_name() {
        let generated =
            build_cast_display_edid(request(Some("  Model  X, Inc.  ".into()))).unwrap();
        assert_eq!(generated.display_name(), Some("Model  X, Inc."));
        assert!(matches!(
            build_cast_display_edid(request(Some("bad\nname".into()))),
            Err(CastDisplayEdidError::NonPrintableProductName)
        ));
        assert!(matches!(
            build_cast_display_edid(request(Some("X".repeat(107)))),
            Err(CastDisplayEdidError::TooLong {
                field: "product name",
                ..
            })
        ));
    }

    #[test]
    fn serial_is_device_stable_and_product_code_tracks_make_model() {
        let first = build_cast_display_edid(request(Some("Model One".into()))).unwrap();
        let second = build_cast_display_edid(request(Some("Model Two".into()))).unwrap();
        assert_eq!(first.identity().serial, second.identity().serial);
        assert_ne!(
            first.identity().product_code,
            second.identity().product_code
        );
        assert_ne!(first.edid(), second.edid());

        let mut other_device = request(Some("Model One".into()));
        other_device.device_id = "device-5678".into();
        let other_device = build_cast_display_edid(other_device).unwrap();
        assert_ne!(first.identity().serial, other_device.identity().serial);
        assert_eq!(
            first.identity().product_code,
            other_device.identity().product_code
        );
    }

    #[test]
    fn assigned_name_does_not_change_numeric_identity() {
        let first = build_cast_display_edid(request(Some("Model One".into()))).unwrap();
        let mut renamed_request = request(Some("Model One".into()));
        renamed_request.display_name = Some("Apartment Living Room TV".into());
        let renamed = build_cast_display_edid(renamed_request).unwrap();

        assert_eq!(first.identity(), renamed.identity());
        assert!(first.has_same_monitor_configuration(&renamed));
        assert_ne!(first.edid(), renamed.edid());
        assert_eq!(renamed.display_name(), Some("Apartment Living Room TV"));
    }

    #[test]
    fn advertises_only_validated_modes_and_requested_audio() {
        let mut input = request(None);
        input.audio = false;
        input.modes.push(EdidMode::new(1280, 720, 60_000).unwrap());
        let generated = build_cast_display_edid(input).unwrap();
        let bytes = generated.edid().as_bytes();
        let cta = bytes
            .chunks_exact(EDID_BLOCK_SIZE)
            .find(|block| block[0] == CTA_EXTENSION_TAG)
            .expect("EDID has a CTA extension");
        assert_ne!(&bytes[54..72], &[0_u8; 18]);
        assert_ne!(&bytes[72..90], &[0_u8; 18]);
        assert_eq!(cta[2], 11);
        assert_eq!(cta[3], 0x81);

        assert!(matches!(
            EdidMode::new(1366, 768, 60_000),
            Err(CastDisplayEdidError::UnsupportedMode(_))
        ));
        let mut duplicate = request(None);
        duplicate.modes.push(duplicate.modes[0]);
        assert!(matches!(
            build_cast_display_edid(duplicate),
            Err(CastDisplayEdidError::DuplicateMode(_))
        ));
    }

    #[test]
    fn advertises_a_validated_cec_address_in_the_hdmi_vsdb() {
        let mut input = request(None);
        input.cec_physical_address = Some(0x1000);
        let generated = build_cast_display_edid(input).unwrap();
        let bytes = generated.edid().as_bytes();
        assert_eq!(bytes[19], 4);
        assert_eq!(bytes[EDID_BLOCK_SIZE], CTA_EXTENSION_TAG);
        assert!(bytes
            .windows(6)
            .any(|bytes| bytes == [0x65, 0x03, 0x0c, 0x00, 0x10, 0x00]));

        for invalid in [0x0000, 0x1010, 0x1001, 0xffff] {
            let mut input = request(None);
            input.cec_physical_address = Some(invalid);
            assert_eq!(
                build_cast_display_edid(input).unwrap_err(),
                CastDisplayEdidError::InvalidCecPhysicalAddress(invalid)
            );
        }
    }
}
