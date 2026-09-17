# Anonymous DRM capture transport

`drm-capture` implements the final-image capture interface used by CastKMS. The
client has no modesetting, attachment, EDID, audio, allocation or raw-plane
operations. Grant issuance separately borrows the current DRM master and
returns distinct capture and revocation owners.

The client is a transport boundary, not a pool manager or another renderer.
It depends only on `nix`. The kernel remains authoritative for permission,
configuration names, increasing request IDs and outstanding request capacity.
No mirrored queue accounting is inferred from descriptor lifetime. The pool
manager separately tracks downstream ownership and native reuse.

## Operation and ownership

`Access::from_fd` retains an inherited descriptor without an ioctl. It is useful
when authority arrives before the display has been activated. Retention is not
validation: `Access::open` duplicates the descriptor with close-on-exec and
queries the active output. Failure leaves the retained access available for a
later attempt. Clones share the same kernel file and authorization, not a fresh
namespace or an independent permission lifetime.
`Access::describe` performs the same checked query directly on retained access,
without duplicating the descriptor. Display observation uses that operation;
it does not open streams or allocate names.

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

The qualification gate uses a two-CPU/2 GiB guest and linear 640x480 XRGB8888
output. It verifies exact changing pixels, source-alias rejection, backpressure,
failed admission without consuming a name, duplicate descriptors, an exported
native reuse fence closed immediately after admission, cancellation, destination
removal during an accepted request, revoked dequeue, a fresh grant in the same
authorization domain, and creator-file revocation while control is retained.
No test claims to revoke previously exported backing allocations.

## Application integration

Pronk's application owns an issuer-independent display session. Its configured
Mutter adapter obtains separate monitor-control, final-image capture, and
renderer capabilities. The display observer retains monitor control and the
issuer's release obligation. The final-image pipeline receives capture access.
The renderer pipeline receives separate renderer and capture capabilities: the
former admits complete-scene reads, while the latter owns final-image
destinations and publication. Neither pipeline receives monitor control or the
issuer's revocation files. See [display-session ownership](kernel-sessions.md).

`pronkd --capture-source final-image` selects capture without acquiring images
through a renderer endpoint or requesting a renderer transition. The default
is `renderer`. Selection is explicit, not an error fallback; it does not choose
the backend currently rendering the display.

`pronk_capture::Session` owns the stream and destination namespace for one
issued capture file. Create it once and reuse it across media generations.
Separate sessions made from duplicate descriptors would allocate conflicting
names. Each `spawn` reserves fresh stream and destination names; failed setup
does not reuse its reservation. Actors retain the file independently and each
new generation receives fresh destination storage.

The final-image pipeline performs pool allocation and stream setup on a
blocking worker, with the session retained across cancelled or abandoned
starts. Retries wait asynchronously for earlier setup. Cancellation is checked
before queued work accesses the file and between setup stages; it does not
cancel an allocator or ioctl already in progress. Failed registration attempts
close the new stream and remove all destinations registered by that attempt.

Both media paths report failures with the owning media generation. A normal
stop cancels health observation before joining the media owner. Neither a
stopped observer nor a timed-out cleanup wait establishes ended native access.

The renderer path reuses the same generic capture actor and PipeWire publisher
as final-image capture. Final-image capture allocates CPU-mappable destinations
from the configured DMA heap. A renderer pipeline also uses that allocation
path when its selected media profile requires system memory. When the profile
selects DMA-BUF storage instead, the renderer allocates the exact capture offer
on its selected Vulkan device and transfers those destinations into the shared
actor. Its additional renderer service only produces private completed images
and satisfies kernel-issued recipient claims. The capture queue does not encode
Chromecast's display cadence or transport window.

If upstream chooses V4L2 for buffer transport, it would replace these transport
operations rather than introduce a second production path. Keeping the client
separate from the renderer, display control and application state machine confines
that migration without promising identical buffer-lifecycle semantics.
