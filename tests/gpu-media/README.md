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

Four generated single-memory-plane modifier images belong to a separate
producer Vulkan device instance: a 1920x1080 base, a 640x480 overlay, a
128x128 cursor-sized layer and a 128x64 RGB565 patch. The base uses packed
ten-bit BGR with two alpha bits, the overlay uses eight-bit RGBA and the
cursor-sized layer uses eight-bit BGRA.
All fixture pixels are opaque. Exact per-format allocation checks apply to the
selected modifier; unsupported source tuples fail without format substitution.
The worker checks matching physical-device
and driver identities, imports each source use with exact allocator metadata
and its explicit producer fence, and copies into independently allocated,
non-exportable floating-point input images. It then overwrites every original
source white before shader composition. A third Vulkan device owns the four
persistent output images; none of those allocations is imported on the source
worker. The composed private image is converted into a fresh packed bridge
allocation. Its exported descriptor remains internal to the renderer, and its
source-side Vulkan image and memory owners are destroyed before the output
device imports it. The output device copies the bridge into a persistent output
image and destroys the import after completion. The private composition image
is overwritten black before publication. Only the four output allocations are
registered with PipeWire. They remain eight-bit packed RGB in the selected
output channel order, regardless of source precision; the hardware H.264
profile is not ten-bit or HDR.
Private input and composition allocations are created before source admission
and reused across frames; they have no export API or external reuse dependency.
A single immutable blend program is created alongside that storage and retained
across every layer and frame. Each native operation still owns its own image
views and descriptors; source-use accounting retains no shader program state.
A separately retained gamma program inverts the green channel after blending,
including the uncovered background. Its two-entry table is uploaded once before
source admission. The decoded-image oracle applies the CPU reference table to
expected colors; leaving color unchanged, reading overwritten sources, or
publishing overwritten private storage cannot satisfy those expectations.

The internal bridge adds an allocation and a GPU transfer per frame. Its backing
storage survives the source-side owner through the retained DMA-BUF descriptor,
without retaining a compositor-source import. Separate logical devices and
owner destruction establish the application-side boundary, not a portable proof
of native virtual-memory unbind completion or isolation from reservation,
eviction and scheduling dependencies. Those require driver-specific observation
and stalled-consumer tests. Shared GPU execution time remains shared even when
buffer lifetimes are independent.

The base source selects a 1856x1024 crop starting at (32,16), placed over a
black 1920x1080 background. Placements cycle through (-32,16), (32,-16) and
(64,32), exercising left clipping, top clipping and an inset rectangle. The
independent geometry model supplies expected visible rectangles; unit tests
check those against literal source and destination coordinates. The overlay
starts at (640,320), and the top plane moves between (608,288), (672,288) and
(736,288), overlapping both the overlay and exposed base. Layers have distinct
frame-dependent colors and opaque alpha. The RGB565 patch is solid red at
(32,864), covering base pixels and, in one placement, part of the background.
Its endpoint channels are exact at both pixel depths; it remains distinct from
background and overwritten source colors after the gamma operation.
Private source copies have independent
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

Each four-source operation reserves four `SourceUse<Option<SyncFile>>`
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
The four-record budget matches the fixture's source-copy operations, not a
frame or transport limit. No executor ioctl or kernel release message is
exercised here; this
connects the trusted accounting library to actual generated-source GPU work.

Successful output reports publication count and per-slot uses. Thirty-fps
timestamps are fixture configuration, not a measurement of delivered cadence.
The renderer also reports nearest-rank p50/p95/max host durations over the
successful frames. Generated-source submission includes fixture clears,
producer waits and native read submission. Remaining-source wait starts only
after accounting collection; it excludes the coordinator's intervening delay.
Private composition includes background initialization and blend operations.
Private gamma measures the subsequent color operation separately;
shared-output copying includes bridge allocation, format conversion, device
handoff, final copying and any destination wait. Test-only source and
composed-image overwrites have a separate total. Those intervals are not GPU
timestamps, a full source-retention interval, or end-to-end presentation timing.
The twenty-frame sample includes startup effects. Device, modifier, validation
layers and media profile must be recorded before comparing runs.
The default `raw` profile does not inspect pixel contents. None of the profiles
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

The `production-va-h264` profile replaces that fixture-owned consumer graph
with `pronk-media`'s `MediaGraphActor` and `VideoEncoder::VaH264` path. It
checks the actor's generation-scoped access units and reported encoder,
VA-memory path and render device before sending the same bytes through the
hardware-decoding pixel oracle. This is the preferred qualification profile;
`va-h264` remains a smaller transport and plugin diagnostic.

For the tested Fedora/Lunar Lake installation, the codec-capable VA driver
is selected explicitly:

```sh
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 production-va-h264
```

These driver paths are machine-specific. The wrapper uses a fresh GStreamer
registry in each log directory so plugin discovery reflects the chosen driver.
Missing codec support or incompatible formats fail the test; there is no
software-encoder fallback.

To inspect the exact GPU/VA layout overlap at every initially offered Cast
picture size, run the opt-in qualification test with the same render node:

```sh
PRONK_GPU_RENDER_NODE=/dev/dri/renderD128 \
PRONK_EXPECT_SHARED_MODE=3840x2160 \
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    cargo test -p pronk-gpu-media-test --features native \
    --test hardware_offer -- --ignored --nocapture
```

The test allocates, exports and reimports disposable Vulkan images for each
candidate modifier, then intersects them with the selected VA converter's
advertised DMA-BUF formats. It reports per-size and common layouts. Omit
`PRONK_EXPECT_SHARED_MODE` to inspect another machine without requiring 4K;
the test still cannot guarantee that every later VA frame import succeeds.

This profile describes the Vulkan image as ARGB with producer-written opaque
alpha. On this device VA conversion accepts tiled ARGB but not tiled XRGB;
the raw profile retains XRGB. Conversion must produce independent VA-memory
NV12 images before encoding. In the fixture-owned `va-h264` graph, a held input
remains retained through six encoded outputs, so a successful diagnostic run
exercises subsequent publications while that input is unavailable for
rewriting. The production profile instead exercises the production queue and
buffer-return policy.

An explicitly linear ARGB DMA-BUF does not link to this device's VA converter:
its advertised DMA-BUF sink caps list the tested tiled modifier but not linear
ARGB. The installed backend intersects exact converter caps with Pronk's GPU
offer, so that unsupported tuple is excluded before a session is prepared.
The fixture reports a converter-link error promptly if it is requested anyway.

The optional fifth argument selects an exact output fourcc: `XR24`, `AR24`,
`XB24` or `AB24`. The raw profile defaults to `XR24`; encoded profiles default
to `AR24`. The private bridge, PipeWire caps and VA input all follow that exact
choice. A selected converter need not accept every fourcc it can render: this
machine's VA converter advertises tiled `AR24`, `XB24` and `AB24`, but not tiled
`XR24`. The production test checks decoded pixel order for each supported
choice:

```sh
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 production-va-h264 sandbox AB24
```

The same production and sandbox checks also pass with `XB24` on this device.

The optional sixth argument chooses the output size: `1920x1080` (the
default), `2560x1440` or `3840x2160`. Source layers retain their fixed
geometry, so larger runs also check the uncovered background. Their longer
bounded runtime covers the full decoded-pixel oracle rather than relaxing its
checks. For example:

```sh
LIBVA_DRIVERS_PATH=/usr/lib64/dri-nonfree LIBVA_DRIVER_NAME=iHD \
    sh tests/gpu-media/run-private.sh /dev/dri/renderD128 \
    0100000000000009 production-va-h264 sandbox AR24 3840x2160
```

The encoder disables B-frames, requests constrained-baseline byte-stream access
units, and supplies parameter sets with keyframes. Validation checks the caps,
decode timestamps no later than presentation, exact fixture presentation
intervals and IDR/SPS/PPS presence on keyframes. These checks are not a full
H.264 dependency parser or Chromecast receiver qualification.

The production oracle uses 20 Mbit/s so sharp synthetic color boundaries remain
useful pixel evidence after lossy encoding. This is test configuration, not the
Chromecast bitrate policy. The production graph may discard work in its bounded,
leaky raw queue; the test requires at least twelve useful access units and
reports how many inputs that queue discarded. The encoded sink applies
backpressure instead of dropping access units. The test preserves source
sequence numbers and rejects any reported or observed loss after the encoder.

After source shutdown and native retirement, a separate test oracle decodes
the surviving access units on the selected GPU. It maps only decoded oracle
images and verifies every RGB pixel against its original scene, with a
six-level channel tolerance for conversion and codec rounding. A narrow band
around layer edges permits sixteen levels for chroma-subsampling bleed. Its
test-only NV12-to-RGB conversion explicitly uses nearest-neighbor interpolation
to preserve the sharp-edged fixture's chroma boundaries. The encoder conversion
retains its default interpolation; the oracle does not qualify other decoder
resampling filters. It also requires decoder end-of-stream. Every publication
has a distinct RGB color;
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
    0100000000000009 production-va-h264 sandbox
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
identity, arbitrary compositor scenes, device-reset recovery or receiver traffic.
No installed service unit is edited or restarted.
See the installed hardware-encoder section in `docs/gpu-media.md` for the
per-instance device authorization that a qualified deployment must provide.
