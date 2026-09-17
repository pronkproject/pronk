# Display-session ownership

Display setup and media lifetimes use application-owned interfaces. They do not
depend on the protocol used to obtain kernel authority. Mutter is the configured
issuer; no administrative issuer or compositor-independent production setup is
implemented yet.

Code dependencies point toward the application-owned interfaces:

```text
display and media use cases → application-owned ports
Mutter adapter             → application-owned ports
Mutter adapter             → broker and kernel clients
```

`mutter_kernel_session` translates the broker into those application interfaces.
`pronk-capture-broker` implements the D-Bus protocol. The adapter retains issuer
connections and renderer endpoint numbers; application session IDs are opaque
diagnostic values. `castkms-monitor` implements monitor
ioctls without depending on Mutter. The generic `drm-capture` client likewise
has no issuer or application dependency.

## One display lifetime, separate capabilities

`KernelSession` retains capture access, optional renderer access, and
`KernelSessionControl`. Control owns monitor operations and whatever keeps the
issuer's authority alive. Its diagnostic ID is local to the provider, not a
capability and not an identity comparable across issuers.

Taking renderer access transfers that endpoint exactly once. Cloning capture
access duplicates only the same capture file description. Neither operation
transfers the display lifetime. Retaining a capture descriptor therefore cannot
prevent the display owner from requesting release.

`KernelDisplay` owns attachment, route observation, and detach. It establishes
observation before mutating the monitor. Cancellation before attachment avoids
the mutation; cancellation after the operation began waits for it, detaches
when needed, and releases the session. Synchronous monitor calls run on a
blocking worker rather than on the async scheduler.

Explicit session release closes local capture access, releases any unused
renderer endpoint, and then releases display control. A renderer release error
does not skip control release. Ordinary drop requests cleanup too, but does
not wait for it or promise successful recovery after a process crash.

## Authority is not constraints selection

`RendererSession` uses `RendererProvider` to issue a renderer endpoint for one
display lifetime. Acquiring that endpoint neither publishes an offer nor
selects display constraints.

The renderer pipeline validates the endpoint and render node, prepares private
storage, completes its native readiness check, and publishes an immutable
offer. The compositor discovers that offer through the generic KMS constraints
list and selects its ID with an ordinary atomic update. No private broker
request acknowledges or completes that selection.

The Mutter adapter only issues and revokes authority. Its endpoint numbers and
D-Bus connection never become native graphics identities. The renderer
pipeline receives a checked descriptor, render-node selection, and an opaque
release obligation. Compositor cooperation grants no additional pixel access,
and atomic acceptance is not GPU completion.

## Release and abandoned operations

`CapabilityLease` requests exactly one asynchronous cleanup operation. Explicit
release also observes its result; dropping that wait does not cancel cleanup.
The Tokio runtime must remain alive for the worker to finish. Issuers bound
their outstanding acquisition and cleanup operations.

The Mutter broker retains a late-issued session or renderer endpoint until the
caller claims it. If a reply is abandoned, cleanup goes to the original issuer,
not a new owner of the same bus name. Invalid reply metadata follows the same
cleanup path whenever the session identity is usable.

Explicit broker release closes local capability descriptors before waiting.
Its timeout bounds the caller's wait, not the remote operation. The provider
retains its capacity until that operation actually finishes. A timeout is a
reported failure, not confirmation that kernel or native work has ended.

## Choosing images without changing display authority

`CaptureSource::Renderer` is the default. It transfers renderer access into the
GPU pipeline. `CaptureSource::FinalImage` instead clones capture access and
uses the generic final-image queue. It neither publishes a userspace renderer
offer nor changes display constraints. Unused renderer access stays
session-owned.

The final-image path can operate with a provider that has no renderer endpoint.
Both paths keep the same display, media-generation, private PipeWire, and
failure-reporting interfaces. Neither selection silently falls back to the
other after an authorization or pipeline failure. The installed service still
uses the Mutter issuer in either mode.

The network backend receives final images and media configuration, not monitor
control, renderer source descriptors, or issuer revocation authority. Buffer
reuse and capture namespace lifetimes are described in
[the capture transport contract](drm-capture.md).

## Tests

Application tests use an independent fake issuer to exercise attach, observe,
cancel, detach, optional renderer ownership, and final-image selection. Private
D-Bus tests additionally cover the real Mutter adapter's ownership transfers,
late replies, malformed metadata, original-owner cleanup, and bounded release.
They do not start a compositor or prove an administrative kernel issuance path.

```sh
cargo test --locked -p pronk --lib
cargo test --locked -p pronk-capture-broker
cargo test --locked -p castkms-monitor -p drm-capture -p pronk-capture
```
