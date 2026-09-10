# Display executor rendering model

`drm-display-executor` contains device-independent renderer inputs and reference
operations. It has no external dependencies and forbids unsafe Rust. It does
not open DRM nodes, import DMA-BUFs, own capture grants or publish PipeWire
frames. Native storage and completion remain in `pronk-gpu`; application
supervision and capability routing remain outside the rendering model.

## Initial geometry profile

`scene::geometry` describes integral, unscaled, unrotated pixel placement.
`Extent` requires nonzero dimensions. `SourceRect` validates a crop against its
source image, using widened endpoint arithmetic so bounds cannot wrap.
`clip_to` places that crop at signed output coordinates and returns either a
nonempty `CopyRegion` or no visible pixels. Left/top clipping advances the
source origin by the same amount. Right/bottom clipping reduces the copied
extent. The original crop and image description stay unchanged.

These values describe geometry, not permission or storage validity. A renderer
must match them to the actual source and destination dimensions, formats and
access lifetimes. A native backend must separately enforce device limits and
checked conversion into its own coordinate types. No assumption that all
unsigned image coordinates fit a signed native offset is implied.

`SourceRect::from_fixed_16_16` accepts DRM-style unsigned source coordinates
only when all four values are exactly integral. It rejects fractional origins
and sizes, then checks nonempty dimensions and image bounds. It does not
round unsupported inputs into a different image. This is a coordinate adapter,
not a scene protocol or authorization check: destination scaling and rotation
still require independent profile validation.

The crop model has no implicit fixed-point truncation, scaling,
rotation, blend or color-pipeline behavior. A protocol adapter must reject
unsupported input rather than silently interpreting it as an integral copy.
It is not a kernel ABI or a complete scene packet yet.

Run `cargo test --locked -p drm-display-executor` without a GPU or media stack.
The geometry tests compare small crops against a per-pixel visibility oracle
and exercise empty extents, source bounds, fully offscreen placements and
extreme signed/unsigned coordinates.

## CPU reference image views

`render::cpu::image` accepts ordinary borrowed byte slices, not native image
handles. `LinearLayout` checks four-byte packed RGB row width, stride, offset
and the last addressed byte with overflow detection. Views require sufficient
backing but do not require padding after the final row. Row access excludes
leading, inter-row and trailing padding; out-of-range rows return `None`.

`Image` supplies shared reads; `ImageMut` borrows output storage exclusively.
A mutable row cannot outlive its exclusive view borrow. The initial encodings
are XRGB8888, ARGB8888, XBGR8888 and ABGR8888 in the documented little-endian
byte order. Format identity alone does not select a blend mode or color space.

These views neither allocate nor map pixels. They do not establish native
producer completion or cache coherency, and do not enable CPU fallback in the
casting service. Their purpose is a deterministic rendering reference with
checked ordinary storage access. Tests verify address limits, exact row bounds,
padding preservation and exclusive borrowing without a GPU.

## Initial RGB composition reference

`render::cpu::compose` places validated layers over an opaque RGB background,
in caller-supplied bottom-to-top order. Each `Layer` validates its crop against
the actual input view. The profile uses integral placement without scaling
and identity color processing. `scene::blend` independently describes
pixel interpretation and normalized 16-bit plane-wide alpha. A layer defaults
to premultiplied pixel alpha and fully opaque plane alpha; `with_blend` selects
DRM's None, Pre-multiplied or Coverage equation explicitly. None ignores pixel
alpha, while Coverage multiplies source colors by it. XRGB/XBGR padding never
participates as pixel alpha, making all three modes equivalent for those formats.

The reference expands byte components to 16-bit normalized integers, rounds
each blend at that precision and converts to bytes only after all layers.
Plane and pixel alpha participate in one widened numerator before rounding;
their product is not first quantized back to 16 bits. Zero plane alpha leaves
the background unchanged even for malformed premultiplied source colors.
Premultiplied sums outside the normalized range saturate. Output alpha (or its
X byte) is written as opaque. Source channels, background and destination are
assumed to share one encoded RGB domain: no implicit linearization, lookup table,
matrix or transfer-function conversion is performed. This implements a bounded
subset of the [DRM plane blend contract](https://docs.kernel.org/gpu/drm-kms.html#plane-composition-properties),
not full color-pipeline parity.

One scratch row is reserved before output writes. Reported allocation failure
therefore leaves output untouched. Successful return ends ordinary slice reads
and writes; it is not by itself DMA-BUF cache maintenance or a native fence.
No production fallback is enabled. Tests cover channel order, alpha, stacking,
intermediate precision, clipping, background fill and untouched output padding.
Blend tests compare all pixel-alpha byte values and selected plane-alpha
boundaries against independently evaluated normalized equations.

## Orthogonal source transforms

`scene::transform` describes source-axis reflection followed by counter-clockwise
quarter-turn rotation, matching DRM's ordering. Quarter turns swap crop width
and height. `source_at` maps a transformed crop coordinate back to its original
local pixel, rejecting out-of-bounds coordinates without sampling or filtering.
The crop origin is added only after that inverse transform.

CPU layers select the policy with `with_transform`; the default is identity.
Output clipping happens in the transformed crop's coordinate space. Neither
rotation nor reflection permits reading outside the validated original crop.
The model does not implicitly enable transforms in native GPU copies, which
remain restricted to the documented unrotated profile.

Tests use literal non-square rotation patterns, all reflection/rotation
combinations over small dimensions, unsigned coordinate limits and a clipped
rotated crop surrounded by sentinel pixels. Output padding remains untouched.
