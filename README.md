# Pronk

<p align="center">
  <img src="docs/images/pronk.png" alt="A luminous pronghorn leaping from a laptop screen into a television" width="654">
</p>

Pronk is experimental Linux software that lets your desktop use a nearby
Google Cast Device, such as a Chromecast or Cast-enabled TV, as an
additional monitor. Pronk sends video to the Device and can send audio with
the video.

Pronk depends on **CastKMS**, an experimental Linux kernel driver that creates
a virtual monitor. Your desktop's compositor—the component that arranges
windows and produces the final screen image—sees this virtual monitor in the
same way that it sees a physical one. Pronk captures the completed image and
passes it to a separate program that communicates with the Cast Device.

That separate program is called a **backend** in this project. The included
`pronk-chromiacast` backend discovers Devices, encodes the captured audio and
video, and sends it over the network. The main service runs as the `pronkd`
daemon and never connects to the network itself. You control it with the
`pronkctl` command-line tool.

> [!IMPORTANT]
> Pronk is not yet a ready-to-install desktop application. It currently
> requires a separately built kernel driver and a source build. Session mode
> additionally requires a compositor with the experimental CastKMS grant
> broker; optional system mode can operate without compositor integration.

## Contents

- [Requirements](#requirements)
- [Build and install](#build-and-install)
- [Optional system service](#optional-system-service)
- [Silverblue development extension](#silverblue-development-extension)
- [Add and remove a display](#add-and-remove-a-display)
- [How Pronk works](#how-pronk-works)
- [Technical FAQ](#technical-faq)
- [Display control with HDMI-CEC](#display-control-with-hdmi-cec)
- [Capture authorization](#capture-authorization)
- [Backends](#backends)
- [Private audio and video](#private-audio-and-video)
- [D-Bus API](#d-bus-api)
- [Tests](#tests)
- [License](#license)

## Requirements

To run Pronk in its default session mode, you need:

- a Linux desktop with an active graphical login session;
- the CastKMS 0.12 kernel driver installed and loaded, with at least one
  available virtual monitor slot;
- GNOME Mutter with the `org.gnome.Mutter.CastKms` session-bus grant broker;
- a Google Cast Device on the same network as the computer;
- systemd user services, PipeWire, and WirePlumber 0.5.15 or newer; and
- these GStreamer components:
  - for video: `x264enc`, `h264parse`, and `pipewiresrc`;
  - for optional audio: `opusenc`, `audioconvert`, and `audioresample`.

Pronk uses systemd to start its processes, PipeWire to move captured media
between them, and WirePlumber to restrict what each process can access.

System mode replaces the active graphical-session and Mutter requirements
with polkit, a dedicated `pronk` system account, and system services for
PipeWire and WirePlumber. The compositor still needs ordinary DRM/KMS support
for the CastKMS connector, but it does not need a Pronk-specific D-Bus API.
When the system service is running, it is authoritative: a session daemon
declines startup or shuts down when the system service appears.

On Fedora, `x264enc` is provided by RPM Fusion's
`gstreamer1-plugins-ugly` package. Package names differ on other Linux
distributions.

Building Pronk also requires:

- Rust 1.83 or newer;
- Meson 1.4 or newer;
- development files for `libdrm`, `libsystemd`, and `libpipewire-0.3`; and
- Cargo access to crates.io for the published
  [`chromiacast`](https://crates.io/crates/chromiacast) dependency.

[CastKMS](https://github.com/pronkproject/castkms) is also a separate project; it
is not included in this repository. Install and load that driver before trying
to add a display.

## Build and install

Configure Meson for the standard `/usr` installation layout:

```sh
meson setup build -Dprefix=/usr -Dlibexecdir=libexec
meson compile -C build
meson test -C build --print-errorlogs
```

Meson currently builds the Rust workspace in debug mode and copies the debug
binaries into the installation layout.

Install the build and update the affected user services:

```sh
sudo meson install -C build
systemctl --user daemon-reload
systemctl --user enable --now pronk-chromiacast.socket
systemctl --user restart wireplumber.service
```

The Chromiacast socket waits for Pronk to request a backend connection. When
that request arrives, systemd starts the backend. Restarting the desktop
WirePlumber loads CastKMS output routing; Pronk starts its own private
PipeWire/WirePlumber pair automatically when `pronkctl` activates the main
service.

To run only the Rust tests, without using Meson, use:

```sh
cargo test --workspace --locked
```

## Optional system service

The system service is useful when the compositor does not implement the
CastKMS grant broker. After installing, create the packaged service account
and private directories, then start the service:

```sh
sudo systemd-sysusers /usr/lib/sysusers.d/pronk.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/pronk.conf
sudo systemctl daemon-reload
sudo systemctl enable --now pronk.service
```

Use `--system` for every control operation:

```sh
pronkctl --system list-devices
pronkctl --system add-display --device chromiacast:<device-id> --no-audio
pronkctl --system list-displays
```

An unprivileged invocation asks polkit to authenticate an administrator and
then runs only the installed `pronkctl` as the `pronk` account. The public
system-bus API also exposes read-only inventory and state to desktop clients.
Its control methods only accept a caller that already holds the polkit
authorization; the daemon never asks polkit to display an authentication
dialog. Graphical clients acquire that authorization through an explicit
Unlock action. The daemon, backends, PipeWire, and WirePlumber remain under
`pronk`; root exists only in a short-lived grant helper described under
[Capture authorization](#capture-authorization).

System mode has its own PipeWire graph below `/run/pronk`. This prevents a
root process from exchanging pixels with a user-controlled media server and
keeps backend media private. Desktop audio still follows the normal output
path into the CastKMS ALSA playback card; a grant-scoped kernel tap carries the
consumed samples into Pronk's private graph, so no cross-graph audio route is
needed.

## Silverblue development extension

The whole-stack Fedora Silverblue development extension is maintained by the
[Pronk packaging repository](https://github.com/pronkproject/packaging#development-system-extension).
That workflow builds the packaging checkout's `sources/*` submodules so every
component comes from one explicit source bundle.

## Add and remove a display

Keep your graphical session active throughout these steps.

First, list the Cast Devices that Pronk can find:

```sh
pronkctl list-devices
```

Each result contains an `ID` such as `chromiacast:<device-id>`. Copy the full
value after `ID` and use it to add the Device as a display. Starting without
audio makes the first test simpler:

```sh
pronkctl add-display --device chromiacast:<device-id> --no-audio
```

Remove `--no-audio` to send audio as well as video. Pronk prints each setup
stage and waits for the operation to finish. You can press Ctrl-C while setup
is in progress to request cancellation.

After setup succeeds, the new monitor appears in your desktop's normal display
settings. You can arrange it and choose a supported resolution and refresh
rate there just as you would for a physical monitor.

To inspect configured displays, run:

```sh
pronkctl list-displays
```

This command shows the Device, the virtual monitor in use, its resolution and
refresh rate, whether audio is enabled, and the current media state. The
possible media states are `Inactive`, `Starting`, `Running`, `Suspended`,
`Recovering`, `Stopping`, and `Failed`.

To remove a display, use its display `ID` from `list-displays`—not the Device
ID from `list-devices`:

```sh
pronkctl remove-display <display-id>
```

If no Devices appear, confirm that the Device is on the same network and
that `pronk-chromiacast.socket` is active. If media does not reach `Running`,
check that WirePlumber is new enough, that the required GStreamer elements are
installed, and that PipeWire and WirePlumber were restarted after
installation. Use `pronkctl list-displays` to check the media state.
If session setup fails while authorizing the display, verify that Mutter owns
`org.gnome.Mutter.CastKms` on the session bus and that the loaded CastKMS
driver exposes capture UAPI 0.12 with the grant-control-fd capability.

Pronk can tell when a Device accepts a stream, but the Device does not
confirm that the television decoded and displayed it. A successful send means
"delivered to the Device," not "confirmed visible on screen."

## How Pronk works

The media paths are:

```text
desktop compositor
  → CastKMS virtual monitor
  → grant-scoped video capture
  → Pronk private PipeWire
  → Chromiacast backend
  → Cast Device

desktop PipeWire
  → CastKMS playback-only ALSA card
  → grant-scoped kernel audio tap
  → private PipeWire connection
  → Chromiacast backend
  → Cast Device
```

A display is set up as follows:

1. CastKMS creates empty virtual monitor slots. At this point, the desktop does
   not see a monitor attached to them.
2. The Chromiacast backend discovers Devices on the local network.
3. When you select a Device, session mode confirms that the request came from
   the active local graphical session and asks Mutter for permission to use
   one CastKMS monitor slot. System mode accepts only the `pronk` account and
   obtains the same connector-scoped rights through its one-shot helper.
4. The backend authenticates the Device. It then finds video and audio
   formats supported by both the Device and Pronk. The backend gives Pronk
   the information needed to describe the virtual monitor. Device
   credentials and network details remain inside the backend.
5. Pronk attaches the virtual monitor and supplies its **EDID**, the standard
   data a monitor uses to report its name, supported resolutions, and audio
   capabilities.
6. The compositor notices the new monitor, configures it, and begins drawing
   frames for it.
7. Pronk captures each completed frame. It stores the frame as an `XRGB8888`
   image in a **DMA-BUF**, a graphics buffer that local processes can share
   using an operating-system handle called a file descriptor. Pronk publishes
   the buffer through a private PipeWire connection. When audio is enabled,
   Pronk also captures audio sent to that virtual monitor.
8. The backend encodes the captured media and sends it to the Device. If the
   backend or PipeWire restarts, the virtual monitor remains attached and
   streaming starts again with a new media session.

Pronk deliberately separates responsibilities:

- The main service can capture only the CastKMS monitor for which it received
  permission. A client cannot point it at another graphics device or virtual
  monitor slot.
- The main service does not implement Google Cast and does not open network
  connections.
- A backend never receives a CastKMS or other graphics-device file descriptor.

## Technical FAQ

The [technical FAQ](docs/faq.md) explains the rationale behind the major design
choices, including DRM writeback and leases, the CastKMS grant lifetime,
retained framebuffer safety, PipeWire policy, process separation, DMA-BUF
copies, latency, audio, and modes.

## Display control with HDMI-CEC

When a Device and its backend support display control, Pronk exposes the
CastKMS connector as a normal Linux HDMI-CEC adapter. Existing CEC clients can
then use `/dev/cecX`; they do not need to know that the display is reached over
a network protocol.

The control path is:

```text
Linux CEC client
  → CastKMS /dev/cecX adapter
  → Pronk CEC translator
  → normalized Device control
  → selected backend
  → Device protocol
```

Pronk translates activation, deactivation, power, standby, key, volume, and
mute operations. The backend reports completion through the same chain, so a
CEC transmit is not reported as successful merely because Pronk accepted it.

CEC belongs to the attached cast display rather than to one video or audio
session. It remains available while the monitor route is disabled, across
ordinary modesets, and while media is replaced. Removing the display, losing
the grant, or stopping Pronk invalidates the CEC physical address and ends the
transport. A temporary grant-authority suspension aborts in-flight work; once
authority returns, a fresh state generation admits new or kernel-retried
transmits.

The CastKMS actor is the sole owner of the grant and CEC file descriptor. The
translator contains no Cast, D-Bus, or network code, and a backend receives
only a bounded normalized control operation—never the DRM or CEC descriptor.

## Capture authorization

Being able to open a graphics device such as `/dev/dri/cardN` does not give a
process permission to capture CastKMS pixels. CastKMS requires a **grant**. The
grant is a file descriptor—an operating-system handle—that gives its holder
specific rights for one virtual monitor slot.

### Session-mode authorization

Pronk asks Mutter, GNOME's compositor and DRM master, to create a normal grant
with exactly the rights required by the selected display profile. Mutter
authorizes the unique session-bus owner of `io.github.pronkproject.Pronk1` and
returns only the restricted **holder** descriptor. Pronk validates the
returned metadata against the requested device, connector, stable output
identity, rights, grant mode, and CastKMS UAPI before using it.

CastKMS also gives Mutter a private **control** descriptor. Closing it revokes
the grant. The descriptor becomes permanently hung up when the final holder is
closed, so Mutter can release the disconnected CastKMS card without a second
userspace lifetime pipe. If Pronk leaves the bus or exits unexpectedly, Mutter
closes the control descriptor; if Pronk simply finishes with a display, it
drops the holder and Mutter observes the hangup. Pronk never receives the
control descriptor or the authority to revoke grants independently of Mutter.

Session-mode grant acquisition has no privileged fallback. A missing broker,
rejected request, invalid response, or cancelled operation fails display setup
and drops any received descriptor. The user service therefore runs with
`NoNewPrivileges=yes` and cannot start setuid programs.

### System-mode authorization

System mode runs `pronkd` permanently as the non-root `pronk` account. For one
grant request it launches `/usr/libexec/pronk-grant-helper` through
`pkexec --keep-cwd` from the unit's fixed `/` working directory. This avoids
exposing `/root` through the daemon's `ProtectHome` sandbox merely to satisfy
`pkexec`'s default target-home-directory change.
Polkit permits that helper only when the requesting process already runs as
`pronk`; the helper then independently requires its Unix seqpacket peer and
parent to be a live process in `pronk.service`, executing the installed
root-owned `pronkd` inode. Those checks are repeated immediately before and
after grant creation.

The helper accepts only a device major/minor, virtual connector, and one fixed
rights profile. It creates an explicit administrative CastKMS grant, drops any
accidental DRM-master state, and transfers exactly two descriptors: the
restricted holder and the close-to-revoke control endpoint. Administrative
grants are creator-independent, so the helper closes its privileged DRM file
before exiting; only the anonymous control descriptor can revoke the grant.

The fixed audio-enabled profile also includes the right to open the
attachment's anonymous kernel audio tap. The long-lived daemon has no
effective, permitted, or inheritable
capabilities and verifies that fact at startup. Its systemd capability
bounding set exists only so `pkexec` can enter the helper with
`CAP_SYS_ADMIN` for grant creation and `CAP_SYS_PTRACE` for repeated
`/proc/PID/exe` identity checks. The helper clears supplementary groups,
drops every other capability, and sets `NoNewPrivileges`. No root daemon
connects to the `pronk` PipeWire server.

## Backends

A backend discovers compatible devices and communicates with the selected
device. Each installed backend has a configuration file in TOML format under
`/usr/lib/pronk/backends.d`. The installed files are owned by the root user.

For safety, a configuration file may specify only a local socket path and the
name of a preinstalled systemd service template. The template tells systemd
how to start one backend process. A configuration file cannot specify an
executable, command-line arguments, or environment variables. Installing a
backend configuration therefore cannot make Pronk run an arbitrary command.

This repository includes two backends:

- **`pronk-chromiacast`** is the backend for normal use. It starts on demand
  through a private systemd user or system socket. It discovers and
  authenticates Cast Devices, encodes H.264 video and optional Opus audio, and
  handles the Cast network protocol.
- **`pronk-backend-mock`** behaves predictably for automated tests. It lets
  developers test Pronk without a physical Device, but it does not replace
  testing with real Cast hardware.

The main service and backend verify each other's identities through the
selected systemd manager. The backend must have been started from its installed
socket, and the main process must be `pronk.service`. On the accepted backend
socket, Pronk also requires kernel-supplied process credentials with every D-Bus read.
It rejects a writer change and verifies that the authenticated writer is the
backend unit's current main process before admitting the connection.

## Private audio and video

Most desktop applications connect to PipeWire through one shared server.
Pronk does not publish captured media there. Both service modes start a second,
hardware-free PipeWire/WirePlumber pair: session mode below `%t/pronk/media`
and system mode below `/run/pronk`. That private server creates two classified
connections:

- `pipewire-0-pronk-core` for the main service; and
- `pipewire-0-pronk-backend` for backends.

For each media generation, Pronk opens fresh PipeWire connections and passes
the backend connections as file descriptors; the backend does not need to open
a PipeWire socket by path. A Unix socket is an endpoint for communication
between processes on the same computer. WirePlumber 0.5.15 or newer applies the
following access rules before either process can use the private graph:

- The main service may publish versioned Pronk video and one audio source made
  directly from its grant-scoped CastKMS tap. The private media services do not
  enumerate ALSA hardware and cannot open `/dev/snd`.
- The backend may see only compatible Pronk-private video and kernel-tap audio
  sources. Each source carries the exact connector, grant, session, and media
  generation identity supplied in the backend protocol.
- Cameras, microphones, unrelated monitors, and video carrying an unrecognized
  access-rule version remain hidden. Other desktop applications retain their
  usual playback access on the ordinary desktop graph.

Pronk refuses to start a real media stream if these access rules are not
loaded. It does not silently use the less restricted default PipeWire
connection. In system mode all four PipeWire sockets and WirePlumber are owned
by the non-root `pronk` account. On WirePlumber versions older than 0.5.15, the
corresponding test is skipped because those versions cannot provide the same
protection.

The current named sockets are mode `0600` and classify a connection by the
socket endpoint. Pronk treats one Unix user ID as one trust principal and does
not claim confidentiality from another unsandboxed process owned by that user.
Claiming a per-process boundary without requiring a separately enforced process
identity would promise more than the standard Unix credential model provides.
The packaged backend sandbox still hides the default and Pronk-specific
PipeWire socket paths, so the backend can use only the connected descriptors
Pronk transfers. WirePlumber then restricts what that admitted backend role can
see without pretending to isolate arbitrary same-user processes.

The first supported video encoder is GStreamer's `x264enc`. Audio uses
`opusenc`.

## D-Bus API

Everything available through `pronkctl` is also available through D-Bus, the
desktop's standard system for communication between applications and
services. Pronk uses `io.github.pronkproject.Pronk1` on the session bus or the
system bus. A graphical client should prefer an already-running system-bus
service and otherwise use the session bus. System-bus inventory and state are
public; control methods require a previously acquired polkit authorization.
Graphical clients should expose that as a deliberate Unlock action and remain
read-only until it succeeds. Command-line callers can use `pronkctl --system`,
whose equivalent polkit workflow runs the installed client as the `pronk`
service account.

An `AddDisplay` call returns immediately with an object that reports the
operation's progress. If setup fails or is cancelled, Pronk gives up the
capture permission, detaches the virtual monitor, and closes the backend
connection. After setup succeeds, the display remains under Pronk's control
until `RemoveDisplay` is called or the service stops.

See the
[`D-Bus interface definition`](data/dbus-1/interfaces/io.github.pronkproject.Pronk1.xml)
for the methods, properties, signals, and data types.

Developers changing grant handling or backend startup should also read:

- [`tests/castkms-live/README.md`](tests/castkms-live/README.md)
- [`tests/backend-activation/README.md`](tests/backend-activation/README.md)

## Tests

The normal test suite runs without a loaded CastKMS device:

```sh
meson test -C build --print-errorlogs
```

It covers:

- Mutter and one-shot helper grant requests with strict metadata validation;
- systemd service and socket definitions;
- PipeWire connection access and WirePlumber privacy rules;
- mock and Chromiacast streaming behavior;
- HDMI-CEC event translation, generation-safe transport state, and normalized
  backend control completion; and
- the public command interface, tested against an isolated copy of the main
  service.

Tests that exercise real grants, Mutter display changes, PipeWire capture, and
a complete connection to a Device require a graphical virtual machine. See
[`tests/vm/README.md`](tests/vm/README.md) for setup and usage.

## License

Pronk is available under the [MIT License](LICENSE). Files that carry a
different SPDX license identifier remain available under the license named in
that file.
