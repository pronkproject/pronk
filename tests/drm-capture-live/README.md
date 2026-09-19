# Rust capture qualification

These opt-in tests modeset a display. Use an **unused Rust CastKMS device in
a disposable VM**, not a desktop output. The fixture requires DRM master and
the Rust driver's version; heap tests also require `/dev/dma_heap/system`.
The kernel must provide the matching generic capture interface and the
built-in reference renderer. Fixture probes select the first connected output
offering 640x480 and find its compatible primary plane; additional outputs,
cursors and overlays may remain enabled. They do not test composition of those
additional planes. Building also requires libdrm and GTK 3 development
files; GTK supplies the Wayland pattern client. Build the programs before
entering a privileged test environment:

The VM must have a connected CastKMS monitor with a 640x480 mode before the
basic fixture starts. Merely loading the driver creates no connected output;
an administrative monitor attachment or the isolated Mutter/Pronk setup must
remain alive for the duration of the probe.

For the standalone VM probes, the kernel selftest utility `monitor-run` can
hold the fallback monitor while giving the child process DRM master:

```sh
monitor-run /dev/dri/cardN pronk-drm-capture-live-test /dev/dri/cardN
```

The same wrapper can run `run-pipewire.sh` or `run-pipeline.sh` with their
usual arguments. It detaches the monitor after the child exits.

```sh
cargo build --locked -p pronk-drm-capture-live-test
```

The binaries cover distinct boundaries:

- `pronk-drm-capture-live-test /dev/dri/cardN`: client ioctls, changing pixels,
  backpressure, cancellation, revocation, and restart.
- `pronk-capture-actor-live-test /dev/dri/cardN`: rejected partial registration,
  recovery using the same grant, fresh heap destinations,
  held frames, ordinary display replacement, repeated media generations under
  one grant, and retained storage across a new authorization.
- `pronk-capture-handoff-live-test /dev/dri/cardN`: real capture frames with
  **synthetic** PipeWire events, including stale releases and retirement.
- `pronk-capture-broker-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID`:
  acquisition through a live Mutter broker, managed monitor attachment, actor
  capture, renderer startup snapshot, explicit release, revocation of retained
  descriptors, and reacquisition. It requires an isolated session bus whose
  Mutter owns the active output. It does not open a DRM primary descriptor or
  submit a modeset directly, and refuses to replace an existing owner of the
  Pronk bus name. Unlike the fixture probes, Mutter must already be displaying
  content. Obtain the exact output IDs from that test device.
- Delegated GPU rendering:
  `pronk-renderer-capture-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID
  WIDTH HEIGHT REFRESH_MILLIHZ MODIFIER
  /path/to/pipewire-0-pronk-backend`
  transfers renderer authority from the Mutter broker into the application
  capture port, publishes renderer constraints after private GPU setup, waits
  for Mutter to select the new constraints entry, and then starts the generic
  capture and PipeWire path. It requires twelve increasing DMA-BUF frame
  sequences while one output remains held, withdraws the offer, and repeats the
  complete renderer generation on the same display session. Supply a supported
  capture-output modifier, optionally with a `0x` prefix. By default the
  renderer's private images use that modifier too; `--private-modifier HEX`
  selects a different private-image modifier when the renderer and encoder
  need distinct layouts.
  `--raw-format FOURCC` selects the exact capture output order when the
  default `XR24` is not accepted by the media device. Receiver mode requires
  `--va-render-node /dev/dri/renderDN`; it checks that the VA encoder and
  renderer use the same device and that the encoder accepts the output's
  format, modifier, picture size and bitrate before starting Cast. There is
  no software-encoder fallback for the DMA-BUF renderer target.
  Like the live Mutter media probe, it requires the sibling pattern client,
  the classified core/backend sockets, and the versioned WirePlumber policy.
  The compositor and Vulkan worker must also be able to import each other's
  DMA-BUFs through the selected GPU driver. A virtual GPU that accelerates
  OpenGL and Vulkan in separate contexts qualifies only when buffers can cross
  that boundary; successful renderer discovery and selection are not enough.
- `pronk-capture-pipewire-live-test /dev/dri/cardN /path/to/private/socket`:
  twelve real frames through PipeWire and GStreamer, checking every pixel,
  retained DMA-BUF memory, changing content, and a held sample across six
  arrivals. The source uses the same socket via `PIPEWIRE_REMOTE`.
- `pronk-capture-encoded-live-test /dev/dri/cardN /path/to/private/socket`:
  capture through the production `pronk-media` H.264 actor and a local decoder.
  It requires an initial key frame, increasing encoded timestamps, and at
  least twelve decoded images including changed display content. Pixel checks
  allow a small tolerance for lossy encoding. It uses the software encoder.
- `pronk-capture-video-live-test /dev/dri/cardN /path/to/private/socket`:
  the continuous `Video` owner drives capture without a test-managed frame
  loop. A real consumer verifies changing pixels and a held sample, followed
  by joined shutdown and the owner's terminal state.
- `pronk-capture-pipeline-live-test /dev/dri/cardN /path/to/private/socket`:
  the application's `DrmCapturePipeline` runs three media generations under
  one grant. It checks activation, changing pixels, retained storage across
  generations, and orderly stop without a false health event. It then revokes
  a separate grant during active delivery, checks its exact failure generation
  and retained pixels, and resumes delivery through fresh authority and fresh
  output storage. The wrapper also restarts its isolated PipeWire server and
  WirePlumber policy during active delivery; the same grant must report the
  transport failure and start another generation after service recovery. The
  test does not qualify broker issuance, hardware encoding, or a receiver.
- Live Mutter media:
  `pronk-capture-mutter-media-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID
  WIDTH HEIGHT /path/to/pipewire-0-pronk-backend` runs a fullscreen Wayland
  client whose image changes under the disposable Mutter.
  The probe obtains authority from Mutter through the application capture
  port, configures the production H.264 actor, and only then admits capture
  frames. It requires twelve decoded images and both known colors at the
  requested output dimensions.
  The sibling `pronk-capture-pattern-client` executable must be built alongside
  the probe. It never opens a DRM primary descriptor or replaces a bus owner.
  Use an isolated session bus and the disposable compositor's Wayland socket.
  Its PipeWire server must expose the sibling `pipewire-0-pronk-core` socket
  and run Pronk's versioned WirePlumber policy. The policy authorizes the two
  classified clients and links their exact private nodes. The two-argument
  wrapper below does not start Mutter, WirePlumber, or that probe.
- `pronk-capture-idle-revoke-live-test /dev/dri/cardN /path/to/private/socket`:
  revoke a grant before any consumer attaches to its PipeWire source. The
  video owner must report failure and join shutdown without waiting for a
  capture request or returning a successful capture owner. The source uses
  `PIPEWIRE_REMOTE`, as with the other two-argument probes.

For the media probes, the wrapper starts an isolated PipeWire server, runs
the supplied already-built executable, and stops only that server:

```sh
sh tests/drm-capture-live/run-pipewire.sh \
    /path/to/pronk-capture-pipewire-live-test /dev/dri/cardN
```

It requires `pipewire`, `pw-link`, `timeout`, GStreamer and its PipeWire plugin,
and a writable `/var/tmp`. Logs remain in a fresh directory printed by the
wrapper. Its server configuration is shared with the generated-GPU test;
no system PipeWire instance or installed policy is changed.

Pass the encoded probe instead to exercise H.264; that additionally requires
the `x264enc` and `h264parse` GStreamer plugins, plus either `avdec_h264` or
`openh264dec`. Encoder startup is driven concurrently with capture because
the media actor acknowledges
startup only after receiving media.

For the application pipeline probe, use the wrapper that also starts the
versioned WirePlumber policy in an isolated configuration:

```sh
sh tests/drm-capture-live/run-pipeline.sh \
    /path/to/pronk-capture-pipeline-live-test /dev/dri/cardN
```

It additionally requires WirePlumber 0.5.15 or newer, `pw-dump`, and `jq`.
The private server and policy are stopped afterward; installed configuration
and host services are not changed. WirePlumber links the consumer to its exact
producer target while both use their classified sockets. Run it only in the
disposable VM, like the other fixture probes.

## Optional receiver test

Only after arranging permission to interrupt a specific receiver, append
`--receiver IP:PORT` to the live Mutter media or delegated-renderer probe.
There is no automatic receiver selection. For example, inside the disposable
compositor environment:

```sh
pronk-capture-mutter-media-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID \
    WIDTH HEIGHT /path/to/pipewire-0-pronk-backend \
    --receiver RECEIVER_IP:8009

pronk-renderer-capture-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID \
    WIDTH HEIGHT REFRESH_MILLIHZ MODIFIER \
    /path/to/pipewire-0-pronk-backend --raw-format FOURCC \
    --private-modifier PRIVATE_MODIFIER \
    --va-render-node /dev/dri/renderDN --receiver RECEIVER_IP:8009
```

Select a `FOURCC` and output `MODIFIER` accepted by the renderer's output
allocator and the selected VA converter. The optional `PRIVATE_MODIFIER`
must be supported by the renderer's private-image allocator. The probe checks
the converter's exact input tuple but cannot select a replacement allocation
itself. The Mutter capture probe still uses software H.264 from mapped frames, while the
delegated-renderer probe uses VA H.264 from DMA-BUF frames.

The probe authenticates the receiver and launches its mirroring application,
**replacing current playback**. It offers the captured mode as H.264 at 30 fps,
4 Mbit/s and 400 ms target delay, rejecting incompatible receiver constraints.
It forwards the production encoder's access units and requests key frames on
feedback. After twelve locally decoded frames and both pattern colors, the
pixel oracle stops decoding so it cannot limit the rest of the run. For at
least fifteen seconds after media activation, the probe requires 24 encoded
and acknowledged frames per second on average, measured against the actual
elapsed time. Any encoded output loss fails the run. Normal completion, error,
Ctrl-C, and the probe timeout all attempt to stop the application; that does
not restore whatever the receiver was previously playing.

The rate check is transport evidence, not a display frame-rate measurement.
Local decoding confirms changing content only in its initial bounded sample;
receiver acknowledgements do not prove that the television displays later
changes or all acknowledged frames. Observe the receiver before claiming
visible end-to-end playback. The VM needs outbound TCP and bidirectional UDP;
an explicit endpoint avoids relying on multicast discovery through NAT.

Set `RUST_LOG=chromiacast::control=trace` to record the negotiation message
types and routing when diagnosing an offer timeout. Logging goes to stderr.
Keep VM stdout and stderr in separate host files if the VM runner opens
independent file handles for them; redirecting both onto one file can overwrite
parts of the record.

The receiver helper is in `tests/capture-receiver` and accepts encoded access
units, not capture descriptors or raw images. The qualification executable
combines capture and networking only for testing; it is not the installed
backend's process or sandbox boundary.

The final-image capture probes use reference CPU composition. The renderer
capture probe explicitly selects DMA-BUF storage and qualifies delegated GPU
composition into a Vulkan-allocated destination through generic final-image
delivery. Its receiver mode carries that output through the production media
graph and a selected VA H.264 encoder. The live probe verifies the graph's
reported VA-memory path and render device after the run. It does not qualify
the installed service sandbox. The default probes do not exercise receiver
transport; either optional receiver mode still needs visual confirmation to
establish television playback.
