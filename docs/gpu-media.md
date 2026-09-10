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

`Device::identity` returns Vulkan physical-device and driver UUIDs for
compatibility checks between separate instances. Equality does not qualify a
format, modifier or external-memory handle, and these values are not a stable
product-level device identifier. Native device tests compare nonzero identities
across repeated opens of the selected node.

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

`Image::clear_rgba_waited` uses the same ownership and synchronization path
with an explicit stored alpha value. It does not premultiply RGB or infer a
blend equation. That supplies native alpha fixtures without CPU pixel writes;
such images do not satisfy an opaque-only composition or media contract unless
the chosen alpha is opaque. The RGB-only entry point still writes alpha 255.

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

An explicit-alpha case imports images from a separate matching Vulkan device,
copies into private storage, overwrites the originals and checks every stored
channel for transparent, intermediate and opaque alpha. It qualifies byte
preservation, not GPU alpha blending.

## Non-exportable rendering intermediates

`Device::allocate_private` creates an exclusively owned `PrivateImage` with
optimal tiling and RGBA32 floating-point storage. The precise storage-image and
blit capabilities and extent are queried before allocation. It has no DMA-BUF
export, clone, native handle or CPU mapping API, and the allocation enables no
external-memory handle types. Keeping private rendering storage distinct from
shared `Image` allocations makes accidental downstream publication unavailable
through the safe interface.

The initial operations are whole-image opaque `clear_waited` and
`copy_into_waited` into an equally sized shared image on the same device.
They consume their owners until native completion. Private storage keeps local
queue ownership; only the shared destination participates in foreign ownership
and reservation-fence enrollment. Uninitialized or mismatched sources fail
before waiting for destination reuse. The output conversion uses a nearest
format blit without scaling or color-space processing.

Destination reuse may block the output operation, so compositor-source claims
must have ended before it starts. The private image retains neither raw-source
imports nor deferred readers. Its type excludes competing external users of
the private allocation; the caller still excludes competing accesses to the
shared destination. This is not a universal guarantee about other GPU workloads
or native-driver scheduling on the same physical device.

RGBA32 uses sixteen bytes per pixel, four times the packed shared-output
storage before native alignment. It is a shader-intermediate profile, not a
qualified production memory-bandwidth or media-cadence choice. Native tests cover
non-square images, device-owner teardown, independent private reuse, complete
output pixels and rejection of unsupported extents or invalid copies.

`SourceImage::copy_into_private_waited` acquires a whole imported source into
same-sized private storage. It waits for explicit producer completion and the
source's reservation writers, then submits a packed-to-floating-point blit.
Any exported native completion is enrolled as a source reader. Successful
return waits for native completion and destroys the import before exposing the
private image; it does not expose an early accounting record. No shared output
allocation or downstream destination dependency participates in that operation.
Source authority and exclusion of source pixel reuse remain caller duties.

`submit_private_copy` splits that operation into accepted work and successful
pixel access. Its `PendingPrivateRead` retains the import and private image,
exposing an optional native completion record immediately after submission and
source-reader enrollment. `None` specifically represents Vulkan's already
completed SYNC_FD sentinel: no pending dependency remains, rather than a promise
to supply a fence later. `Some` can be duplicated for trusted source-use
accounting without extracting the private image. Both outcomes require a
successful `wait` before private pixels are returned. Drop may block on native
retirement, so the pending owner stays on a blocking graphics worker.

Native tests close and collect a trusted source use before extracting private
pixels, then overwrite the original before output conversion. They also check
retirement when the pending owner is dropped. A fast GPU may complete before
the observation; these tests do not claim forced unsignaled execution or crash
recovery. A compile-fail example keeps pending work out of the output API.

The native regression exercises all 256 values in each color and alpha channel
through source-to-private-to-output conversion. It overwrites and destroys each
producer allocation before allocating the shared output, then checks every
output pixel. That qualifies whole-image channel preservation on the selected
device, not shader blending, color conversion or asynchronous source accounting.

### Private shader blending

`PrivateImage::blend_waited` blends one completed, equally sized private image
over another. It accepts the reference renderer's `Blend` policy: ignored pixel
alpha, premultiplied alpha or coverage alpha, plus normalized 16-bit plane alpha.
The compute shader keeps encoded RGB, rounds intermediate color to normalized
16-bit precision and writes opaque output alpha. The whole-image convenience
operation performs no placement, scaling, gamma or color-space conversion.
Source imports must already have
retired; neither image is exported or carries downstream reuse dependencies.

`blend_region_waited` accepts a checked integral crop, signed output placement
and every orthogonal rotation/reflection combination from the reference model.
Source-axis reflection precedes counter-clockwise rotation. Rust clips the
transformed crop to the output using widened integer arithmetic, and passes
bounded unsigned source, destination and visible coordinates to the shader.
Fully invisible crops return both images unchanged without submission; callers
should omit invisible source acquisition earlier. Only affected pixels receive
opaque alpha. Cropping changes no source lifetime and does not imply scaling.

Each operation validates dispatch limits and owns both images, their views and
descriptors through native completion. Source and destination are uniquely
owned and cannot alias through the safe interface. Resource teardown uses the
same native job owner and device-loss handling as the transfer path.

`Device::create_blender` creates an explicitly owned reusable compute program.
`Blender::blend_region_waited` shares only immutable shader and layout state;
each call creates distinct views and descriptor storage for its images. Clones
may serve independent blocking workers. Accepted jobs retain the program, and
the program retains its logical device, without a device-to-program reference
cycle. Queue capability and local workgroup checks happen during creation;
image identity and dispatch bounds remain per-operation checks. The
`PrivateImage` convenience methods still create a temporary program, so callers
that need reuse keep a `Blender`. This is not a general pipeline-cache or
throughput qualification.

Native tests compare all three blend modes, six pixel-alpha values, six plane
alpha values and a mixed three-layer stack against the integer CPU reference.
Every pixel is checked on a non-workgroup-aligned extent, with at most one byte
of RGB error permitted and exact alpha. That is an explicit floating-point
qualification tolerance, not a claim of bit-identical arithmetic or arbitrary
stack-depth error bounds. Tests also retain and inspect unchanged source pixels
and reject uninitialized images, mismatched dimensions and different devices.
An asymmetric alpha-bearing source pattern exercises all sixteen orthogonal
transform combinations at clipped, one-pixel and extreme offscreen placements.
Each output is compared with the CPU renderer after the producer is destroyed.
Three concurrent native workers also reuse one program across distinct images,
with the original device facade and program owner dropped before they finish.
Their descriptors remain independent across repeated output verification;
matching images from a different program device are rejected.

The readable compute shader and compiled SPIR-V module live together under
`crates/pronk-gpu/src/vulkan/private/blend`. Normal builds use the checked-in
module without invoking a shader compiler. The artifact was generated with
glslang 16.3.0 targeting Vulkan 1.1; its source records the regeneration command.
Run `bash crates/pronk-gpu/tests/check-blend-shader.sh` with that compiler to
verify byte-identical regeneration. A compiler upgrade requires explicit
artifact review and renewed native qualification. The native binding and
dispatch checks follow the [Vulkan compute dispatch contract](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdDispatch.html).

### Post-composition gamma

`Device::create_gamma` prepares an immutable table and shader for one logical
device. It accepts the reference model's 1..=65536 uniformly spaced RGB entries,
including singleton constants and descending tables. Setup uploads only color
metadata into a non-exportable device-local storage buffer; no raw frame is
mapped. Native storage limits are checked, and unsupported tables fail without
resampling or falling back to CPU rendering. The buffer uses sixteen bytes per
entry before allocation alignment and command-storage overhead.

`Gamma::apply_waited` consumes an initialized private image and applies that
table in place after composition, before packed output conversion. Input RGB is
rounded to normalized 16-bit values; unsigned integer interpolation follows the
reference's rational position and half-up rounding. The table-size bound keeps
both the position product and rounded weighted sum within 32 bits. Output stays
at normalized 16-bit precision and alpha is unchanged. No degamma, color matrix,
transfer-function inference or per-plane color management is implied.

The shader and table are immutable and cloneable; image views and descriptor
sets belong to individual native jobs. Jobs retain every resource through
completion, and errors return no image for reuse. The program retains its device
without a device-owned reverse reference. There is no source import or downstream
destination dependency in the gamma operation.

Native tests check every byte input at six table sizes, including the maximum,
against reference output with exact byte comparisons. Separate checks cover
imported alpha after producer destruction, invalid tables, wrong-device and
uninitialized images, and concurrent program reuse after the original owners
are dropped. Those cases do not exhaust all 16-bit intermediate values or prove
arbitrary composed scenes bit-identical to the CPU blend implementation.
The shader lives under `crates/pronk-gpu/src/vulkan/private/gamma`; run
`bash crates/pronk-gpu/tests/check-gamma-shader.sh` with glslang 16.3.0 to verify
the checked-in Vulkan 1.1 module. Normal builds do not invoke a shader compiler.
The generated media fixture uses a two-entry green-inversion table after
composition and verifies decoded colors against the CPU reference. That
integrated curve is independent of the broader native table-size checks.

## Waited copies from exportable executor-owned staging

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

## Submission records before private pixels

`Image::submit_opaque` returns a `PendingStage` after native submission and
completion enrollment, without waiting for the composition to finish. Its
borrowed sync file represents accepted GPU work. A worker can duplicate that
descriptor into source-use accounting and close submission admission before
waiting for pixels. There is no userspace promise hidden inside the record.

The pending owner retains all source imports, command resources and private
storage. It exposes no image that another operation could reuse. Consuming it
with `wait` checks native completion, destroys the imports and returns the
initialized private image. The existing `compose_opaque_waited` operation uses
the same submission path followed immediately by that wait.

Both submission and retirement belong on a blocking graphics worker: submission
still waits for native producers, and dropping a pending owner waits for accepted
work. Returning a record early is not a nonblocking rendering API or a guarantee
that a fast GPU remains busy at return. Device loss and indeterminate wait errors
retain the same cleanup rules described above.

The native tests collect a closed source-use record set while the private image
remains inside its pending owner, then wait, rewrite the original source and
verify the private pixels. A separate test drops pending composition and checks
that its retained record has completed. Neither test forces a GPU stall or
claims crash-proof submission accounting.

## Placed source copies

`SourceImage::copy_region_into_waited` extends private staging to a visible
integral crop, using `drm-display-executor` geometry. It validates the crop's
source dimensions, clips signed placement against the output, and checks native
signed-offset conversions. Mismatched or fully offscreen input is rejected
before source waiting or submission; an offscreen plane should not acquire a
source use just to clear its background.

The operation clears opaque background pixels before copying the visible
region, with an explicit transfer-write dependency between those commands.
It retains the same independent-destination check, producer validity and source
completion contract as whole-image staging. Copied bytes, including alpha, are
preserved: it is not blending or color conversion. Using it as an opaque plane
requires opaque pixels in the same encoded RGB domain. Other visual profiles
must not silently take this copy path.

The native crop test generates a nonuniform source on the GPU and compares
three clipped placements with the CPU reference, after source and staging
rewrites. Only test-oracle readback maps pixels. The normal GPU unit command
includes deterministic geometry checks; the opt-in native command runs the
pixel comparison. Synchronization validation may be enabled with
`VK_LAYER_VALIDATE_SYNC=1` in addition to enabling the validation layer.

## Multiple opaque source planes

`Image::compose_opaque_waited` accepts owned `OpaqueLayer` inputs in explicit
bottom-to-top order. Each layer supplies one imported source, crop and signed
placement. It validates the complete list before any producer wait and requires
distinct backing allocations across all sources and private destination.
Repeated imports of one allocation are rejected even for disjoint crops;
sharing one source across several planes is not part of this initial profile.

The destination must already be independently available. Native producer waits
then precede one submitted job that owns all inputs, clears the background,
and orders each visible copy after preceding destination writes. Its completion
is enrolled for every source read and the private write. All source imports
are destroyed before returning initialized private storage. Downstream copies
and reuse remain separate; no encoder dependency is added to those source reads.

An empty list clears only the background. Omit fully invisible layers before
acquiring source uses; supplied invisible or mismatched crops are errors.
Opaque alpha and a shared encoded RGB domain remain caller requirements. The
API does not apply the reference renderer's selectable alpha equations, scale
images or convert colors. Accepted native work retains its resources through
completion on failure paths just as in single-source staging.

Native tests compare three overlapping clipped planes with the CPU reference
in both stacking orders after every source is overwritten and staging is
reused. They also cover empty composition and duplicate-import rejection.

`OpaqueLayer::with_transform` additionally selects source-axis reflection or
a half turn. The native adapter clips in transformed crop coordinates and
uses nearest-neighbor blits with reversed source edges as needed. It performs
no scaling; 90/270-degree rotations return `Unsupported` before producer waits.
Default identity layers retain the literal copy path. Both images use the same
UNORM format and the allocator's required blit capabilities; no sRGB conversion
or shader pipeline is introduced.

Blit coordinate rounding is implementation-dependent in Vulkan, so the native
profile is qualified by pixel comparison on the selected GPU/modifier tuple,
not by assuming arbitrary blits are byte-identical copies. The nonuniform crop
test covers all supported reflection/half-turn combinations and three clipped
placements after source and staging reuse. Geometry tests check reversed pixel
edges, one-pixel footprints and quarter-turn rejection. See the
[Vulkan blit contract](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdBlitImage.html).

## Current scope

The generated-image media harness uses the non-exportable shader path. Three
source copies report independent native completion before private pixels are
collected. Originals are overwritten before private shader composition, and the
composed image is overwritten after conversion into shared output. The scene
still uses opaque layers, followed by post-composition green inversion;
alpha modes and orthogonal transforms are qualified
by the separate native-versus-reference tests. The harness retains its private
PipeWire transport, hardware-encoder and transient-sandbox checks without
enabling the production renderer or claiming delivered frame rate.

Existing casting callers select `MappableLinear`; they do not opt into GPU
layouts automatically. The generated-image harness joins a separate producer's
source import, private staging, exported output reuse and hardware encoding for
one explicit test tuple. It overwrites source and staging before checking the
decoded output. Compositor-source
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
