# Display-session ownership

Display setup and media lifetimes use application-owned interfaces. They do not
depend on the protocol used to obtain kernel authority. Mutter is the configured
issuer for monitor control and capture. A trusted administrative renderer
issuer is not yet connected to the installed service.

Code dependencies point toward the application-owned interfaces:

```text
display and media use cases → application-owned ports
Mutter adapter             → application-owned ports
Mutter adapter             → broker and kernel clients
```

`mutter_kernel_session` translates the broker into those application interfaces.
`pronk-capture-broker` implements the D-Bus protocol. The adapter retains issuer
connections; application session IDs are opaque diagnostic values.
`castkms-monitor` implements monitor ioctls without depending on Mutter. The
generic `drm-capture` client likewise
has no issuer or application dependency.

## One display lifetime, separate capabilities

`KernelSession` retains capture access, optional renderer access, and
`KernelSessionControl`. Control owns monitor operations and whatever keeps the
issuer's authority alive. Its diagnostic ID is local to the provider, not a
capability and not an identity comparable across issuers.

The Mutter adapter supplies no renderer access. Cloning capture access
duplicates only the same capture file description and does not transfer the
display lifetime. Retaining a capture descriptor therefore cannot prevent the
display owner from requesting release.

`KernelDisplay` owns attachment, route observation, and detach. It establishes
observation before mutating the monitor. Cancellation before attachment avoids
the mutation; cancellation after the operation began waits for it, detaches
when needed, and releases the session. Synchronous monitor calls run on a
blocking worker rather than on the async scheduler.

Explicit session release closes local capture access and then releases display
control. Ordinary drop requests cleanup too, but does not wait for it or promise
successful recovery after a process crash.

## Authority is not constraints selection

`RendererSession` is an application-owned interface for a trusted issuer.
Acquiring an endpoint neither publishes a backend nor selects display
constraints. The Mutter display broker does not implement this interface.

The renderer pipeline validates the endpoint and render node, prepares private
storage, completes its native readiness check, and publishes an immutable
configuration. The compositor discovers it through the generic KMS constraints
list and selects its ID with an ordinary atomic update. No private broker
request acknowledges or completes that selection.

The renderer pipeline requires a checked descriptor, render-node selection,
and an opaque release obligation from a separate trusted service. Compositor
cooperation grants no additional pixel access, and atomic acceptance is not GPU
completion.

CastKMS binds renderer authority to the compositor's DRM master identity. A
temporary transfer to another master makes renderer and capture operations
return `EACCES`; it does not turn the foreign master's pixels into an ordinary
stream failure that can be bypassed. Pronk suspends or retires the affected
media generation and waits for display observation to report active authority
again. It then reuses the retained capture access. A separately issued renderer
endpoint would require a fresh generation; configurations, jobs, and private
storage from the earlier master interval must never be revived.

## Release and abandoned operations

`CapabilityLease` requests exactly one asynchronous cleanup operation. Explicit
release also observes its result; dropping that wait does not cancel cleanup.
The Tokio runtime must remain alive for the worker to finish. Issuers bound
their outstanding acquisition and cleanup operations.

The Mutter broker retains a late-issued display session until the caller claims
it. If a reply is abandoned, cleanup goes to the original issuer, not a new
owner of the same bus name.

Explicit broker release closes local capability descriptors before waiting.
Its timeout bounds the caller's wait, not the remote operation. The provider
retains its capacity until that operation actually finishes. A timeout is a
reported failure, not confirmation that kernel or native work has ended.

## Choosing images without changing display authority

`CaptureSource::FinalImage` is the default. It clones capture access without
acquiring renderer authority, publishing a userspace renderer backend, or
changing display constraints. `CaptureSource::Renderer` requires authority from
a trusted issuer; the installed Mutter-based service cannot use it yet.

The final-image path operates with a provider that has no renderer endpoint.
Selecting the renderer path without one fails explicitly; it never silently
falls back after an authorization or pipeline failure.

The network backend receives final images and media configuration, not monitor
control, renderer source descriptors, or issuer revocation authority. Buffer
reuse and capture namespace lifetimes are described in
[the capture transport contract](drm-capture.md).

## Tests

Application tests use an independent fake issuer to exercise attach, observe,
cancel, detach, optional renderer ownership, and final-image selection. Private
D-Bus tests additionally cover the Mutter adapter's ownership transfers,
late replies, original-owner cleanup, and bounded release.
They do not start a compositor or prove an administrative kernel issuance path.

```sh
cargo test --locked -p pronk --lib
cargo test --locked -p pronk-capture-broker
cargo test --locked -p castkms-monitor -p drm-capture -p pronk-capture
```
