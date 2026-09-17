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
  private-image modifier, optionally with a `0x` prefix.
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
the `x264enc`, `h264parse`, and `avdec_h264` GStreamer plugins. Encoder startup
is driven concurrently with capture because the media actor acknowledges
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
`--receiver IP:PORT` to the live Mutter media probe. There is no automatic
receiver selection. For example, inside the disposable compositor environment:

```sh
pronk-capture-mutter-media-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID \
    WIDTH HEIGHT /path/to/pipewire-0-pronk-backend \
    --receiver RECEIVER_IP:8009
```

The probe authenticates the receiver and launches its mirroring application,
**replacing current playback**. It offers the captured mode as H.264 at 30 fps,
4 Mbit/s and 400 ms target delay, rejecting incompatible receiver constraints.
It forwards the production encoder's access units, requests key frames on
feedback, and requires at least thirty acknowledged frames over a run of at
least fifteen seconds. A dropped encoded frame ends the probe rather than
continuing a broken dependency chain. Normal completion, error, Ctrl-C, and the
probe timeout all attempt to stop the application; that does not restore whatever
the receiver was previously playing.

Local decoding and receiver acknowledgements are distinct results. Neither
acknowledgements nor successful packet delivery prove that the television
displays the changing pattern. Observe the receiver before claiming visible
end-to-end playback. The VM needs outbound TCP and bidirectional UDP; an
explicit endpoint avoids relying on multicast discovery through NAT.

Set `RUST_LOG=chromiacast::control=trace` to record the negotiation message
types and routing when diagnosing an offer timeout. Logging goes to stderr.
Keep VM stdout and stderr in separate host files if the VM runner opens
independent file handles for them; redirecting both onto one file can overwrite
parts of the record.

The receiver helper is in `tests/capture-receiver` and accepts encoded access
units, not capture descriptors or raw images. The qualification executable
combines capture and networking only for testing; it is not the installed
backend's process or sandbox boundary.

The capture probes use reference CPU composition. The renderer capture probe
explicitly selects DMA-BUF storage and qualifies delegated GPU composition into
a Vulkan-allocated destination through generic final-image delivery. It does
not qualify hardware encoding or the installed service sandbox. The default
probes do not exercise receiver transport; even the optional receiver probe
needs visual confirmation to establish television playback.
