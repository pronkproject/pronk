# Generated GPU frames through PipeWire

This opt-in harness connects the real Rust Vulkan producer, output pool,
`GpuOutput` adapter and `VideoSourceActor` to a GStreamer PipeWire consumer.
It uses generated images, not a capture grant or compositor sources.

Run from the repository root with explicit GPU and hexadecimal modifier:

```sh
sh tests/gpu-media/run-private.sh /dev/dri/renderD128 0100000000000009
```

That tuple names the Lunar Lake development device, not a portable default.
Requirements include the Rust toolchain, Vulkan shared-image support, PipeWire,
`pw-link`, GStreamer with its PipeWire plugin, `timeout` and `rg`. Native
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

Four persistent, single-plane 1920x1080 modifier images are rewritten with
changing colors and published twenty times. Native rendering and allocation
run on blocking workers, not on the PipeWire loop. Returned buffers pass through
the real source actor, publication correlation and native reuse checks before
another write. The first received sample is retained through six arrivals;
releases before sample disposal or while that sample is held fail the test.
All four images must be rewritten, and every sequence must arrive once in order.
The consumer requires DMA-BUF memory and never maps raw pixels.

Successful output reports publication count and per-slot uses. Thirty-fps
timestamps are fixture configuration, not a measurement of delivered cadence.
The harness does not inspect pixel contents, run an encoder or receiver, prove
an unsignaled native-reader stall, or simulate device loss. The Vulkan unit
test provides the separate generated-pixel readback oracle. Frame metadata and
DMA-BUF memory checks here do not establish that every library or driver avoids
all internal CPU access.
