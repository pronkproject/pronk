# Rust capture qualification

These opt-in tests modeset a display. Use an **unused Rust CastKMS device in
a disposable VM**, not a desktop output. The fixture requires DRM master and
the Rust driver's version; heap tests also require `/dev/dma_heap/system`.
The kernel must provide the matching generic capture interface and the
built-in reference renderer. Build the programs before entering a privileged
test environment:

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
- `pronk-capture-pipewire-live-test /dev/dri/cardN /path/to/private/socket`:
  twelve real frames through PipeWire and GStreamer, checking every pixel,
  retained DMA-BUF memory, changing content, and a held sample across six
  arrivals. The source uses the same socket via `PIPEWIRE_REMOTE`.
- `pronk-capture-encoded-live-test /dev/dri/cardN /path/to/private/socket`:
  capture through the production `pronk-media` H.264 actor and a local decoder.
  It requires an initial key frame, increasing encoded timestamps, and at
  least twelve decoded images including changed display content. Pixel checks
  allow a small tolerance for lossy encoding. It uses the software encoder.

For either media probe, the wrapper starts an isolated PipeWire server, runs
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
composition, hardware encoding, the installed service sandbox, Mutter's live
broker, receiver decoding, or end-to-end television playback.
