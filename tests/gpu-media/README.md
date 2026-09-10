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

One generated, single-plane 1920x1080 modifier image belongs to a separate
producer Vulkan device instance. The worker checks matching physical-device
and driver identities, imports each source use with exact allocator metadata
and its explicit producer fence, and copies into available private staging.
It then overwrites the original source white before copying staging into one
of four persistent output images, and overwrites staging black before output
publication. Only those four output allocations are registered with PipeWire.

The twenty-publication sequence exercises source-to-private-to-output copying
and reuse without a real compositor source or capture grant. Source-reading
completion precedes downstream destination waits. Native rendering and
allocation run on blocking workers, not on the PipeWire loop. Returned buffers pass through
the real source actor, publication correlation and native reuse checks before
another write. The first received sample is retained through six arrivals;
releases before sample disposal or while that sample is held fail the test.
All four images must be rewritten, and every sequence must arrive once in order.
The transport consumer requires DMA-BUF memory and never maps raw pixels.

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
tolerance for conversion and codec rounding. It also requires exactly twenty
images and decoder end-of-stream. Matching pixels after both source and staging
rewrites detect reads that incorrectly outlive those copy boundaries. CPU
readback belongs to this oracle, not to
the capture-to-encoder path. Encoded access units are ordinary CPU-owned bytes.

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
