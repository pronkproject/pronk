# Frequently asked questions

## Why is Pronk a session service?

The graphical session already owns the compositor state that determines what
may be displayed and captured. Mutter authenticates the requesting process and
issues anonymous files for one CastKMS display session. Keeping Pronk on the
session bus preserves that authority chain without a privileged daemon or a
second control plane.

## Why does Pronk need a Mutter broker?

Opening a DRM node identifies a device; it does not prove that a process may
attach a monitor, read the completed image, or inspect the compositor's source
buffers. Mutter owns the display policy decision and can grant those roles
independently. The broker also ties their lifetime to the requesting login and
the selected CastKMS output.

## Why are monitor control, capture, and rendering separate files?

They confer different authority. Monitor control changes connector state and
EDID. Capture receives completed images. Rendering reads complete scene inputs
and is therefore trusted with raw display content. Separate files make the
least-privilege boundary visible to both the kernel and userspace.

## Why not give the renderer a DRM lease?

A lease delegates ordinary KMS objects and operations. The renderer needs no
modesetting authority; it needs bounded scene descriptions and source access
for work selected by CastKMS. The renderer file expresses that narrower
contract and can be revoked without transferring KMS ownership.

## Why is preparation not a DMA fence?

A DMA fence represents work that a driver has accepted. Preparation also has
to close admission for source reads that userspace has not submitted yet.
Turning an eventual userspace response into a fence would mix future admission
with native execution and could create dependency cycles in memory management.

A preparation ticket becomes ready only after admission is closed and every
accepted source read has kernel-retained native completion coverage. The GPU
work may still be pending. Mutter then retries the atomic update that would
retire those sources in the required order.

## Why can an atomic commit return `EAGAIN`?

An opted-in nonblocking KMS client must supply a preparation ticket. If the
ticket is not ready, CastKMS returns `EAGAIN` without accepting the update.
Mutter retains the high-level update, polls the ticket, rebuilds the atomic
request, and submits it again. Callers must use an ioctl path that preserves
`EAGAIN`; libdrm's generic retry wrapper is not suitable for a pending ticket.

## Why does the GPU path stage through a private image?

The compositor source must not remain retained while a downstream destination
is busy in PipeWire or an encoder. The source-reading transaction stages and
composes the complete scene into registered executor-owned storage. CastKMS
releases the source only after native completion covers that transaction's
source reads and final private write. A later operation may wait for and fill
an exported media buffer independently.

## Why not render directly into the encoder's buffer?

Direct rendering is valid only when the native driver's accepted dependency
set proves that destination reuse cannot add a wait to the source-reading job.
The private staging image makes that separation structural. A direct path can
be introduced as an optimization after it demonstrates the same lifetime
property for every supported format and driver.

## Why can't a generation number revoke a DMA-BUF?

An exported DMA-BUF descriptor continues to reference its allocation after a
protocol object is revoked. Generation numbers reject stale messages, but they
cannot remove access to storage already exported. Pronk therefore never
repurposes an allocation for pixels outside the recipient's authorization
scope.

## Does renderer failure have to be perfectly recoverable?

No. Pronk closes admission, releases work it can account for, and requests an
orderly return to the in-kernel renderer. A renderer crash is treated like a
GPU reset: the system should contain the failure and recover when practical,
but the design does not pretend that every failed native submission can be
reconstructed without loss.

## Why is capture separate from PipeWire?

The kernel decides who may receive the completed image; PipeWire transports
buffers between already authorized local components. Pronk uses a private
PipeWire remote so unrelated desktop clients cannot discover or connect to the
raw video stream.

## Why is the network backend a separate process?

The backend parses receiver traffic and communicates over the network. Keeping
it separate prevents that attack surface from inheriting renderer source files,
monitor control, or capture authority. It receives classified final-image
buffers and encoded-media responsibilities through narrow local protocols.

## Why can display refresh and receiver frame rate differ?

Capture cadence, encoder output cadence, transport acknowledgements, and
receiver presentation cadence are different measurements. Chromecast receivers
can acknowledge a 60-frame-per-second stream while presenting 30 frames per
second. Pronk negotiates the media profile explicitly and tests useful changing
frames rather than inferring presentation rate from acknowledgements.

## What happens when the renderer's capabilities change?

The renderer publishes one immutable constraints entry after its private
storage and native readiness checks complete. CastKMS includes that entry in a
generic per-CRTC constraints list, and Mutter selects its ID together with a
compatible atomic update. Each accepted scene remains bound to the exact
renderer and constraints that accepted it.

Withdrawal prevents new selection without cancelling accepted work. If the
renderer cannot continue, CastKMS removes its entry and restores the fixed
in-kernel constraints after outstanding obligations retire. GPU constraints
are not restricted to the fallback renderer's format ceiling.

## How is the end-to-end path tested?

Unit and integration tests cover protocol bounds, stale identities, closure,
revocation, multiple outputs, source and destination lifetimes, private
PipeWire policy, and backend activation. The virtual-machine gate exercises an
installed graphical session through a real receiver; see
[`../tests/vm/README.md`](../tests/vm/README.md).
