# Anonymous DRM capture transport

`drm-capture` implements the experimental final-image capture interface used by
the Rust CastKMS driver. It does not use the C driver's 0.12 capture protocol.
The client has no modesetting, attachment, EDID, audio, CEC, allocation or raw
plane operations. Grant issuance separately borrows the current DRM master and
returns distinct capture and revocation owners.

The client is a transport boundary, not a pool manager or another renderer.
It depends only on `nix`. The kernel remains authoritative for permission,
configuration names, increasing request IDs and outstanding request capacity.
No mirrored queue accounting is inferred from descriptor lifetime. The actor's
future pool manager will separately track downstream ownership and native reuse.

## Operation and ownership

`Client::from_fd` adopts an inherited descriptor after a successful description
query. It rejects inactive or revoked grants rather than pretending to validate
them without an active description. `create_grant` returns a client directly
from successful kernel issuance, which need not yet have an active image.
Neither operation retains the creating primary DRM file. Closing that file
revokes its grants even when their revocation files remain open.

Stream, destination and request names are distinct nonzero types. Each name is
local to its documented namespace; the numeric value is not a capability and
must not be used as a persistent identity across clients. The caller chooses
strictly increasing names. Admission failures do not consume them; admitted
names must not wrap or be reused. A configuration offer is issued by the kernel,
and its description does not authorize all future pixels.

| Operation | What success establishes |
| --- | --- |
| Open stream | An exact configuration and bounded request capacity |
| Register destination | Retained storage references, not exclusive access |
| Queue output | Admission with retained storage and optional native reuse fence |
| Dequeue | This request's destination access ended and its slot was acknowledged |
| Cancel | Cancellation requested, not completed writes or returned capacity |
| Close stream | That stream's destination writes ended; `EBUSY` is not success |
| Remove destination | A name was removed; accepted writes may still be active |
| Drop client | Observation abandoned; no guarantee of ended destination access |

The caller must exclude competing destination access. A reuse sync file covers
already-submitted native work, not work that a userspace process promises to
submit later. Kernel registration and reservation snapshots do not reserve the
allocation against another process. Closing or recycling a submitted descriptor
number does not replace the kernel's retained storage or fence.

`try_dequeue` returns `Ok(None)` for an empty queue without retrying `EAGAIN`.
A failed frame is instead `Ok(Some(completion))`, whose `outcome()` contains a
negative errno, including terminal `EAGAIN`. Successful output carries its
original `CLOCK_MONOTONIC` image-production time, not presentation or dequeue
time. Failed output may have partial pixels and is not a valid image.

Readability is shared across streams and duplicate descriptors. A scheduler
must inspect its streams, not assume a poll event reserves one result for one
reader. Revocation hangup does not mean the terminal queues are empty. Copyout
faults retain records in the kernel; malformed output detected after a successful
syscall has already been acknowledged and is a protocol failure, not a retry of
the same record. All operation errors return directly to the caller.

## Qualified HOST path

The live probe at `tests/drm-capture-live` uses ordinary KMS to select a source
and allocate independent destination storage. Every capture operation uses the
Rust client. Its C fixture contains no capture bindings or capture ioctls.

Build with the C compiler and `libdrm` development files installed:

```sh
cargo test --locked -p drm-capture
cargo build --locked -p pronk-drm-capture-live-test
```

Run **only inside an otherwise unused test VM** with the experimental Rust
CastKMS driver loaded, passing its node explicitly:

```sh
target/debug/pronk-drm-capture-live-test /dev/dri/card1
```

The example node is not an instruction to select `card1` on the host. The probe
requires DRM master, checks the driver name and development version, and changes
the output's mode and framebuffer. It does not choose a device automatically.
Ordinary `cargo test` does not start the live probe.

The September 14 qualification used kernel commit
`0cce0927a0de3e202f900f546f588fbf80ff27a6`, a two-CPU/2 GiB guest and linear
640x480 XRGB8888 output. It verified exact changing pixels, source-alias rejection,
backpressure, failed admission without consuming a name, duplicate descriptors,
an exported native reuse fence closed immediately after admission, cancellation,
destination removal during an accepted request, revoked dequeue, a fresh grant
in the same authorization domain, and creator-file revocation while control is
retained. Three consecutive runs passed alongside all 438 driver cases, followed
by successful module unload. No test qualifies unrestricted exporter latency or
revocation of previously exported backing allocations.

## Application integration

Session mode obtains separate monitor-control and final-image capture
capabilities from Mutter's display-session broker. The production display
observer retains the broker session and monitor capability, while the media
pipeline receives only capture access. Legacy system mode keeps the combined
CastKMS grant because its audio and CEC facilities do not yet have corresponding
generic capabilities.

The production session path uses the built-in reference renderer, which produces
private host images and copies them to registered destinations. Userspace
renderer activation, GPU-compatible media transport and hardware encoding remain
separate integration work. The capture queue does not encode Chromecast's
display cadence or transport window.

If upstream chooses V4L2 for buffer transport, it would replace these transport
operations rather than introduce a second production path. Keeping the client
separate from the renderer, display control and application state machine confines
that migration without promising identical buffer-lifecycle semantics.
