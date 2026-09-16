//! Checked ownership of one versioned complete-scene renderer packet.

use std::io;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use castkms_sys::{
    drm_ioctl_castkms_renderer_dequeue_scene, DrmCastkmsRendererColor,
    DrmCastkmsRendererDequeueScene, DrmCastkmsRendererLayer, DrmCastkmsRendererScene,
    DRM_FORMAT_MOD_INVALID, RENDERER_COLOR_BYPASS, RENDERER_COLOR_LUT, RENDERER_COLOR_MATRIX,
    RENDERER_COLOR_SRGB_EOTF, RENDERER_COLOR_SRGB_INVERSE_EOTF, RENDERER_LAYER_CURSOR,
    RENDERER_LAYER_OVERLAY, RENDERER_LAYER_PRIMARY, RENDERER_MAX_PLANES, RENDERER_SCENE_MAX_BYTES,
    RENDERER_SCENE_MAX_COLOR_OPS, RENDERER_SCENE_MAX_LAYERS, RENDERER_SCENE_VERSION,
    YUV_ENCODING_BT2020, YUV_ENCODING_BT601, YUV_ENCODING_BT709, YUV_RANGE_FULL, YUV_RANGE_LIMITED,
};
use drm_display_executor::scene::geometry::{DestinationRect, Extent, SourceRect};

use crate::source::{has_close_on_exec, release_source};
use crate::{ActiveRenderer, FormatModifier, SourceImage, SourcePlane, SourceReleaseError};

/// KMS plane role retained by one scene layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerKind {
    Primary,
    Overlay,
    Cursor,
}

/// YUV encoding metadata retained independently of format support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorEncoding {
    Bt601,
    Bt709,
    Bt2020,
}

/// YUV range metadata retained independently of format support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorRange {
    Limited,
    Full,
}

/// One owned operation from a layer or output color pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorOperation {
    Bypass,
    SrgbEotf,
    SrgbInverseEotf,
    Matrix([u64; 12]),
    Lut(Box<[[u16; 3]]>),
}

/// One back-to-front layer in a complete scene.
#[derive(Debug)]
pub struct SceneLayer {
    kind: LayerKind,
    zpos: u32,
    image: SourceImage,
    source: SourceRect,
    destination: DestinationRect,
    encoding: ColorEncoding,
    range: ColorRange,
    color: Box<[ColorOperation]>,
}

impl SceneLayer {
    pub fn kind(&self) -> LayerKind {
        self.kind
    }

    pub fn zpos(&self) -> u32 {
        self.zpos
    }

    pub fn image(&self) -> &SourceImage {
        &self.image
    }

    pub fn source(&self) -> SourceRect {
        self.source
    }

    pub fn destination(&self) -> DestinationRect {
        self.destination
    }

    pub fn color_encoding(&self) -> ColorEncoding {
        self.encoding
    }

    pub fn color_range(&self) -> ColorRange {
        self.range
    }

    pub fn color(&self) -> &[ColorOperation] {
        &self.color
    }
}

/// One complete scene claim requiring exactly one terminal release.
#[must_use = "release the scene job after every source access ends"]
#[derive(Debug)]
pub struct SceneJob<'job, 'renderer, F: AsFd> {
    renderer: &'job mut ActiveRenderer<'renderer, F>,
    id: NonZeroU64,
    content_serial: NonZeroU64,
    output: Extent,
    layers: Vec<SceneLayer>,
    color: Box<[ColorOperation]>,
    producer: Option<OwnedFd>,
}

impl<F: AsFd> SceneJob<'_, '_, F> {
    pub fn content_serial(&self) -> NonZeroU64 {
        self.content_serial
    }

    pub fn output(&self) -> Extent {
        self.output
    }

    pub fn layers(&self) -> &[SceneLayer] {
        &self.layers
    }

    pub fn color(&self) -> &[ColorOperation] {
        &self.color
    }

    pub fn producer_completion(&self) -> Option<BorrowedFd<'_>> {
        self.producer.as_ref().map(AsFd::as_fd)
    }

    pub fn release_without_access(self) -> Result<(), SourceReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_NO_ACCESS, None)
    }

    /// Promise that all synchronous CPU access to every layer has ended.
    pub fn release_cpu(self) -> Result<(), SourceReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_CPU_DONE, None)
    }

    pub fn release_submitted(
        self,
        completion: Option<BorrowedFd<'_>>,
    ) -> Result<(), SourceReleaseError<Self>> {
        self.release(castkms_sys::RENDERER_RELEASE_SUBMITTED, completion)
    }

    fn release(
        self,
        kind: u32,
        completion: Option<BorrowedFd<'_>>,
    ) -> Result<(), SourceReleaseError<Self>> {
        if let Err(error) = release_source(self.renderer.as_fd(), self.id, kind, completion) {
            return Err(SourceReleaseError::new(self, error));
        }
        Ok(())
    }
}

impl<'renderer, F: AsFd> ActiveRenderer<'renderer, F> {
    /// Claim the next changed complete scene as one source-read transaction.
    ///
    /// `None` means the current scene is blank or unchanged. The exclusive
    /// borrow prevents another source or scene job on this renderer endpoint.
    ///
    /// ```compile_fail
    /// use castkms_renderer::ActiveRenderer;
    /// use std::fmt::Debug;
    /// use std::os::fd::AsFd;
    ///
    /// fn claim_twice<F: AsFd + Debug>(renderer: &mut ActiveRenderer<'_, F>) {
    ///     let first = renderer.try_dequeue_scene().unwrap().unwrap();
    ///     let second = renderer.try_dequeue_scene().unwrap();
    ///     drop((first, second));
    /// }
    /// ```
    pub fn try_dequeue_scene<'job>(
        &'job mut self,
    ) -> io::Result<Option<SceneJob<'job, 'renderer, F>>> {
        let words = RENDERER_SCENE_MAX_BYTES / size_of::<u64>();
        let mut storage = Vec::new();
        storage.try_reserve_exact(words).map_err(io::Error::other)?;
        storage.resize(words, u64::MAX);
        let request = DrmCastkmsRendererDequeueScene {
            result: storage.as_mut_ptr() as u64,
            capacity: RENDERER_SCENE_MAX_BYTES as u32,
            ..Default::default()
        };
        // SAFETY: The fixed request and aligned writable maximum-size storage
        // remain live throughout the synchronous ioctl. Success installs fresh
        // descriptors only in the returned scene records.
        if let Err(error) =
            unsafe { drm_ioctl_castkms_renderer_dequeue_scene(self.as_fd().as_raw_fd(), &request) }
        {
            if error == nix::errno::Errno::ENODATA {
                return Ok(None);
            }
            return Err(error.into());
        }
        // SAFETY: The u64 allocation is initialized and remains live. Viewing
        // its storage as bytes preserves its allocation and alignment.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                storage.as_ptr().cast::<u8>(),
                storage.len() * size_of::<u64>(),
            )
        };
        let job_id = read::<DrmCastkmsRendererScene>(bytes, 0)
            .ok()
            .and_then(|header| NonZeroU64::new(header.job_id));
        let decoded = match decode_scene(bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                if let Some(id) = job_id {
                    let _ = release_source(
                        self.as_fd(),
                        id,
                        castkms_sys::RENDERER_RELEASE_NO_ACCESS,
                        None,
                    );
                }
                return Err(error);
            }
        };
        Ok(Some(SceneJob {
            renderer: self,
            id: decoded.id,
            content_serial: decoded.content_serial,
            output: decoded.output,
            layers: decoded.layers,
            color: decoded.color,
            producer: decoded.producer,
        }))
    }
}

pub(super) struct DecodedScene {
    id: NonZeroU64,
    content_serial: NonZeroU64,
    output: Extent,
    layers: Vec<SceneLayer>,
    color: Box<[ColorOperation]>,
    producer: Option<OwnedFd>,
}

pub(super) fn decode_scene(bytes: &[u8]) -> io::Result<DecodedScene> {
    let header: DrmCastkmsRendererScene = read(bytes, 0)?;
    let total = usize::try_from(header.bytes).map_err(|_| invalid("scene size overflowed"))?;
    if header.version != RENDERER_SCENE_VERSION
        || total < size_of::<DrmCastkmsRendererScene>()
        || total > bytes.len()
        || total > RENDERER_SCENE_MAX_BYTES
        || total % 8 != 0
        || header.reserved != 0
    {
        return Err(invalid("CastKMS returned an invalid scene header"));
    }
    let id = NonZeroU64::new(header.job_id)
        .ok_or_else(|| invalid("CastKMS returned a zero scene job ID"))?;
    let content_serial = NonZeroU64::new(header.content_serial)
        .ok_or_else(|| invalid("CastKMS returned a zero scene content serial"))?;
    let output = Extent::new(header.width, header.height)
        .map_err(|_| invalid("CastKMS returned empty scene dimensions"))?;
    let layer_count = usize::try_from(header.layer_count)
        .ok()
        .filter(|count| (1..=RENDERER_SCENE_MAX_LAYERS).contains(count))
        .ok_or_else(|| invalid("CastKMS returned an invalid scene layer count"))?;
    let output_color_count = usize::try_from(header.output_color_count)
        .ok()
        .filter(|count| *count <= 3)
        .ok_or_else(|| invalid("CastKMS returned an invalid output color count"))?;

    const MAX_FDS: usize = 1 + RENDERER_SCENE_MAX_LAYERS * RENDERER_MAX_PLANES;
    let mut adopted = [-1; MAX_FDS];
    let mut adopted_count = 0;
    let producer = adopt_fd(header.producer_fd, &mut adopted, &mut adopted_count)?;

    let mut cursor = size_of::<DrmCastkmsRendererScene>();
    let mut raw_layers = Vec::new();
    raw_layers
        .try_reserve_exact(layer_count)
        .map_err(io::Error::other)?;
    for _ in 0..layer_count {
        let start = cursor;
        let layer: DrmCastkmsRendererLayer = read(bytes, start)?;
        let mut plane_fds: [Option<OwnedFd>; RENDERER_MAX_PLANES] = std::array::from_fn(|_| None);
        for (owner, plane) in plane_fds.iter_mut().zip(layer.planes) {
            *owner = adopt_fd(plane.dma_buf_fd, &mut adopted, &mut adopted_count)?;
        }
        let layer_bytes = usize::try_from(layer.bytes)
            .ok()
            .filter(|length| *length >= size_of::<DrmCastkmsRendererLayer>() && *length % 8 == 0)
            .ok_or_else(|| invalid("CastKMS returned an invalid scene layer size"))?;
        let end = start
            .checked_add(layer_bytes)
            .filter(|end| *end <= total)
            .ok_or_else(|| invalid("CastKMS returned a truncated scene layer"))?;
        cursor = start + size_of::<DrmCastkmsRendererLayer>();
        let color_count = usize::try_from(layer.color_count)
            .ok()
            .filter(|count| *count <= RENDERER_SCENE_MAX_COLOR_OPS)
            .ok_or_else(|| invalid("CastKMS returned too many layer color operations"))?;
        let color = decode_colors(bytes, &mut cursor, color_count, end)?;
        if cursor != end {
            return Err(invalid("CastKMS returned trailing layer metadata"));
        }
        raw_layers.push((layer, color, plane_fds));
    }
    let color = decode_colors(bytes, &mut cursor, output_color_count, total)?;
    if cursor != total {
        return Err(invalid("CastKMS returned trailing scene metadata"));
    }
    if producer.as_ref().is_some_and(|fd| !has_close_on_exec(fd)) {
        return Err(invalid("CastKMS returned a producer without close-on-exec"));
    }

    let mut layers = Vec::new();
    layers
        .try_reserve_exact(layer_count)
        .map_err(io::Error::other)?;
    for (raw, color, plane_fds) in raw_layers {
        layers.push(decode_layer(raw, color, plane_fds)?);
    }
    if layers
        .windows(2)
        .any(|pair| pair[0].zpos() > pair[1].zpos())
    {
        return Err(invalid("CastKMS returned layers outside stacking order"));
    }
    Ok(DecodedScene {
        id,
        content_serial,
        output,
        layers,
        color: color.into_boxed_slice(),
        producer,
    })
}

fn adopt_fd<const N: usize>(
    fd: i32,
    adopted: &mut [i32; N],
    count: &mut usize,
) -> io::Result<Option<OwnedFd>> {
    if fd < -1 {
        return Err(invalid("CastKMS returned an invalid scene descriptor"));
    }
    if fd == -1 {
        return Ok(None);
    }
    if *count >= adopted.len() {
        return Err(invalid("CastKMS returned too many scene descriptors"));
    }
    if adopted[..*count].contains(&fd) {
        return Err(invalid("CastKMS returned a duplicate scene descriptor"));
    }
    adopted[*count] = fd;
    *count += 1;
    // SAFETY: Each nonnegative descriptor is adopted once, and the duplicate
    // check runs before constructing another owner for the same descriptor.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn decode_layer(
    raw: DrmCastkmsRendererLayer,
    color: Vec<ColorOperation>,
    plane_fds: [Option<OwnedFd>; RENDERER_MAX_PLANES],
) -> io::Result<SceneLayer> {
    let kind = match raw.kind {
        RENDERER_LAYER_PRIMARY => LayerKind::Primary,
        RENDERER_LAYER_OVERLAY => LayerKind::Overlay,
        RENDERER_LAYER_CURSOR => LayerKind::Cursor,
        _ => return Err(invalid("CastKMS returned an unknown scene layer role")),
    };
    let encoding = match raw.color_encoding {
        YUV_ENCODING_BT601 => ColorEncoding::Bt601,
        YUV_ENCODING_BT709 => ColorEncoding::Bt709,
        YUV_ENCODING_BT2020 => ColorEncoding::Bt2020,
        _ => return Err(invalid("CastKMS returned an unknown color encoding")),
    };
    let range = match raw.color_range {
        YUV_RANGE_LIMITED => ColorRange::Limited,
        YUV_RANGE_FULL => ColorRange::Full,
        _ => return Err(invalid("CastKMS returned an unknown color range")),
    };
    let extent = Extent::new(raw.width, raw.height)
        .map_err(|_| invalid("CastKMS returned empty layer dimensions"))?;
    let source = SourceRect::from_fixed_16_16(extent, raw.source)
        .map_err(|_| invalid("CastKMS returned invalid layer source coordinates"))?;
    let destination_extent = Extent::new(raw.destination[0], raw.destination[1])
        .map_err(|_| invalid("CastKMS returned empty layer destination dimensions"))?;
    let destination = DestinationRect {
        position: raw.position,
        extent: destination_extent,
    };
    let plane_count = usize::try_from(raw.plane_count)
        .ok()
        .filter(|count| (1..=RENDERER_MAX_PLANES).contains(count))
        .ok_or_else(|| invalid("CastKMS returned an invalid layer plane count"))?;
    let mut planes: [Option<SourcePlane>; RENDERER_MAX_PLANES] = std::array::from_fn(|_| None);
    for (index, ((metadata, fd), destination)) in raw
        .planes
        .into_iter()
        .zip(plane_fds)
        .zip(planes.iter_mut())
        .enumerate()
    {
        if index < plane_count {
            let dma_buf = fd.ok_or_else(|| invalid("CastKMS omitted a scene plane descriptor"))?;
            if metadata.reserved != 0 || !has_close_on_exec(&dma_buf) {
                return Err(invalid("CastKMS returned invalid scene plane metadata"));
            }
            *destination = Some(SourcePlane::from_parts(
                dma_buf,
                NonZeroU32::new(metadata.pitch)
                    .ok_or_else(|| invalid("CastKMS returned a zero scene plane pitch"))?,
                metadata.offset,
            ));
        } else if fd.is_some()
            || metadata.pitch != 0
            || metadata.offset != 0
            || metadata.reserved != 0
        {
            return Err(invalid("CastKMS initialized an unused scene plane"));
        }
    }
    Ok(SceneLayer {
        kind,
        zpos: raw.zpos,
        image: SourceImage::from_parts(
            raw.format,
            if raw.modifier == DRM_FORMAT_MOD_INVALID {
                FormatModifier::Unspecified
            } else {
                FormatModifier::Explicit(raw.modifier)
            },
            extent,
            planes,
            plane_count,
        ),
        source,
        destination,
        encoding,
        range,
        color: color.into_boxed_slice(),
    })
}

fn decode_colors(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
    end: usize,
) -> io::Result<Vec<ColorOperation>> {
    let mut operations = Vec::new();
    operations
        .try_reserve_exact(count)
        .map_err(io::Error::other)?;
    for _ in 0..count {
        let header: DrmCastkmsRendererColor = read_bounded(bytes, *cursor, end)?;
        *cursor = cursor
            .checked_add(size_of::<DrmCastkmsRendererColor>())
            .ok_or_else(|| invalid("color record offset overflowed"))?;
        let payload = usize::try_from(header.payload_bytes)
            .ok()
            .filter(|length| *length % 8 == 0)
            .ok_or_else(|| invalid("CastKMS returned a misaligned color payload"))?;
        let payload_end = cursor
            .checked_add(payload)
            .filter(|payload_end| *payload_end <= end)
            .ok_or_else(|| invalid("CastKMS returned a truncated color payload"))?;
        let operation = match header.kind {
            RENDERER_COLOR_BYPASS if payload == 0 => ColorOperation::Bypass,
            RENDERER_COLOR_SRGB_EOTF if payload == 0 => ColorOperation::SrgbEotf,
            RENDERER_COLOR_SRGB_INVERSE_EOTF if payload == 0 => ColorOperation::SrgbInverseEotf,
            RENDERER_COLOR_MATRIX if payload == 12 * size_of::<u64>() => {
                let mut matrix = [0; 12];
                for value in &mut matrix {
                    *value = read_bounded(bytes, *cursor, payload_end)?;
                    *cursor += size_of::<u64>();
                }
                ColorOperation::Matrix(matrix)
            }
            RENDERER_COLOR_LUT if (8..=256 * 8).contains(&payload) => {
                let mut entries = Vec::new();
                entries
                    .try_reserve_exact(payload / 8)
                    .map_err(io::Error::other)?;
                while *cursor < payload_end {
                    let red: u16 = read_bounded(bytes, *cursor, payload_end)?;
                    let green: u16 = read_bounded(bytes, *cursor + 2, payload_end)?;
                    let blue: u16 = read_bounded(bytes, *cursor + 4, payload_end)?;
                    let reserved: u16 = read_bounded(bytes, *cursor + 6, payload_end)?;
                    if reserved != 0 {
                        return Err(invalid("CastKMS returned reserved LUT data"));
                    }
                    entries.push([red, green, blue]);
                    *cursor += 8;
                }
                ColorOperation::Lut(entries.into_boxed_slice())
            }
            _ => return Err(invalid("CastKMS returned an invalid color operation")),
        };
        *cursor = payload_end;
        operations.push(operation);
    }
    Ok(operations)
}

fn read<T: WireValue>(bytes: &[u8], offset: usize) -> io::Result<T> {
    read_bounded(bytes, offset, bytes.len())
}

/// Values that can be copied from an unaligned native-endian byte record.
///
/// # Safety
///
/// Every initialized bit pattern must represent a valid value, with no padding
/// bytes whose value is observed by safe Rust.
unsafe trait WireValue: Copy {}

// SAFETY: These types contain only integer fields, so every initialized bit
// pattern is valid and an unaligned byte copy can construct a value.
unsafe impl WireValue for u16 {}
unsafe impl WireValue for u64 {}
unsafe impl WireValue for DrmCastkmsRendererScene {}
unsafe impl WireValue for DrmCastkmsRendererLayer {}
unsafe impl WireValue for DrmCastkmsRendererColor {}

fn read_bounded<T: WireValue>(bytes: &[u8], offset: usize, end: usize) -> io::Result<T> {
    let record_end = offset
        .checked_add(size_of::<T>())
        .filter(|record_end| *record_end <= end && *record_end <= bytes.len())
        .ok_or_else(|| invalid("CastKMS returned truncated scene metadata"))?;
    let _ = record_end;
    // SAFETY: Callers instantiate this private helper only with repr(C)
    // integer-only UAPI records or integer primitives, for which every bit
    // pattern is valid. Bounds were checked above and unaligned reads are used.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;

    use nix::fcntl::{fcntl, FcntlArg, FdFlag};

    use super::*;

    fn word(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }

    fn wide(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }

    fn patch(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    fn tracked_descriptor() -> (i32, UnixStream) {
        let (descriptor, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        (descriptor.into_raw_fd(), peer)
    }

    fn assert_descriptor_open(peer: &mut UnixStream) {
        let mut byte = [0];
        assert_eq!(
            peer.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    fn assert_descriptor_closed(peer: &mut UnixStream) {
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    fn scene_packet() -> (Vec<u8>, i32, UnixStream) {
        let (dma_buf, peer) = tracked_descriptor();
        let mut bytes = Vec::new();
        word(&mut bytes, RENDERER_SCENE_VERSION);
        word(&mut bytes, 0);
        wide(&mut bytes, 7);
        wide(&mut bytes, 11);
        word(&mut bytes, 1920);
        word(&mut bytes, 1080);
        word(&mut bytes, 1);
        word(&mut bytes, u32::MAX);
        word(&mut bytes, 1);
        word(&mut bytes, 0);

        let layer = bytes.len();
        word(&mut bytes, 0);
        word(&mut bytes, RENDERER_LAYER_PRIMARY);
        word(&mut bytes, 3);
        word(&mut bytes, castkms_sys::DRM_FORMAT_ARGB8888);
        wide(&mut bytes, 9);
        word(&mut bytes, 640);
        word(&mut bytes, 480);
        for coordinate in [0, 0, 640 << 16, 480 << 16] {
            word(&mut bytes, coordinate);
        }
        word(&mut bytes, (-20_i32) as u32);
        word(&mut bytes, 10);
        word(&mut bytes, 1280);
        word(&mut bytes, 960);
        word(&mut bytes, 1);
        word(&mut bytes, 1);
        word(&mut bytes, 1);
        word(&mut bytes, 2);
        for index in 0..RENDERER_MAX_PLANES {
            word(
                &mut bytes,
                if index == 0 { dma_buf as u32 } else { u32::MAX },
            );
            word(&mut bytes, if index == 0 { 2560 } else { 0 });
            word(&mut bytes, 0);
            word(&mut bytes, 0);
        }
        word(&mut bytes, RENDERER_COLOR_BYPASS);
        word(&mut bytes, 0);
        word(&mut bytes, RENDERER_COLOR_MATRIX);
        word(&mut bytes, 96);
        for value in 0..12 {
            wide(&mut bytes, value);
        }
        let layer_bytes = (bytes.len() - layer) as u32;
        patch(&mut bytes, layer, layer_bytes);

        word(&mut bytes, RENDERER_COLOR_LUT);
        word(&mut bytes, 16);
        for entry in [[1_u16, 2, 3], [4, 5, 6]] {
            for channel in entry {
                bytes.extend_from_slice(&channel.to_ne_bytes());
            }
            bytes.extend_from_slice(&0_u16.to_ne_bytes());
        }
        let total = bytes.len() as u32;
        patch(&mut bytes, 4, total);
        (bytes, dma_buf, peer)
    }

    #[test]
    fn scene_packet_preserves_geometry_color_and_descriptor_ownership() {
        let (bytes, _raw_fd, mut peer) = scene_packet();
        let scene = decode_scene(&bytes).unwrap();
        assert_eq!(scene.id.get(), 7);
        assert_eq!(scene.content_serial.get(), 11);
        assert_eq!(scene.output, Extent::new(1920, 1080).unwrap());
        assert!(scene.producer.is_none());
        assert_eq!(scene.layers.len(), 1);
        let layer = &scene.layers[0];
        assert_eq!(layer.kind(), LayerKind::Primary);
        assert_eq!(layer.zpos(), 3);
        assert_eq!(layer.image().format(), castkms_sys::DRM_FORMAT_ARGB8888);
        assert_eq!(layer.image().modifier(), FormatModifier::Explicit(9));
        assert_eq!(layer.image().planes().len(), 1);
        assert_eq!(layer.destination().position, [-20, 10]);
        assert_eq!(layer.color_encoding(), ColorEncoding::Bt709);
        assert_eq!(layer.color_range(), ColorRange::Full);
        assert_eq!(layer.color()[0], ColorOperation::Bypass);
        assert_eq!(
            layer.color()[1],
            ColorOperation::Matrix(std::array::from_fn(|index| index as u64))
        );
        assert_eq!(
            scene.color.as_ref(),
            [ColorOperation::Lut(Box::new([[1, 2, 3], [4, 5, 6]]))]
        );
        assert_descriptor_open(&mut peer);
        drop(scene);
        assert_descriptor_closed(&mut peer);
    }

    #[test]
    fn color_records_reject_invalid_payloads_and_reserved_lut_channels() {
        let mut truncated = Vec::new();
        word(&mut truncated, RENDERER_COLOR_MATRIX);
        word(&mut truncated, 96);
        let mut cursor = 0;
        assert_eq!(
            decode_colors(&truncated, &mut cursor, 1, truncated.len())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut lut = Vec::new();
        word(&mut lut, RENDERER_COLOR_LUT);
        word(&mut lut, 8);
        for value in [1_u16, 2, 3, 4] {
            lut.extend_from_slice(&value.to_ne_bytes());
        }
        let mut cursor = 0;
        assert_eq!(
            decode_colors(&lut, &mut cursor, 1, lut.len())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn duplicate_scene_descriptors_are_closed_once() {
        let (mut bytes, raw_fd, mut peer) = scene_packet();
        let layer = size_of::<DrmCastkmsRendererScene>();
        patch(&mut bytes, layer + 72, 2);
        patch(&mut bytes, layer + 80 + 16, raw_fd as u32);
        patch(&mut bytes, layer + 80 + 16 + 4, 2560);
        assert_eq!(
            decode_scene(&bytes).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_descriptor_closed(&mut peer);
    }

    #[test]
    fn descriptors_are_closed_when_later_scene_metadata_is_rejected() {
        let (mut bytes, _raw_fd, mut peer) = scene_packet();
        let reserved = bytes.len() - size_of::<u16>();
        bytes[reserved..].copy_from_slice(&1_u16.to_ne_bytes());

        assert_eq!(
            decode_scene(&bytes).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_descriptor_closed(&mut peer);
    }

    #[test]
    fn invalid_producer_flags_do_not_leak_layer_descriptors() {
        let (mut bytes, _layer_fd, mut layer_peer) = scene_packet();
        let (producer_fd, mut producer_peer) = tracked_descriptor();
        fcntl(producer_fd, FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
        patch(
            &mut bytes,
            std::mem::offset_of!(DrmCastkmsRendererScene, producer_fd),
            producer_fd as u32,
        );

        assert_eq!(
            decode_scene(&bytes).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert_descriptor_closed(&mut producer_peer);
        assert_descriptor_closed(&mut layer_peer);
    }
}
