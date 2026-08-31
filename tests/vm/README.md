# VM live gates

## Mutter grant-owner handoff

`run-grant-owner-handoff-gate` verifies that capture authority follows the
current owner of `io.github.pronkproject.Pronk1`, rather than the lifetime of a
connection that owned the name previously. The live client acquires a grant,
releases the well-known name without disconnecting, transfers the name to a
second connection, and requires the old holder to become terminal before the
replacement owner acquires a fresh grant for the same connector.

```sh
tests/vm/run-grant-owner-handoff-gate \
  target/debug/pronk-castkms-live-test \
  /dev/dri/card1 SESSION CONNECTOR-ID CRTC-ID
```

Run it in the same graphical CastKMS VM used by the other direct live gates.
The test does not attach a monitor or start capture, so it isolates brokered
grant ownership from display and media lifecycle behavior.

## Whole-daemon media and recovery

`run-whole-daemon-video-gate` is the opt-in production-path media and recovery
gate. It must run inside a VM with an active graphical session, the production
Pronk payload and PipeWire/WirePlumber policy installed, WirePlumber 0.5.15 or
newer active, and CastKMS loaded with at least one unassigned output. It
deliberately contacts and interrupts a real Google Cast Device.

The Device is selected explicitly from `pronkctl list-devices`; the gate never
chooses an arbitrary discovered Device:

```sh
PRONK_VM_WHOLE_DAEMON_GATE=real-device \
tests/vm/run-whole-daemon-video-gate \
  chromiacast:70ba349a7a140f3d38089c7b5f2a5eb6
```

By default the gate drives `pronkctl add-display --no-audio`. Set
`PRONK_VM_AUDIO=true` to exercise the production A/V path instead. In that mode,
Pronk must resolve the exact connector-bound CastKMS sink and the backend must
capture its monitor, encode a 48 kHz stereo Opus packet, submit both streams,
and receive Cast Device acknowledgements for both before `StartMedia` succeeds.
The gate waits for GNOME to route the new CastKMS monitor and requires the
public per-display MediaSession state, read through `pronkctl list-displays`, to
reach `Running`. It then kills the socket-activated backend and restarts
PipeWire in turn. Before fault injection, the public `Running` state must remain
stable beyond the sender's post-acknowledgement watchdog interval, which
rejects a one-frame startup followed by Device silence. After each failure, the
same display must remain attached, cross a recovery state, and return to public
`Running` through a fresh media generation. The journal is consulted only to
observe the deliberately brief recovery edge; steady-state assertions use the
versioned D-Bus projection. Finally, the gate removes the display and lets the
normal ordered teardown path stop the Cast application.

Mutter can retain a saved two-monitor logical layout after CastKMS has detached
the connector and disabled its kernel CRTC. If applying that already-cached
layout does not produce an active Pronk route within five seconds, the gate
forces `1280x720@60.000` and then restores the requested cast mode. Override the
distinct intermediate mode with `PRONK_VM_ROUTE_PRIME_MODE` when testing a
different bounded mode set. Every desktop gate first sets Mutter's
`PowerSaveMode` to on through the graphical session bus. When display power is
off, Mutter records a hotplug and logical monitor but deliberately defers its
modeset—and therefore CastKMS grant activation—until the session wakes. Closing
the DRM fd while every connector is disconnected is a normal resource-lifetime
choice; Mutter reopens the persistent connector on a later hotplug without a
display-manager restart.

For the A/V gate:

```sh
PRONK_VM_WHOLE_DAEMON_GATE=real-device \
PRONK_VM_AUDIO=true \
tests/vm/run-whole-daemon-video-gate \
  chromiacast:70ba349a7a140f3d38089c7b5f2a5eb6
```

For a human-visible check, set `PRONK_VM_CAST_PRIMARY=true` and
`PRONK_VM_VISIBLE_HOLD_SECONDS=N`. The gate makes the CastKMS output the VM
guest's primary monitor and holds the initial transport open for up to 900
seconds before running its recovery and teardown checks. This is an observation
window, not an automated assertion that the physical display decoded and
presented the video. A black secondary desktop is not a useful visual fixture:
for presentation sign-off, place a deterministic non-black compositor surface
on the Cast output before media reaches `Running`, do not reconfigure the
monitor topology during the hold, and record the physical-display result
separately from the transport gate. Changing topology after startup may revoke
the active capture route and intentionally return the Device to its home screen.

The script refuses a host or container, refuses active Pronk services and an
existing bus owner, and does not install, replace, or delete system payload.
This keeps provisioning separate from the product lifecycle assertion and
makes it safe to rerun in a disposable desktop VM.

Fedora 43 currently packages WirePlumber 0.5.14. For this gate, build the
upstream `0.5.15` tag with prefix `/opt/pronk-wireplumber-0.5.15` and
`libdir=lib64`, then install
`wireplumber-0.5.15.override.conf` as
`~/.config/systemd/user/wireplumber.service.d/pronk-live-gate.conf`. The
override keeps the distribution package intact while making the user service
load the matching 0.5.15 library, base configuration, scripts, and modules.
Copy Pronk's WirePlumber fragment and its two Lua scripts into that prefix's
`share/wireplumber` tree before restarting the service. Keeping all three
WirePlumber search paths on the same prefix avoids mixing the newer daemon and
permission-manager API with distribution-provided 0.5.14 data files.

## Direct GStreamer DMA-BUF lifetime

`run-pipewire-gstreamer-bufferpool-gate` is the deterministic, TV-independent
test for the shared video-buffer path. It attaches a real CastKMS output, starts
the classified Pronk PipeWire producer, and targets it with stock
`pipewiresrc use-bufferpool=true`. The gate requires all four caller-owned
buffers to be wrapped as DMA-BUF memory, 30 buffers to return asynchronously,
and the production BGRx-to-I420-to-H.264 shape to reach EOS before the grant and
pool are destroyed.

```sh
tests/vm/run-pipewire-gstreamer-bufferpool-gate \
  target/debug/pronk-castkms-live-test \
  "$(command -v gst-launch-1.0)" \
  /dev/dri/card1 SESSION CONNECTOR-ID CRTC-ID
```

Run it from a fresh disconnected CastKMS fixture with the classified PipeWire
and WirePlumber policy active. The runner wakes Mutter before attaching the
monitor, so an idle guest cannot turn a deferred modeset into a grant timeout.

## HDMI-CEC control and lifetime

`run-cec-control-gate` exercises the production kernel-to-backend CEC path.
Run the deterministic mock mode first:

```sh
PRONK_VM_CEC_GATE=mock tests/vm/run-cec-control-gate
```

The VM must have an active graphical session, the current Pronk payload and
mock backend installed, `cec-ctl` and `gdctl` available, and CastKMS loaded
with CEC enabled. In particular, `/dev/cec0` must exist and this parameter must
report `Y`:

```sh
cat /sys/module/castkms/parameters/enable_cec
```

The mock gate attaches `mock:living-room`, waits for grant authorization and
the CEC transport, claims playback logical address 4, and transmits paired
volume-up/down pressed and released commands to logical address 5. It requires
backend-completed kernel transmits before and after route disablement and two
different Mutter modesets. It then removes the display and requires physical
address `f.f.f.f`, an empty logical-address mask, and no allocated logical
address. Thus a passing result covers control translation, backend completion,
CEC independence from the media route, modeset survival, and detach cleanup.

Mock mode makes the CastKMS monitor primary while perturbing the topology so
Mutter cannot satisfy the request from a cached disconnected-monitor layout
without a real DRM route. Set `PRONK_VM_CEC_CAST_PRIMARY=false` only when the
fixture already guarantees that the requested modes cause real modesets. The
real-Device mode leaves the VM's local output primary by default.

After the deterministic gate passes, an explicit same-network Device can be
used to exercise Chromiacast control:

```sh
PRONK_VM_CEC_GATE=real-device \
tests/vm/run-cec-control-gate \
  chromiacast:70ba349a7a140f3d38089c7b5f2a5eb6
```

This mode deliberately interrupts whatever the selected TV is doing, starts a
Cast session, and changes its volume up and down. It never chooses a discovered
Device implicitly. Use only an exact Device ID selected from
`pronkctl list-devices` and only when interrupting that Device is acceptable.

Like the media gate, the CEC gate refuses hosts, containers, an existing Pronk
bus owner, or active selected services. It installs nothing and always removes
its display and stops the services it started. A final successful CEC transmit
may include an earlier retry error count alongside `Tx, OK` when grant or
topology authority changed between attempts; the gate requires final `OK` for
both messages and rejects a terminal transmit error.
