# Offline display-scene replay

`drm-executor-replay` renders a saved JSON fixture through the CPU reference
renderer and writes a binary PPM image. It needs no display server, GPU, media
stack or privileges. It does not capture a live desktop or enable CPU fallback
in Pronk. The fixture schema is a test format, not a proposed kernel or executor
wire protocol.

From the repository root:

```sh
cargo test --locked -p drm-executor-replay
cargo run --locked -p drm-executor-replay -- \
  tests/executor-replay/fixtures/alpha-stack.json /var/tmp/alpha-stack.ppm
```

Choose a new output path: existing files are never overwritten. The complete
scene is validated and rendered before opening the output. An output I/O error
is reported but may leave a partial newly created file. PPM stores RGB bytes;
the reference renderer's opaque output alpha is omitted.

## Fixture contract

The files in `fixtures/` are small, complete examples. Every field is required;
unknown fields and versions other than `1` are rejected.

- `output` gives nonzero `width`, `height` and an opaque RGB `background`.
- `sources` contain explicit dimensions, `format`, byte `offset`, row `stride`
  and initialized `bytes`. `XR24`/`AR24` use B,G,R,X/A byte order;
  `XB24`/`AB24` use R,G,B,X/A. X bytes never supply alpha. Leading and row
  padding are allowed; bytes must cover the final visible pixel.
- `layers` are ordered bottom to top. `source` indexes the source list.
  `crop_16_16` is unsigned fixed-point `[x, y, width, height]`: each integer
  pixel unit is 65536. Fractional values are rejected, not rounded.
  `position` gives signed destination coordinates; output clipping is allowed.
- `pixel_blend` is `none`, `premultiplied` or `coverage`. `plane_alpha` is a
  normalized integer from 0 to 65535, independent of the pixel format.
- `reflect_x` and `reflect_y` reflect the cropped source axes before
  counter-clockwise `rotation` of 0, 90, 180 or 270 degrees. There is no scaling.

All colors share one encoded RGB domain. There is no color-space conversion,
YUV, sampling filter, native synchronization or modifier interpretation. The
[renderer model](../../docs/display-executor.md) defines the supported blend
equations and rounding behavior.

Input is limited to 4 MiB, 64 sources and 256 layers; output storage is limited
to 64 MiB. Those are offline tool limits, not capture queue or kernel limits.
The tool is intended for small regression scenes, not processing untrusted
workloads with a guaranteed execution-time budget.

## Checks

The corpus compares complete PPM output against literal RGB expectations for
padded source crops, negative placement, stacked alpha and reflected quarter
turns. It also rejects invalid layouts, missing sources, unsupported transforms,
fractional crops and excessive sizes, and checks output writer errors.

JSON parsing and file output stay in this test package. The
`drm-display-executor` model remains dependency-free; native resource ownership
and GPU submission remain in `pronk-gpu`.
