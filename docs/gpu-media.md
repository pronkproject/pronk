# GPU media integration

The PipeWire producer accepts two explicit storage descriptions for one
XRGB8888 memory plane:

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
including storage kind and offset.

For linear storage, validation checks pitch and complete rows after the
offset. For other modifiers, pitch times height is not a valid allocation-size
formula. The caller must obtain a valid, single-memory-plane description from
its graphics API and the receiving API must support importing it. The transport
checks dimensions, signed PipeWire field bounds and offset containment; it
does not validate vendor-specific tiling. Auxiliary memory planes and other
pixel formats are not supported by this initial adapter.

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

The pool API is implemented and tested independently; the application does not
yet wire it to a graphics allocator, renderer, or PipeWire event adapter.
That adapter must correlate release events to the exact active publication,
not infer ownership from a reusable slot number alone.

## Current scope

Existing application and live-test callers select `MappableLinear`; they do
not opt into GPU layouts automatically. Hardware encoding, GPU allocation,
native reuse-fence integration and service render-node access remain separate
integration work. The default software media graph and installed service
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
