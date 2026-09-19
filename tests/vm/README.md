# Virtual-machine media gate

`run-whole-daemon-video-gate` exercises the installed session service from
device discovery through CastKMS attachment, capture, private PipeWire
transport, encoding, and delivery to a real receiver. Run it inside a graphical
login whose Mutter provides the CastKMS session broker.

The gate contacts the selected receiver and can interrupt its current content.
Name the exact backend device and opt in explicitly:

```sh
PRONK_VM_WHOLE_DAEMON_GATE=real-device \
  tests/vm/run-whole-daemon-video-gate chromiacast:DEVICE-ID
```

The script starts the user service and backend socket, creates a cast display,
routes a visible desktop mode through Mutter, checks that encoded media reaches
the receiver and remains active beyond its feedback watchdog, and then removes
the display. It does not inspect decoded pixels. Set
`PRONK_VM_VISIBLE_HOLD_SECONDS` to leave the image visible for manual receiver
inspection before teardown.

To qualify the installed hardware path, configure the Chromecast backend's
systemd drop-in with the selected VA H.264 render node as described in
`docs/gpu-media.md`, then set `PRONK_VM_REQUIRE_VA_RENDER_NODE` to that exact
node when running the gate. The gate checks the backend's configured GStreamer
encoder, DMA-BUF-to-VA memory path and render-node report for the initial run,
backend replacement and PipeWire recovery. Without this opt-in, a successful
run does not establish that hardware encoding was used. This report does not
measure whether the receiver displays every acknowledged frame.

Audio can be requested with `PRONK_VM_AUDIO=true` once the session broker and
installed kernel expose the audio capability. The default gate remains
video-only so that absence of optional audio support does not hide a video-path
regression.
