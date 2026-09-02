# Pronk technical FAQ

This document answers design questions that commonly arise when discussing
Pronk, CastKMS, and their PipeWire and compositor integration. The interfaces
are experimental. The answers describe the current prototype, not a claim that
every boundary is already in its final form.

## Product model

### What is Pronk trying to provide?

Pronk makes a network display behave like an additional desktop monitor. The
compositor can extend the desktop onto it, arrange it beside physical outputs,
select a mode, and route applications and audio to it through existing desktop
interfaces. It is not limited to mirroring an existing monitor.

The first backend targets Google Cast Devices. The architecture keeps the
network protocol behind a backend boundary so Miracast, AirPlay, or another
transport could be added without putting those protocols into the kernel or
the session coordinator.

### Why make the Device a monitor instead of using screen casting?

An ordinary screen-casting session captures an existing monitor or window.
Upstream Mutter also has a virtual-monitor API, exposed through its screen-cast
stack, that creates an output on which GNOME can independently place windows
and choose a mode. GNOME Network Displays can use that virtual source, so
creating additional desktop space is not unique to a DRM connector.

Pronk chooses the DRM/KMS abstraction so the virtual monitor participates in
the standard kernel display topology. A compositor can drive it through
ordinary KMS support, and system-mode Pronk does not require the compositor to
implement a native virtual-monitor or remote-display API. The tradeoff is the
host-memory software composition described below. On GNOME, Mutter's native
virtual monitor is often the more efficient source for screen casting.

### Why does selecting a Device require an explicit setup action?

Launching a Cast mirroring application can interrupt content already playing
on a television. Network discovery therefore updates the Device inventory but
does not hotplug every discovered television into the desktop. The user must
select **Set Up** before Pronk attaches a monitor.

Removing a configured display detaches the monitor and releases its capture
authority. Temporary Device unavailability and recoverable media failures do
not silently forget the user's configured display; Pronk retains the monitor
attachment while it waits for the Device or creates a fresh media generation.
If recovery itself fails or a terminal kernel or grant event occurs, the
orderly failure path detaches the monitor instead of leaving a phantom output.

### Does the core design require GNOME?

No. CastKMS is a DRM driver, `pronkd` can be a systemd user or system service,
and backends use a private peer-to-peer D-Bus protocol. Session mode currently
uses the experimental capture-grant integration in the
[PronkProject Mutter fork](https://github.com/pronkproject/mutter) and a GNOME
Settings panel. Upstream Mutter does not provide that grant API. Another
compositor or display server can implement the same normal-grant interface, or
Pronk can use optional system mode and its tightly scoped administrative-grant
helper without display-server integration.

## DRM and kernel design

### Why is a kernel driver involved at all?

The kernel driver gives an unmodified display stack an ordinary DRM connector,
CRTC, planes, modes, vblank cadence, EDID, HDMI audio presentation, and CEC
adapter. The compositor uses its normal KMS path instead of learning a second
kind of userspace-only monitor.

The driver also enforces the pixel capability below the session processes.
Opening the DRM primary node—even from a sandbox with graphics-device
access—does not authorize capture. Only a connector-scoped grant can attach
the monitor or read its completed composition.

A compositor-native virtual-output API is another valid design. It would move
more display, lifetime, and capture behavior into each compositor and would not
by itself provide a common kernel-enforced capability boundary. Pronk is an
experiment in the DRM-based design point, not a claim that every remote-display
product must use it.

### Is CastKMS just VKMS with a network protocol in the kernel?

CastKMS is derived from VKMS, but no network protocol is in the kernel. It adds
durable disconnected output slots, controlled monitor attachment and EDID,
connector-scoped capture grants, cursor capture, per-attachment HDMI audio,
and a CEC transport. Pronk and its backend implement PipeWire, encoding, Cast,
discovery, and network recovery in userspace. The Chromiacast library also
provides Android TV Remote APIs, but the current Pronk backend does not yet
configure or use them.

The VKMS checksum, configfs, writeback, and fbdev facilities remain optional
test features and are disabled on the default product device.

### Why not use a DRM writeback connector?

Writeback is an operation performed by DRM master as part of an atomic commit.
It is useful for tests and master-owned capture, but it does not provide a
restricted non-master process with a durable stream, buffer queue, cursor
policy, monitor attachment, or revocable connector-scoped authority.

Pronk intentionally keeps the capture agent out of the modesetting authority.
CastKMS capture lets that agent queue buffers after the compositor has produced
an ordinary completed frame. The optional VKMS-derived writeback connector is
still present for tests, but it is not the product capture path.

### Why not use a DRM lease?

A lease delegates modesetting resources. The Pronk service should not become
the process that configures the output; the display server should continue
treating it as part of the desktop topology and rendering it with the other
monitors.

A lease also does not grant a capture stream over the compositor's completed
composition. CastKMS capture safety is device-global because every visible
plane must belong to the current content owner. A lease master cannot create a
grant whose safety the driver cannot represent.

### Why introduce a driver-private capture UAPI?

No existing DRM interface combines these requirements:

- a never-master capability for one connector;
- independently selectable attachment, EDID, pixels, cursor, and CEC rights;
- durable authorization across ordinary modesets;
- mode-generation-scoped streams and registered buffers;
- synchronous revocation in both lifetime directions; and
- protection against pixels retained from a previous DRM master.

The current UAPI is explicitly versioned `0.x` so it can change after review.
The kernel-native capture and authority cores do not depend on DRM file
descriptors or ioctl IDs; the UAPI is an adapter over those cores. Trusted code
linked into the driver can use the same rules directly without fabricating a
UAPI client.

### Why are there holder and grantor file descriptors?

The holder descriptor is the capability used by Pronk. The compositor retains
the grantor descriptor. Closing the final grantor reference synchronously
revokes the holder; polling the grantor reports `POLLHUP` when the final holder
closes or another terminal event revokes the grant.

That gives both sides kernel-backed lifetime observation. It avoids a separate
userspace pipe whose only purpose is to babysit another descriptor, and it
continues to work through device teardown. The relationship is similar to two
ends of a capability, not two copies of the same authority: the grantor cannot
capture, and the holder cannot independently preserve authority after the
grantor revokes it.

### Why diverge from the single-descriptor lifetime used by DRM leases?

A lease primarily delegates resource ownership in one direction. A CastKMS
grant has two independent owners with useful terminal information: the
compositor must revoke capture, and it must also learn when the capture agent
has gone away so it can release its held KMS device fd and grant bookkeeping.

Returning the grantor descriptor from the creation ioctl makes that
relationship atomic and explicit. The design does not require DRM leases to
change.

### Does capture protection break seamless framebuffer handoff?

No. Generic DRM behavior remains intact: the current DRM master can use the
normal `GETFB2` and `CLOSEFB` framebuffer-handoff mechanisms for retained
scanout, which supports flicker-free compositor handoff.

The additional ownership rule applies only to CastKMS pixel-export paths:
capture, writeback, and checksums. A newly current master cannot export the
previous master's residual image until an atomic commit establishes a
composition whose visible framebuffers all belong to the new master. A grant
file never becomes master and cannot use `GETFB2`.

### Why is being current DRM master not sufficient for safe capture?

A newly current master may inherit pixels produced by the previous login
session. Treating current-master identity as proof of ownership would let a
new session capture those residual pixels before drawing anything itself.

CastKMS stamps framebuffer and complete-composition ownership. Mixed,
ownerless, residual, and in-flight compositions are withheld rather than
guessed safe. A no-op commit cannot claim old content.

### What happens across a modeset?

The grant and monitor attachment survive. A capture stream does not, because
its CRTC, dimensions, timing, and buffers describe one mode generation.
CastKMS completes queued work with a stale-generation result, and Pronk creates
a replacement stream and buffer set on the same holder descriptor.

That separation is intentional:

```text
grant       durable authorization
attachment  durable connector state
stream      one CRTC and mode generation
buffers     one stream generation and dimensions
```

### Does CastKMS remove connectors when a Device disconnects?

No. The module creates a bounded number of persistent connector objects. An
unused slot is disconnected; attachment changes its connection state and
emits ordinary hotplug notification. Revoking the attachment owner disconnects
the monitor and clears its EDID, but does not remove the DRM connector object.

That matches physical hotplug more closely than dynamically destroying KMS
objects and avoids requiring the compositor to tolerate connector-object
disappearance.

### What are normal, delegated, and administrative grants?

- A **normal** grant is created by the current top-level DRM owner master and
  is bound to that exact master. Pronk's experimental Mutter fork uses this
  form for session mode.
- A **delegated** grant is created by host root while root is not the current
  master, is bound to the current top-level owner master, and can outlive the
  short-lived helper that created it. It exists for integrations that need
  such a helper; Pronk does not currently need one.
- An **administrative** grant is created by host root and follows whichever
  master owns safe content. VM tests use it directly, and Pronk's optional
  system mode uses it through a single-purpose one-shot helper. Session mode
  never silently falls back to it.

The forms are explicit. A privileged non-master caller does not silently
receive administrative behavior when it requested an ordinary grant.

### Does Pronk require a privileged helper?

Session mode does not. The display server already owns DRM master, and the
experimental PronkProject Mutter fork can create a normal grant on behalf of
Pronk. Pronk receives only the holder descriptor and runs as a sandboxed user
service. Failure to reach or pass the compositor's authorization fails setup;
there is no administrative fallback.

System mode deliberately uses a privileged helper because it cannot assume the
active display server implements Pronk's grant interface. The long-running
daemon, backends, PipeWire, and WirePlumber all run as the dedicated non-root
`pronk` user. `pronkd` invokes the installed helper through
`pkexec --disable-internal-agent --keep-cwd` from the unit's fixed `/` working
directory; a polkit rule admits only that service user, and the helper
independently verifies the live parent, Unix peer credentials, installed
`pronkd` inode, and
`pronk.service` membership before and after creating one fixed-profile grant.
It drops supplementary groups and every capability except `CAP_SYS_ADMIN`
for grant creation and `CAP_SYS_PTRACE` for its repeated parent-executable
checks before opening CastKMS, passes the restricted holder and anonymous
close-to-revoke control descriptor back, closes its privileged DRM file, and
exits. Administrative grants are deliberately independent of that creator
file, so the long-running daemon never owns a root-opened DRM descriptor.

### Why does the helper not exchange the parent's process start time?

`/proc/PID/stat` start time does not change across `exec`; it only helps
distinguish a reused numeric PID. The helper already opens a pidfd for the
exact parent and repeatedly checks that it is live, still its parent, still
the seqpacket peer, still running the installed `pronkd` inode, and still the
member of `pronk.service`. Adding a caller-supplied start time would not
prove anything about the `exec` transition and would duplicate the pidfd's PID
reuse protection.

### How can an ordinary user control system mode?

`pronkctl --system` follows the `grdctl` model: if it is not already running
as `pronk`, it asks polkit to run the installed `pronkctl` as that account.
The action uses `auth_admin`, so the active authentication agent can request
an administrator password. Only the `pronk` account may call the public
system-bus service; `pronkd` also checks the bus broker's PID and UID and
pidfd-pins every caller that starts a setup operation.

### Why does each mode run a separate PipeWire server?

A root daemon sending captured pixels over a socket controlled by a desktop
user would invert the intended trust boundary. System mode therefore runs
PipeWire and WirePlumber as `pronk` in `/run/pronk`, alongside the non-root
daemon and backends.

Session mode uses the same media architecture below `%t/pronk/media`. One path
is easier to secure and test, and there is no need for the old special case
that found and captured a sink monitor in the desktop graph. The desktop graph
still routes applications into the ordinary CastKMS playback sink. The kernel
copies the consumed samples into the grant-scoped audio tap, and Pronk
publishes that fd as a source in its private graph. Neither mode needs a
cross-graph PipeWire link.

## Process and PipeWire architecture

### Why are Pronk and the Device backend separate processes?

They have different authority and failure domains:

- Pronk owns the CastKMS holder, attachment, capture, and private PipeWire
  publication. It has no Device protocol or network implementation.
- The backend discovers and authenticates Devices, encodes media, and talks to
  the network. It never receives a DRM or CastKMS descriptor.

That boundary contains protocol parsers and network-facing code, permits
future backends, and lets recoverable backend or media failures restart without
discarding the user's monitor attachment. Systemd activates installed backends
through fixed local sockets rather than allowing configuration to name
arbitrary executables.

### Why use PipeWire between two processes that could share buffers directly?

PipeWire supplies a standard negotiated buffer transport, scheduling model,
and audio graph. GStreamer can consume the same nodes without a bespoke
cross-process media protocol, and future backends are not forced to link the
CastKMS capture implementation.

It is also a security boundary between service roles and ordinary session
clients. The kernel grant controls who can obtain the original pixels and
audio-tap fd. The private WirePlumber controls which admitted role may consume
the video and audio sources published from those capabilities. Those are
different questions, so PipeWire is more than a compatibility shim. This
boundary does not claim to separate mutually hostile, unsandboxed processes
owned by one Unix user.

Putting both halves in one process remains technically possible, but it would
combine kernel capture authority with the network attack surface and make
backend replacement less useful.

### Is the captured desktop visible on the ordinary PipeWire socket?

No. Pronk uses separately named core and backend connections. For every media
generation it creates fresh connected descriptors and passes only the backend
side to the selected backend. WirePlumber 0.5.15 or newer restricts the visible
objects before either side can use PipeWire.

The core role can publish versioned Pronk video and the exact audio source made
from its kernel tap. The backend role is denied ordinary video sources, audio
sources, and audio sinks, then allowed compatible Pronk-private video and
kernel-tap audio sources. The private protocol supplies the exact node identity
for the current media generation, and the normal backend uses only that target.

The current PipeWire permission is role-wide, not a per-session object
capability: a client admitted to the backend role can see every object carrying
the supported Pronk marker. Ordinary clients cannot see the private video or
audio, and the backend cannot see cameras, microphones, or ordinary desktop
audio sink/source nodes.

### Does Pronk hide captured media from other unsandboxed same-user processes?

No. Pronk treats a Unix user ID as one trust principal. A deliberately hostile,
unsandboxed process running as that user can open a Pronk PipeWire socket with
mode `0600`, just as it can access other resources owned by that user. Pronk
does not claim confidentiality between those processes.

Claiming otherwise without requiring an operating-system sandbox or another
independently enforced identity would create a security contract that the
standard Unix credential model cannot uphold. Unix filesystem and socket
permissions assign authority to users and groups; they do not assign different
rights to arbitrary programs sharing one user ID. Process metadata can help
supervise a known child, but it does not turn one account into independently
protected principals. A PipeWire security context can narrow object
permissions after a client has been admitted, but it cannot by itself
establish that missing authority boundary.

Pronk instead states and enforces the narrower boundaries available in the
packaged system: other Unix users cannot open the sockets, ordinary PipeWire
clients cannot see Pronk-private video, and the sandboxed network backend
cannot open arbitrary PipeWire endpoints. Pronk passes that backend only
preconnected descriptors, and WirePlumber restricts what its admitted role can
see. Those properties limit accidental exposure and confine the backend
without pretending to divide one unsandboxed Unix user into separate security
principals.

### Why require a relatively new WirePlumber?

WirePlumber 0.5.15 introduced the permission-manager API that the packaged
policy uses to attach and maintain per-object permissions as a client is
admitted. Pronk's current policy has no equivalent older-version path, so it
treats the version as a security requirement rather than silently falling back
to the ordinary session connection.

GStreamer encoding and Cast transport do not intrinsically depend on that API,
but the production video source checks a WirePlumber-owned policy marker and
fails closed when the policy is absent. A future implementation of equivalent
restrictions through older APIs could lower the version requirement without
changing the architecture.

### Is the video path zero-copy?

Not end to end. CastKMS is a software compositor and must produce a completed
frame in a capture buffer. That is the unavoidable composition/copy point in
the current driver.

Afterward, the capture destination is a DMA-BUF shared through PipeWire. The
backend's GStreamer source uses the PipeWire buffer pool, avoiding an extra
full-size BGRx staging copy before conversion. Pronk marks that PipeWire stream
non-live because the pipeline already forces `GstSystemClock`; this prevents
GstBaseSrc's live presentation-timestamp wait from pinning a dequeued CastKMS
destination. The
leaky queue follows `videoconvert`, so any queued frame is the copied I420
allocation rather than an imported BGRx DMA-BUF. The current `videoconvert` and
software `x264enc` path still maps the linear DMA-BUF and allocates converted
I420 output; this is not an end-to-end zero-copy encoder path.

Explicit synchronization does not replace pool depth or timely buffer return.
The software encoder still CPU-maps its input, and CastKMS cannot compose into
a destination while a consumer is reading it; synchronization timelines order
those accesses but do not create additional destinations.

Vulkan could become useful for GPU composition, conversion, or encoding, but
adding it now would not remove the current CPU composition requirement and
would substantially enlarge synchronization and device-compatibility scope.

### How does Pronk compare with GNOME Network Displays?

[GNOME Network Displays](https://gitlab.gnome.org/GNOME/gnome-network-displays)
and Pronk can expose the same desktop concept: an additional monitor that the
user can arrange independently from physical displays. GNOME Network Displays
asks Mutter to create that monitor through its native virtual-monitor and
screen-cast APIs. Pronk instead exposes a DRM connector that the compositor
drives through KMS. The distinction is therefore below the desktop model:
compositor-native rendering and capture on GNOME versus a kernel display
device that works with ordinary KMS support. The latter also imposes a
different memory path.

CastKMS is a virtual, kernel-side KMS device rather than a physical GPU.
Its software composition must produce a completed frame in host memory; the
frame cannot remain solely inside a physical GPU's private render pipeline.
Sharing the resulting DMA-BUF through PipeWire avoids another BGRx staging
copy, but does not remove the software composition or the current CPU color
conversion and encoding described above.

GNOME Network Displays can instead receive GPU-backed DMA-BUFs directly
from Mutter. With compatible formats, modifiers, GStreamer elements, and
drivers, a hardware encoder can import those buffers and keep conversion
and encoding on the GPU without a round trip through CPU buffers. This uses
hardware *encoding* on the sender; hardware decoding is performed by the
receiver.
That path is not automatic: a generic scaling or color-conversion element
may still map or copy a frame when the negotiated memory formats do not line
up.

[GNOME Network Displays merge request 238](https://gitlab.gnome.org/GNOME/gnome-network-displays/-/merge_requests/238)
adds a daemon-oriented architecture and a hardware-first H.264 Cast
mirroring path. If it lands, it will give GNOME users a more direct and
potentially more efficient story for casting an existing or virtual GNOME
display. Pronk will retain a different role: providing a KMS-visible virtual
monitor, including on desktops that have no GNOME screen-cast integration.

### Why is there polling as well as PipeWire process notification?

The normal path uses PipeWire's `RequestProcess` command and stream process
callback. A consumer that sends `RequestProcess` is handled immediately.
Stock `pipewiresrc` with the shared buffer pool, however, queues a returned
buffer when its final GStreamer reference is released without sending that
command. The producer then needs another normal graph cycle before its process
callback can observe the return.

While any buffer remains submitted, Pronk arms a quarter-frame deadline,
bounded to 2–10 ms, and calls PipeWire's own `trigger_process` when it expires.
The deadline is disarmed when no submitted buffer remains; it is neither a
second buffer-delivery path nor an always-running poll.

That preserves the process-callback ownership rules while avoiding both a busy
loop and an indefinitely stranded reusable buffer. Tests exercise explicit
process requests, returned-buffer triggering, deadline disarming, and media
generation changes.

### How is latency controlled?

The Cast offer starts with a target playout delay of roughly 33 ms, raised when
necessary to hold at least one video frame or audio packet. When every accepted
stream negotiates target-delay updates, Chromiacast observes RTCP NACKs, loss,
round-trip time, Device-confirmed delay, sender drops, and queue pressure.
Stable delivery can reduce the delay toward the packet-duration floor; loss or
rising pressure increases it in bounded steps up to both the Device's reported
limit and Pronk's 250 ms cap. Audio and video must confirm the same delay.

If the Device does not negotiate the update extension for every stream, the
session keeps its negotiated fixed target instead of pretending adaptation is
active.

The controller also reports buffer pressure to the encoder path, requests key
frames after loss, and reduces bitrate under sustained pressure. It does not
claim that a television and Wi-Fi link will have local-monitor latency.

### How does audio work?

Each attached audio-capable CastKMS display owns one ALSA HDMI presentation
card. Its user-visible name follows the Device name assigned by the user, so
two televisions of the same model remain distinguishable. WirePlumber exposes
the normal desktop output and selects a newly available Cast sink without
changing the user's configured default. A later explicit output choice clears
that automatic selection.

The CastKMS card has playback only, fixed at 48 kHz, signed 16-bit stereo. An
audio-enabled grant lets Pronk open one anonymous kernel tap for that exact
attachment. The tap follows the playback position, returns the samples the
desktop actually sent, and supplies silence while playback is idle. Pronk
publishes it as an `Audio/Source` only in its private PipeWire graph; the
backend converts and encodes Opus on the same Cast media timeline as video.

Starting desktop playback before Pronk opens the tap is supported. Detach,
grant loss, ELD replacement, or device removal terminates the tap and the media
generation. Audio-source startup is required when audio was negotiated; it
does not silently degrade that generation to video-only.

The tap is an anonymous fd, not an ALSA capture PCM. The desktop session sees
the CastKMS card as an output and cannot enumerate the tap as a microphone.

### Can a television microphone be exposed as an input?

Not currently. Some televisions have microphones for local assistants, but
neither their presence nor a protocol for exporting raw microphone media has
been established for the supported Cast path. The backend capability model can
grow an input-media direction without giving a backend direct CastKMS access,
but that would require a proven Device protocol plus new backend-to-PipeWire
and desktop input policy. The presence of Alexa or Google Assistant does not
show that raw microphone media is remotely available.

## Identity, modes, control, and trust

### Where do the monitor manufacturer and product name come from?

The authenticated Cast control channel supplies the selected Device ID and,
when available, its Device model. An optional setup endpoint supplies richer
manufacturer and product strings, but those strings are not covered by Cast
Device authentication. Chromiacast bounds them and cross-checks the endpoint's
SSDP UDN against the selected Device ID when the endpoint supplies one.

Pronk resolves manufacturer presentation text to a PNP identifier when needed
and generates EDID/DisplayID for the virtual monitor. The assigned room name,
such as “Apartment Living Room TV,” is used for user-facing display and audio
naming because users can own multiple Devices of the same model.

Discovery advertisements and setup-endpoint strings are presentation data,
not authorization credentials. They are bounded and validated before crossing
process and EDID boundaries.

### Why are 16:10 modes filtered for Chromecast?

DRM timing details such as reduced blanking and sync polarity stop at the
virtual monitor. A Chromecast receives an encoded picture, not the original
modeline. The tested TCL Device cropped a 1680×1050 picture to its 16:9
presentation surface, so the Chromiacast backend currently advertises a
conservative whitelist of 16:9 presentation modes.

The generic Pronk offer and CastKMS remain capable of other modes for future
backends. Chromiacast retains 640×480 only as the compatibility timing required
by the generated CTA EDID. Explicit letterboxing could make additional aspect
ratios safe in the future.

### Does Pronk blindly trust any Device found on the network?

No for the Cast media identity, and deliberately not fully for presentation
metadata. Chromiacast performs the Cast device-authentication exchange,
including certificate-chain, nonce, live TLS certificate binding, signature,
and Device-supplied revocation checks when present. Pronk also cross-checks the
authenticated Device ID against the user's selection.

Names, model strings, discovery routes, and unauthenticated setup-endpoint
metadata are treated as bounded untrusted input. A selected network Device can
still lie, refuse service, or exploit a protocol implementation bug; the
backend sandbox and process boundary limit the authority exposed to that
attack surface.

### Why expose CEC when Chromecast does not carry an HDMI CEC wire?

CEC is the existing Linux abstraction for display power, activation, keys,
volume, and mute. CastKMS exposes a connector-associated CEC adapter, and
Pronk translates supported operations into normalized backend controls. The
kernel does not know which Device protocol, if any, a backend uses to satisfy
the operation, and a backend may report an operation as unsupported.

The current production Chromiacast backend implements volume and mute through
Cast control. The Chromiacast library has separately tested Android TV Remote
pairing and key APIs, but Pronk does not yet provide their credential storage
and backend wiring. Input-source selection is not exposed: injected Android TV
input keys did not change the input on the tested television, and Google's own
remote UI did not offer that control.

## Authorization and other questions

### How does session mode decide which process receives a grant?

Upstream Mutter does not implement Pronk's capture-grant API. Session mode
currently uses the experimental
[PronkProject Mutter fork](https://github.com/pronkproject/mutter), which owns
the CastKMS card as DRM master and brokers normal grants on the session bus.
The fork reuses Mutter's D-Bus access checker: it follows the installed
`io.github.pronkproject.Pronk1` well-known name and permits method calls from
that name's current unique owner. Each invocation carries that unique sender
name; the fork does not add a separate Unix-credential policy for the method.
Explicit unsafe debug mode bypasses the access checker. Pronk independently
validates every returned grant property before use.

The D-Bus decision controls which peer receives the descriptor. Possession of
the descriptor is the kernel authorization for subsequent connector and
capture operations. If Pronk leaves the bus, the fork closes the grantor and
the kernel revokes the holder.

### What prevents a backend from asking Pronk to capture another monitor?

The backend protocol contains negotiated Device capabilities and PipeWire
targets, not DRM paths, connector IDs chosen by the backend, or capture file
descriptors. Pronk chooses a free CastKMS slot, validates the compositor grant
against that exact slot and rights profile, and owns the capture actor itself.

Backend definitions are installed as root-owned, non-writable files and name a
fixed systemd service template and runtime-relative local socket. Both sides
require D-Bus `EXTERNAL` authentication for the same Unix UID and validate the
peer PID as the main process of the expected active systemd unit.

The two credential paths differ because systemd owns the `Accept=yes` listener.
On Pronk's side, `SO_PASSCRED` supplies `SCM_CREDENTIALS` with every incoming
read; Pronk rejects a missing credential or a writer change before validating
the backend unit instance. The backend has a direct connection to Pronk and
uses the socket's ordinary peer credentials before validating `pronk.service`.

### What is tested today?

Coverage includes Rust unit and actor tests, protocol fixtures, peer-credential
and systemd-activation tests, PipeWire policy gates, EDID conformance checks,
CastKMS KUnit suites, a kernel-feature configuration build matrix, and Fedora
VM product scenarios for grants, capture, cursor, modesets, PipeWire, audio,
CEC, teardown, and failure recovery. The live media gate requires encoded
video acknowledgements and public `Running` state; its A/V variant requires
acknowledgements for both streams. It correctly does not claim that a VM can
automate what pixels a physical television presented.

The complete stack has also been exercised on a Fedora Silverblue laptop with
a TCL Google TV for extended desktop, mirrored desktop, modesets, cursor,
audio, adaptive latency, and failure recovery. The Chromiacast Android TV
Remote library was exercised separately on that television; it is not part of
the production Pronk backend path described above. Hardware coverage is still
narrow; broader Device, GPU, compositor, and network testing is needed.

### What is the rationale behind the main design decisions?

The design starts from the user-visible requirement: a Device should behave as
a real monitor, participate in the normal display topology, and remain subject
to the compositor's existing monitor policy. A DRM virtual monitor provides
those semantics through the standard KMS interface. A plain screen-sharing or
writeback capture does not create an independent display. A compositor-native
virtual-monitor API can provide the same desktop model through a different
integration path, as the GNOME Network Displays comparison above explains.

Existing DRM leases also have a different authority model. Pronk needs the
compositor to retain display ownership while granting another process narrowly
scoped access to completed frames. CastKMS grants express that relationship
explicitly, revoke capture when either side closes its descriptor, and prevent
pixels retained from a previous DRM master from crossing an authority boundary.

The PipeWire boundary is likewise deliberate. It keeps network credentials and
protocol code outside the display service, gives media backends a conventional
integration point, and leaves room for Chromecast, Miracast, AirPlay, or other
backends without moving protocol policy into the kernel or compositor. A
restricted PipeWire remote preserves that modularity without exposing desktop
frames to ordinary session clients.

Finally, attachment, media routing, and Device connectivity have separate
lifetimes because they are separate facts. Keeping the connector stable across
ordinary media restarts avoids artificial hotplug churn, while explicit failure
handling still detaches a display when the cast can no longer be sustained.
This separation also makes the synchronization, DMA-BUF, audio, CEC, modeset,
and teardown paths independently testable.

For the detailed kernel contract, see the
[CastKMS capture-grant documentation](https://github.com/pronkproject/castkms/blob/main/docs/capture-grants.md).
For the media protocol boundary, see the
[Chromiacast project](https://github.com/pronkproject/chromiacast).
