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
routes a visible desktop mode through Mutter, verifies changing video on the
private PipeWire connection, and then removes the display. Set
`PRONK_VM_VISIBLE_HOLD_SECONDS` to leave the image visible for manual receiver
inspection before teardown.

Audio can be requested with `PRONK_VM_AUDIO=true` once the session broker and
installed kernel expose the audio capability. The default gate remains
video-only so that absence of optional audio support does not hide a video-path
regression.
