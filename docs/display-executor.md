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

The model intentionally has no implicit fixed-point truncation, scaling,
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
