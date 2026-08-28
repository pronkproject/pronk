# Backend activation gate

The baseline test launches a small probe through
`systemd-socket-activate --accept`, connects to its Unix socket, and verifies
that the child receives exactly one connected stream named
`pronk-backend-control`. The probe consumes and unsets `LISTEN_*` in its
synchronous `main`, validates the descriptor, and only then constructs Tokio.

The P2P gate launches the real `pronk-backend-mock` the same way. Pronk's test
endpoint is the zbus authentication server and owns the generated GUID; the
backend is the client. Both sides require `EXTERNAL` plus the effective Unix
UID. The gate subscribes to all discovery signals before `StartDiscovery`,
installs the revision-2 snapshot, and proves the two queued initial signals are
contiguous and already covered by that snapshot. It also rejects stale
discovery and connection generations, creates and prepares a bounded session
without transferring an fd, requests clean shutdown, and waits for P2P EOF.

The same activation listener serves a second fresh process to prove
reactivation. Another listener deliberately launches a protocol-major-2 peer;
`RegisterBackend` rejects it and the backend exits. No grant, DRM descriptor,
PipeWire remote, or media descriptor participates in this gate.

The Chromiacast P2P gate launches the production backend executable with an
unmanaged-test-only Device/control fixture. It proves that authenticated,
versioned registration completes before discovery is available, obtains a
revisioned Device only after Start, creates a generation-bound session, and
returns bounded setup-endpoint make/model only after `Prepare`. The
fixture uses alternate UUID spellings for `deviceId` and `ssdp_udn`, so the
gate also exercises exact normalized selected-device cross-checking. It stops
and recreates the same session object path to prove bounded object lifetime.
The prepared session advertises control, accepts one normalized volume
operation, and must return the exact correlated `ControlCompleted` signal.
It also resolves `Sony Corporation` through the installed root-owned
`pnp.ids`, checks the deterministic Sony PNP code and DisplayID product name,
and runs upstream `edid-decode` before observing ordered backend shutdown and
control-stream EOF. Non-CEC fixtures must pass completely; the CEC fixture may
contain only the documented HDMI-VSDB/EDID-1.4 diagnostic and no other
conformance failure. Run it with:

```sh
cargo build -p pronk-chromiacast -p pronk-backend-activation-test
target/debug/pronk-chromiacast-p2p-test \
  "$(command -v systemd-socket-activate)" \
target/debug/pronk-chromiacast \
  "$(command -v edid-decode)"
```

The production Chromiacast media variant runs that same fixture inside the
disposable PipeWire/WirePlumber environment. It rejects a disconnected fd and
a nonexistent exact serial with ambient selection poisoned, then requires
validated constrained-baseline H.264 Annex-B access-unit progress, stable
counters while suspended, fresh progress on resume, and backend-client removal
after StopMedia. The graph itself rejects missing SPS/PPS/IDR framing,
non-advancing PTS, DTS reordering, or missing frame duration before those
counters can advance. The fixture transport then accepts the complete
generation-tagged units through the same bounded sender actor used by the
production Cast adapter; Start and Resume wait for that transport-side count,
not merely encoder output. The fixture then requests a key frame and advertises
an overfull Cast in-flight window. The gate requires generation-scoped
`KeyframeRequested` and `BitrateRequested` signals, observes the live encoder
downshift from 2.0 to 1.6 Mb/s, proves transport admission pauses, clears the
pressure snapshot, and requires delivery to resume from a newly forced key
frame. Run it with:

```sh
tests/backend-activation/run-media-session-gate \
  "$(command -v pipewire)" "$(command -v wireplumber)" \
  "$(command -v pw-cli)" "$(command -v timeout)" \
  "$(command -v gst-launch-1.0)" "$(command -v pw-dump)" \
  target/debug/pronk-chromiacast-p2p-test \
  "$(command -v systemd-socket-activate)" \
  target/debug/pronk-chromiacast \
  data/pipewire/80-pronk-remotes.conf \
  tests/backend-activation/80-pronk-media-gate-access.conf \
  "$(command -v edid-decode)"
```

The live preparation-only gate authenticates and cross-checks one selected
Device, queries its setup identity, resolves the expected PNP ID, validates the
generated DisplayID, and shuts down without launching a Cast application:

```sh
cargo build -p pronk-chromiacast -p pronk-backend-activation-test
PRONK_CHROMIACAST_LIVE_DEVICE_ID=00112233445566778899aabbccddeeff \
PRONK_CHROMIACAST_LIVE_EXPECTED_MANUFACTURER=TCL \
PRONK_CHROMIACAST_LIVE_EXPECTED_PRODUCT=G08 \
PRONK_CHROMIACAST_LIVE_EXPECTED_PNP_ID=TOL \
target/debug/pronk-chromiacast-p2p-test \
  "$(command -v systemd-socket-activate)" \
  target/debug/pronk-chromiacast \
  "$(command -v edid-decode)"
```

An opt-in live-Device media variant uses the same isolated PipeWire graph and exact
fd/serial checks, but selects one discovered production Device, authenticates
and cross-checks it, launches the mirroring application, and requires at least
30 encoded frames to reach the Cast transport before ordered shutdown. The VM
must share the Device's network (directly or through reflected mDNS), and its
GStreamer installation must provide `x264enc`. This replaces whatever app the
selected display is currently showing:

```sh
PRONK_CHROMIACAST_LIVE_DEVICE_ID=00112233445566778899aabbccddeeff \
PRONK_CHROMIACAST_LIVE_EXPECTED_MANUFACTURER=TCL \
PRONK_CHROMIACAST_LIVE_EXPECTED_PRODUCT=G08 \
PRONK_CHROMIACAST_LIVE_EXPECTED_PNP_ID=TOL \
tests/backend-activation/run-media-session-gate \
  "$(command -v pipewire)" "$(command -v wireplumber)" \
  "$(command -v pw-cli)" "$(command -v timeout)" \
  "$(command -v gst-launch-1.0)" "$(command -v pw-dump)" \
  target/debug/pronk-chromiacast-p2p-test \
  "$(command -v systemd-socket-activate)" \
  target/debug/pronk-chromiacast \
  data/pipewire/80-pronk-remotes.conf \
  tests/backend-activation/80-pronk-media-gate-access.conf \
  "$(command -v edid-decode)"
```

A separate isolated media-session gate starts disposable PipeWire and
WirePlumber processes, a real GStreamer `videotestsrc` producer, and the mock
backend in its normal GStreamer mode. It first passes a disconnected Unix fd
while poisoning ambient PipeWire selection, then passes a valid backend-class
connection with a nonexistent exact object serial; both must fail closed. A
fresh generation targets the real producer and requires encoded-frame counters
to advance only after H.264 Annex-B validation, remain stable while suspended,
and advance again before Resume
returns, and cleanly stop. It then removes that exact producer while an
unrelated compatible producer remains and requires the graph to fail instead
of reconnecting. Finally, it kills the exact activated backend child, proves
its PipeWire client disappears, and requires supervisor reactivation to use a
fresh process, P2P connection, session, media generation, and PipeWire remote.
Run it with:

```sh
cargo build -p pronk-backend-mock -p pronk-backend-activation-test
tests/backend-activation/run-media-session-gate \
  "$(command -v pipewire)" "$(command -v wireplumber)" \
  "$(command -v pw-cli)" "$(command -v timeout)" \
  "$(command -v gst-launch-1.0)" "$(command -v pw-dump)" \
  target/debug/pronk-backend-p2p-test \
  "$(command -v systemd-socket-activate)" \
  target/debug/pronk-backend-mock \
  data/pipewire/80-pronk-remotes.conf \
  tests/backend-activation/80-pronk-media-gate-access.conf
```

The separate `wireplumber-private-media-policy` Meson gate exercises the
packaged privacy policy rather than the permissive media fixture. It requires
WirePlumber 0.5.15 or newer, publishes supported and unknown-version private
sources, and proves that ordinary native, restricted, Flatpak, and portal
clients cannot capture supported private video. The backend may capture only
the supported version. A real Rust `VideoSource` then starts behind the
versioned policy marker; killing WirePlumber must remove the marker, deliver
`PolicyUnavailable`, destroy the source, and prevent a fresh source from
starting. On an older host WirePlumber the test exits with Meson's standard
skip status.

The same gate then gives the unsolicited-EOF mock to `BackendSupervisor`. It
requires an unavailable snapshot preserving both device identities, a
bounded retry, a strictly newer connection generation, a fresh accepted mock
process, refusal to retry that healthy process, and graceful supervised
shutdown. A no-listener unit test also proves that retry exhaustion leaves the
actor owned and manually retryable rather than detaching its task.

The packaged-unit tests verify both mode-0600 `Accept=yes` sockets, exact fd
names, nonblocking `Type=notify` services, disabled fd stores, and core
hardening directives, then ask `systemd-analyze verify` to load each socket and
template. The mock is restricted to `AF_UNIX`; Chromiacast additionally gets
`AF_INET` and `AF_INET6` for discovery and transport. The normal Meson tests do
not require installing units or modifying the running user manager.

An opt-in gate temporarily installs runtime-only copies of the mock units into
the current user manager. It runs the production `SystemdRegistrationValidator`
and `BackendSupervisor`, checking invocation ID, exact template instance,
socket trigger, main PID, readiness/stopping notification, requested shutdown,
and EOF. It first runs the same client as a different same-UID transient
service and requires the backend to reject it, then runs the successful client
as `pronk.service`. The runner refuses to replace existing runtime units or stop an
already-active mock socket, and removes only the two files it installed:

```sh
cargo build -p pronk-backend-mock -p pronk-backend-activation-test \
  --bin pronk-backend-user-manager-test
tests/backend-activation/run-user-manager-gate \
  target/debug/pronk-backend-mock \
  target/debug/pronk-backend-user-manager-test \
  data/systemd/user/pronk-backend-mock.socket \
  data/systemd/user/pronk-backend-mock@.service.in
```

The core-service opt-in gate additionally stages the root-owned mock registry
definition and D-Bus activation file at their exact installed paths, while
placing only rewritten executable paths in runtime user units. It refuses any
pre-existing target or active Pronk service, invokes `pronkctl list-devices` to
activate `pronk.service`, verifies the real manager/backend inventory, and
removes only the files and directories it created:

```sh
tests/backend-activation/run-core-service-gate \
  build/cargo-target/debug/pronkd \
  build/cargo-target/debug/pronkctl \
  build/cargo-target/debug/pronk-backend-mock \
  build/data/pronk.service \
  data/systemd/user/pronk-backend-mock.socket \
  build/data/pronk-backend-mock@.service \
  build/data/io.github.pronkproject.Pronk1.service
```

The normal public-control-plane gate uses `pronkctl list-devices`,
`list-displays`, and an idempotent `remove-display` against the isolated
service. Its separate lifecycle client exercises `AddDisplay` with a stale
exact-generation token and requires the stable `DeviceChanged` result without
creating a display or touching DRM. The installed-contract check also requires
the read-only MediaSession `GetState`/`StateChanged` interface and its fixed
state tuple. `list-displays` feature-negotiates that interface even for an
empty inventory; the daemon's zbus lifecycle test covers a real display path,
including initial state, a material Running transition, suppression of
topology-only media signals, and interface removal with the display.
