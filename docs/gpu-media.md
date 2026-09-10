# GPU media integration

The PipeWire producer accepts two explicit storage descriptions for one
packed memory plane. `VideoPixelFormat` separately selects XRGB8888 or
ARGB8888; producers selecting alpha must supply meaningful alpha values:

- `VideoBufferStorage::MappableLinear` preserves the existing CastKMS CPU
  capture path. The plane begins at offset zero, its modifier is linear, and
  PipeWire may advertise CPU mapping. The format offer omits a DRM modifier so
  the software consumer can negotiate ordinary raw-video caps.
- `VideoBufferStorage::DrmModifier { modifier, offset }` transports a layout
  supplied by a graphics allocator. The format offer includes the exact
  modifier, including zero. The allocation is not advertised as CPU-mappable.
  Negotiation must retain the modifier; a consumer cannot silently substitute
  a linear or implicit layout.

`VideoBufferLayout::size` is the whole allocation size, including any prefix
before the plane. PipeWire receives that whole size as `maxsize`, with the
plane's offset and the remaining allocation extent in its chunk. Every frame
publication restores those values. Pools require identical complete layouts,
including pixel format, storage kind and offset.

For linear storage, validation checks pitch and complete rows after the
offset. For other modifiers, pitch times height is not a valid allocation-size
formula. The caller must obtain a valid, single-memory-plane description from
its graphics API and the receiving API must support importing it. The transport
checks dimensions, signed PipeWire field bounds and offset containment; it
does not validate vendor-specific tiling. Auxiliary memory planes and other
pixel formats beyond those two are not supported by this initial adapter.

## Ownership and synchronization

The layout API does not submit GPU work or allocate buffers. Those operations
remain with the capture/executor owner, outside the PipeWire loop. In waited
transport, publication requires the caller to establish producer completion.
A `BufferReleased` event reports the end of PipeWire retention, not completion
of native GPU reads. Before rewriting storage, the owner must establish reader
completion as well, for example through a qualified DMA-BUF implicit-sync
bridge. Layout support alone does not establish that synchronization path.

Returning a buffer also does not revoke its exported storage. Pools must not
reuse backing allocations across incompatible recipient authorization scopes.

`pronk-dmabuf` provides the native synchronization primitives independently of
the capture protocol and PipeWire. `export_dependencies` snapshots the native
dependencies for an intended read, write, or read/write access. Write access
waits for both existing readers and writers. After submission,
`import_completion` enrolls the actual native completion for implicit users.
The caller must exclude competing submissions throughout snapshot, submission,
and import; these separate ioctls are not an atomic ownership transaction.

`SyncFile::wait` uses asynchronous readiness plus native completion status.
`Completion::Failed` means work ended without valid output. An ioctl or wait
error is not completion evidence. Dropping a wait does not cancel GPU work,
and a failed completion import does not undo submission. The allocation owner
must keep storage unavailable until native completion is established. None of
these operations belongs on the PipeWire loop or makes returned-buffer events
equivalent to native completion.

## Destination pool ownership

`pronk-gpu::output_pool::OutputPool` owns a bounded, distinct set of destination
DMA-BUF descriptors. It has no dependency on PipeWire, capture grants, or KMS.
Each slot follows the native and transport ownership sequence:

```text
prepare -> reader wait -> claim -> submit -> producer wait -> publish
              ^                                                |
              +---------------- transport return --------------+
```

`prepare_initial` establishes native completion before the first claim.
`claim` issues a single-use write permit only for a writable slot. `submitted`
imports the actual native write fence and returns an independent asynchronous
wait. Successful completion yields a publication permit; `publish` transfers
ownership before the caller attempts a transport handoff. Keep its publication
handle even if the handoff acknowledgement is lost. Only an authoritative
release or quiesced transport permits `returned`, which starts a native reader
wait rather than immediately allowing another write.

Waits carry pool identity, slot and use serial without borrowing the pool, so
one retained or pending destination does not prevent another from progressing.
Errors quarantine slots. Dropping handles or waits never recycles storage.
Graphics command/image lifetime and shutdown remain executor responsibilities;
closing an allocation descriptor or abandoning a wait is not GPU cancellation.

The pool owns output storage only. Do not attach compositor-source leases to
its pending waits. In the reference staged path, copy the compositor source
into independent private storage before copying to an exported destination.
If private storage is exhausted, capture demand remains source-unbound.
Recipients and authorization scope must remain fixed for the pool's lifetime;
new pool identities never justify cross-authorization backing-storage reuse.

The application-side `GpuOutput` adapter maps the pool to one immutable
`VideoNodeIdentity` and its registered buffer IDs. Source release events carry
the sequence retained from the submitted frame, not writable consumer metadata.
The source actor checks that sequence against its submitted use before emitting
a generation-scoped release. `GpuOutput` requires strictly increasing frame
sequences, so buffer ID, sequence and generation identify the active publication.

Call `begin_publish` before submitting its returned frame to `VideoSourceActor`.
The adapter retains the publication even if that handoff's acknowledgement is
lost. `handle_event` returns native waits for initial availability and matching
releases; the caller drives those waits asynchronously and applies their results
through `complete`. Stale-generation events are ignored; wrong-use releases are
rejected without consuming the active publication. The initial adapter accepts
waited transport only, not PipeWire synchronization timelines.

After joining the source loop, pass its stop report to `stopped`. A matching
generation-failure event has the same effect. Retirement includes all locally
held publications, even when the actor already queued their release events and
therefore omitted them from its reclaim list. It attempts each native reader
snapshot and reports individual failures without skipping the remaining slots.
Stopped generations never accept new claims or publications. The immutable
recipient scope and executor-owned graphics resource lifetimes still apply.

The installed media path does not instantiate the adapter yet. The opt-in
[generated GPU transport harness](../tests/gpu-media/README.md) connects it to
the Vulkan allocator/producer and a real source generation on a private graph.
Its optional VA H.264 profile converts into native NV12 storage, checks encoded
access units and verifies their decoded pixels in a separate CPU-readback oracle.
Neither the harness nor the adapter enables GPU media defaults.

## Optional Vulkan allocation

`pronk-gpu`'s `vulkan` feature provides a native device and image allocator.
It is disabled by default and has no dependency on PipeWire or capture policy.
`Device::open` matches the opened render node's device numbers against Vulkan
DRM properties, rather than selecting the first enumerated GPU. Vulkan opens
its own native descriptors; selecting a node does not make the driver adopt a
brokered fd or establish that it will run inside an installed service sandbox.

The backend requires Vulkan 1.1, external DMA-BUF memory, DRM modifiers with
their image-format-list dependency, foreign ownership, and importable/exportable
binary sync files. Each allocation additionally checks the selected modifier's
single-plane blit support, exportability and dimensions. Unsupported requests
fail without falling back to linear storage or another device. Modifier choice
must be negotiated with the intended importer; allocator support alone does
not qualify an encoder or a PipeWire consumer.

Images use dedicated device-local memory. Their immutable `ImageLayout` reports
B8G8R8A8 dimensions, modifier, plane offset, pitch and allocation size directly
from Vulkan. Images retain their device and loader; exported DMA-BUFs retain
backing storage after image destruction. No image is mapped for CPU access.
Allocation and export do not initialize pixels or establish producer completion.
Do not publish a newly allocated image until rendering has initialized it.
Keep each allocation within one compatible recipient scope for its lifetime.

Run the optional native allocation checks with explicit hardware selection:

```sh
PRONK_GPU_RENDER_NODE=/dev/dri/renderD128 \
PRONK_GPU_MODIFIER=0100000000000009 \
cargo test -p pronk-gpu --features vulkan --test vulkan_images -- --ignored
```

The node and hexadecimal modifier above are the Lunar Lake development tuple,
not portable defaults. The tests check four distinct exported images, alias
rejection by the output pool, close-on-exec descriptors, device/image/storage
lifetimes and rejection without fallback. `vulkan_device` separately tests
selection and repeated device teardown. These tests do not submit rendering,
read pixels, run PipeWire or qualify media performance. Vulkan validation layers
may be enabled through the usual loader environment for the opt-in tests.

The allocation flow follows the [Vulkan DRM modifier extension](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_image_drm_format_modifier.html).
Device selection uses [Vulkan DRM device properties](https://docs.vulkan.org/refpages/latest/refpages/source/VkPhysicalDeviceDrmPropertiesEXT.html).

## Waited GPU frame generation

`Image::clear_waited` initializes a generated frame with opaque RGB pixels using
GPU commands. It consumes the image until completion, snapshots native reuse
dependencies, acquires foreign ownership when needed, clears the entire image,
and releases it for foreign consumers. Actual submitted completion is enrolled
as a native writer before publication. The returned image and checked sync file
can drive the output pool's submitted/producer-completion transition.

Run that synchronous operation on a dedicated blocking graphics worker, such
as a bounded `spawn_blocking` task, never on a PipeWire loop or Tokio runtime
worker. It owns no compositor sources. The caller must hold exclusive output
ownership across snapshot, submission and completion enrollment; neither the
Rust image owner nor a reservation snapshot excludes competing external users.
Do not start one task per queued capture request: bound outstanding graphics
work by independently available output storage.

Command ownership lives separately from the clear operation. A native job owns
its command pool, binary semaphore, fence and operation resources. Queue host
access is serialized only during submission, not through completion waits.
Resources return only after the accepted job finishes. Error cleanup waits for
accepted work; device loss permits teardown but does not validate pixels. If a
native wait repeatedly fails without establishing retirement or device loss,
the exceptional path retains resources until process exit. That is not a normal
reuse or cancellation result. A worker crash still has best-effort semantics.

Vulkan may export `-1` for already-completed binary synchronization. That case
does not become an invalid owned descriptor: after native completion, the
operation returns a materialized reservation snapshot for the pool handoff.
No userspace-dependent future completion is put into a native fence.

With the explicit device/modifier environment above, run
`cargo test -p pronk-gpu --features vulkan --lib -- --include-ignored` for
native job ownership and changing-color rendering checks. Pixel readback uses
GPU copies into CPU-visible storage solely as a test oracle, checking every
pixel and alpha across repeated foreign handoffs. The generated-frame producer
itself does not map raw pixels. These tests do not run a media graph, simulate
device loss, or qualify an unsignaled downstream-reader stall.

## Waited copies from executor-owned staging

`destination.copy_from_waited(source)` copies a complete initialized image into
a separate same-size allocation on the same Vulkan device. Both image owners
move into the native job. Successful completion returns `CopiedImages`, with
the source, destination and a checked native completion for publication. The
operation rejects an uninitialized source, mismatched dimensions or another
device before recording GPU commands. It performs no scaling, blending or CPU
pixel transfer.

The caller must own exclusive native access across dependency snapshot and
completion enrollment. The source is executor-owned staging, never a retained
compositor-source lease: destination reuse may block this operation. A prior
source-to-private-stage operation must already have completed and released the
compositor source. Native completion is enrolled as a reader of the staging
allocation and a writer of the destination; it is not a promise of future
userspace submission.

Run copies on a bounded blocking graphics worker, as with clear operations.
Errors do not return either image for reuse. The submitted job retains both
allocations through native cleanup, including errors after submission. The
existing worker-loss and indeterminate-wait rules still apply.

The explicit native unit tests verify repeated copies, then rewrite the source
before reading every destination pixel. They also reject undefined sources,
extent mismatches and distinct Vulkan devices. CPU mapping remains confined to
the shared test oracle. Those tests use locally allocated images; they do not
qualify foreign-source import or the complete capture/staging pipeline.

## Imported sources and private-stage admission

`Device::import_source` imports a fixed single-plane B8G8R8A8 transfer image
using a source DMA-BUF and its separately retained producer sync file. Import
is deliberately an unsafe Rust boundary: its caller must establish compatible
same-physical-GPU allocation metadata, native layout/ownership release, and
source-read authority. A valid descriptor and numeric bounds do not prove
those cross-API facts. No native-driver access-control extension is required.

The importer checks descriptor type, backing size, plane bounds and native
format/modifier support before binding compatible imported memory. Linear rows
are checked with overflow handling; tiled extents remain the native driver's
responsibility. Vulkan receives a duplicate fd, whose ownership transfers only
after successful memory allocation. The Rust source owner retains its own fd,
producer fence, image, imported memory and device through native reading.

`SourceImage` is distinct from writable `Image`: it has no clear operation or
conversion into an output allocation. `copy_into_waited` consumes one source
use and copies into a same-device, same-size private destination. It rejects
aliased backing storage and queries destination completion without waiting.
Pending destination work returns `WouldBlock` before source-producer waiting
or reading. The caller still must exclude racing external access; a completed
reservation snapshot does not provide exclusivity.

After admission, the operation waits for the explicit producer and current
native writer dependencies, preserving failed-completion status. Submitted
completion covers only source reads and private-stage writes. Successful return
destroys the source import and returns the staging image and checked completion;
the higher layer remains responsible for the corresponding source-use release.
Errors retain accepted native work through cleanup, not through a fabricated
successful completion. Indeterminate native failures keep best-effort worker-loss
semantics rather than claiming revocation of retained descriptors.

The native tests use two Vulkan devices on the selected physical GPU, then
overwrite the original source before copying private storage to output. They
also test imported backing surviving producer-device destruction and alias
rejection. The pending-destination predicate has a deterministic unit test;
that is not a real unsignaled native-reader stall experiment. The import remains
unqualified for arbitrary compositor formats, modifiers, GPUs or source policy.

Native ownership follows the Vulkan [memory-fd import contract](https://docs.vulkan.org/refpages/latest/refpages/source/VkImportMemoryFdInfoKHR.html)
and [explicit modifier layout contract](https://docs.vulkan.org/refpages/latest/refpages/source/VkImageDrmFormatModifierExplicitCreateInfoEXT.html).

## Current scope

Existing casting callers select `MappableLinear`; they do not opt into GPU
layouts automatically. The generated-image harness joins allocation, native
reuse and hardware encoding for one explicit test tuple. Compositor-source
composition and installed service render-node access remain separate integration
work. The default software media graph and installed service
sandboxes are unchanged. A transport-level modifier test is not qualification
of the complete private PipeWire, encoder or receiver path.

Run `cargo test -p pronk-pipewire --lib` for layout-boundary, modifier-negotiation,
native metadata and existing ownership tests. The metadata tests use ordinary
descriptors without GPU access; they do not qualify a particular GPU modifier.

Run `cargo test -p pronk-dmabuf` for synchronization unit tests. With access to
`/dev/dma_heap/system`, run
`cargo test -p pronk-dmabuf --test native -- --ignored` for actual kernel
export/import, close-on-exec, and completed-fence waits on an empty allocation.
That opt-in test does not exercise unsignaled GPU work or hardware failures.

Run `cargo test -p pronk-gpu` for destination ownership and compile-fail tests.
`cargo test -p pronk-gpu --test native_pool -- --ignored` requires system-heap
access and exercises actual fence ioctls over repeated pool cycles. It holds
one destination across other slots' reuse and checks cancellation retention.
Those tests use completed no-op fences, not real producer or consumer GPU jobs.
Where elevated access is necessary, build as the normal user and run only the
generated native test executable with sudo.

Run `cargo test -p pronk-pipewire --lib` and `cargo test -p pronk --lib` for
release-sequence and adapter guards. Run
`cargo test -p pronk --test gpu_output_native -- --ignored` for the adapter
with real system-heap fence exchange and synthetic actor events. It checks
old releases after republication and shutdown
after an unacknowledged handoff. It does not run a PipeWire graph or GPU job.
