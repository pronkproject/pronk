# Generated GPU frames through PipeWire and hardware encoding

This opt-in harness connects the real Rust Vulkan producer, output pool,
`GpuOutput` adapter and `VideoSourceActor` to a GStreamer PipeWire consumer.
It uses generated images, not a capture grant or compositor sources.

Run from the repository root with explicit GPU and hexadecimal modifier:

```sh
sh tests/gpu-media/run-private.sh /dev/dri/renderD128 0100000000000009
```

That tuple names the Lunar Lake development device, not a portable default.
Requirements include the Rust toolchain, Vulkan shared-image support, PipeWire,
`pw-link`, GStreamer with its PipeWire plugin, `timeout`, `jq` and `rg`. Native
device access is required; do not build Cargo artifacts as root. The binary
requires the `native` feature and is not built by ordinary workspace commands.

The wrapper builds the harness, starts an isolated PipeWire server in a fresh
`/var/tmp` directory, runs the test and terminates only that server. Logs are
retained in the printed directory. An outer process timeout complements the
async timeout and bounded runtime shutdown. Vulkan validation layers may be
enabled through the loader environment; reported validation errors fail the
wrapper even when the application itself exits successfully.

The producer uses the existing `AmbientDevelopment` mode. Its ambient remote
must equal the explicitly supplied socket path; it cannot accidentally fall
back to the desktop remote. The consumer connects a separate socket to that
same private graph. A waiting `pw-link` process connects the fixture's known
video ports concurrently with GStreamer startup, which may wait for the link.
No installed WirePlumber policy, service unit or casting session is changed.
This fixture does not qualify the production classified connection policy.

Three generated single-memory-plane modifier images belong to a separate
producer Vulkan device instance: a 1920x1080 base, a 640x480 overlay and a
128x128 cursor-sized top plane. The worker checks matching physical-device
and driver identities, imports each source use with exact allocator metadata
and its explicit producer fence, and copies into independently allocated,
non-exportable floating-point input images. It then overwrites every original
source white before shader composition. The composed private image is converted
into one of four persistent output images and overwritten black before output
publication. Only those four output allocations are registered with PipeWire.
Private input and composition allocations are created before source admission
and reused across frames; they have no export API or external reuse dependency.

The base source selects a 1856x1024 crop starting at (32,16), placed over a
black 1920x1080 background. Placements cycle through (-32,16), (32,-16) and
(64,32), exercising left clipping, top clipping and an inset rectangle. The
independent geometry model supplies expected visible rectangles; unit tests
check those against literal source and destination coordinates. The overlay
starts at (640,320), and the top plane moves between (608,288), (672,288) and
(736,288), overlapping both the overlay and exposed base. Layers have distinct
frame-dependent colors and opaque alpha. Private source copies have independent
native completion records. The compute shader blends their completed pixels in
bottom-to-top order, exercising cropped placement without retaining the imports.

The twenty-publication sequence exercises source-to-private-to-output copying
and reuse without a real compositor source or capture grant. Source-reading
completion precedes downstream destination waits. Native rendering and
allocation run on blocking workers, not on the PipeWire loop. Returned buffers pass through
the real source actor, publication correlation and native reuse checks before
another write. The first received sample is retained through six arrivals;
releases before sample disposal or while that sample is held fail the test.
All four images must be rewritten, and every sequence must arrive once in order.
The transport consumer requires DMA-BUF memory and never maps raw pixels.

Each three-source operation reserves three `SourceUse<Option<SyncFile>>`
submission permits before dispatch, one per source copy. `None` denotes the
native already-completed sentinel, not future work or omitted accounting.
The coordinator closes admission while the blocking source stage runs; that
stage records each actual native read immediately after submission. The
blocking worker retains pending imports and private storage while the
coordinator collects the normal terminal result. Only then
does the coordinator allow the worker to wait for successful pixels, overwrite
the originals, blend private inputs and convert composed storage to output.
Native execution does not wait for that permission: all work represented by
the reported fences has already been submitted. A fence may have signaled before collection on a
fast GPU, but collection does not require that outcome.

Pending native owners remain on the blocking worker even when coordination
fails. Channel closure prevents output work and runs native retirement there,
not on the Tokio runtime thread. Source and output operations live in a renderer
helper separate from PipeWire publication scheduling.
The three-record budget matches the fixture's source-copy operations, not a
frame or transport limit. No executor ioctl or kernel release message is
exercised here; this
connects the trusted accounting library to actual generated-source GPU work.

Successful output reports publication count and per-slot uses. Thirty-fps
timestamps are fixture configuration, not a measurement of delivered cadence.
The default `raw` profile does not inspect pixel contents. Neither profile
qualifies receiver behavior, an unsignaled native-reader stall, device loss,
installed service permissions or production private-node policy. Frame metadata
and memory-type checks do not establish that every library or driver avoids
all internal CPU access.

## Hardware H.264 profile

An optional `va-h264` profile runs the same Rust producer and real private
PipeWire transport through `vapostproc` and `vah264enc`. It requires those VA
plugins, `h264parse`, `vah264dec` and a VA driver supporting the requested
modifier. The selected render node must match the conversion, encoding and
decoding elements; another GPU is not silently accepted.

For the tested Fedora/Lunar Lake installation, the codec-capable VA driver
is selected explicitly:

```sh
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 va-h264
```

These driver paths are machine-specific. The wrapper uses a fresh GStreamer
registry in each log directory so plugin discovery reflects the chosen driver.
Missing codec support or incompatible formats fail the test; there is no
software-encoder fallback.

This profile describes the Vulkan image as ARGB with producer-written opaque
alpha. On this device VA conversion accepts tiled ARGB but not tiled XRGB;
the raw profile retains XRGB. Conversion must produce independent VA-memory
NV12 images before encoding. A held input remains retained through six encoded
outputs, so a successful run exercises subsequent publications while that
input is unavailable for rewriting.

The encoder disables B-frames, requests constrained-baseline byte-stream access
units, and supplies parameter sets with keyframes. Validation checks the caps,
decode timestamps no later than presentation, exact fixture presentation
intervals and IDR/SPS/PPS presence on keyframes. These checks are not a full
H.264 dependency parser or Chromecast receiver qualification.

After source shutdown and native retirement, a separate test oracle decodes
the twenty access units on the selected GPU. It maps only decoded oracle
images and verifies every RGB pixel, in order, with a six-level channel
tolerance for conversion and codec rounding, including rectangle edges. Its
test-only NV12-to-RGB conversion explicitly uses nearest-neighbor interpolation
to preserve the sharp-edged fixture's chroma boundaries. The encoder conversion
retains its default interpolation; the oracle does not qualify other decoder
resampling filters. It also requires exactly twenty
images and decoder end-of-stream. Every publication has a distinct RGB color;
tests require disjoint tolerance ranges between all twenty colors and the
black/white overwrite values. A stale image must fail even if its sequence
metadata is current. At each coordinate, the topmost visible layer supplies the
expected color; pixels outside all visible layers must remain black.
Matching pixels after both source and staging rewrites
detect reads that incorrectly outlive those copy boundaries. CPU readback
belongs to this oracle, not to the capture-to-encoder path. Encoded access units
are ordinary CPU-owned bytes.

## Transient sandbox experiment

Append `sandbox` to run the compiled fixture in a collected transient user
service. Cargo and the private PipeWire server remain outside the sandbox:

```sh
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 va-h264 sandbox
```

The service retains memory-execution restrictions, the system-service syscall
filter, no-new-privileges and a private device namespace. It binds only the
selected render node, with a narrow device allow rule, and the private fixture
socket directory. The process has no effective capabilities, IPv4 or IPv6
socket access, primary DRM nodes or user-home access. It uses private temporary
storage for plugin and shader caches. Only explicitly supplied VA-driver
environment overrides are forwarded; the host launcher's validation-layer
environment is not forwarded by this wrapper.

Before opening Vulkan, the fixture checks its effective process flags, memory
execution policy, network denial and visible DRM nodes. The following control
uses the same restrictions without binding or allowing the render node:

```sh
sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 raw sandbox-denied
```

That control succeeds only when the restrictions hold and selected-device
access is denied; it deliberately does not create a GPU or media graph. Both
variants require a working user service manager and the relevant sandbox
support. Unsupported restrictions are failures, not permission to run on the
host instead. Unit runtime and stop limits bound foreign-library hangs.

This is a deployment-feasibility experiment, not the installed backend. The
fixture combines generated-image production and encoding in one non-networked
process, uses the development socket policy, and has no capture/executor
capabilities. It does not qualify the eventual process split, system-service
identity, shader-based composition, device-reset recovery or receiver traffic.
No installed service unit is edited or restarted.
