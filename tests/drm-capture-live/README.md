# Rust capture qualification

These opt-in tests modeset a display. Use an **unused Rust CastKMS device in
a disposable VM**, not a desktop output. The fixture requires DRM master and
the Rust driver's version; heap tests also require `/dev/dma_heap/system`.
The kernel must provide the matching generic capture interface and the
built-in reference renderer. Building also requires libdrm and GTK 3 development
files; GTK supplies the Wayland pattern client. Build the programs before
entering a privileged test environment:

```sh
cargo build --locked -p pronk-drm-capture-live-test
```

The binaries cover distinct boundaries:

- `pronk-drm-capture-live-test /dev/dri/cardN`: client ioctls, changing pixels,
  backpressure, cancellation, revocation, and restart.
- `pronk-capture-actor-live-test /dev/dri/cardN`: fresh heap destinations,
  held frames, ordinary display replacement, and retained old-session storage.
- `pronk-capture-handoff-live-test /dev/dri/cardN`: real capture frames with
  **synthetic** PipeWire events, including stale releases and retirement.
- `pronk-capture-broker-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID`:
  acquisition through a live Mutter broker, actor capture, explicit release,
  revocation of a retained capture descriptor, and reacquisition. It requires
  an isolated session bus whose Mutter owns the active output. It does not
  open a DRM primary descriptor or modeset, and refuses to replace an existing
  owner of the Pronk bus name. Unlike the fixture probes, Mutter must already
  be displaying content. Obtain the exact output IDs from that test device.
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
- `pronk-capture-mutter-media-live-test /dev/dri/cardN CRTC_ID CONNECTOR_ID /path/to/private/socket`:
  a fullscreen Wayland client changes its image under the disposable Mutter.
  The probe obtains authority from Mutter, starts the continuous capture owner,
  and feeds the production H.264 actor and local decoder. It requires twelve
  decoded images and both known colors at the offered output dimensions.
  The sibling `pronk-capture-pattern-client` executable must be built alongside
  the probe. It never opens a DRM primary descriptor or replaces a bus owner.
  Use an isolated session bus and the disposable compositor's Wayland socket.
  Set `PIPEWIRE_REMOTE` to the same private socket passed on the command line.
  The two-argument wrapper below does not start Mutter or run that probe.
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

These tests use reference CPU composition. They do not qualify delegated GPU
composition, hardware encoding, the installed service sandbox, receiver
decoding, or end-to-end television playback.
