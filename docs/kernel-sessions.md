# Display-session ownership

Display setup and media lifetimes use application-owned interfaces. They do not
depend on the protocol used to obtain kernel authority. Mutter is the configured
issuer for monitor control and final-image capture. Its private broker admits
only Pronk's registered session-bus service, then binds
each issued capability to one exact CastKMS output.

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

`KernelSession` retains capture access and `KernelSessionControl`. Control owns
monitor operations and whatever keeps the
issuer's authority alive. Its diagnostic ID is local to the provider, not a
capability and not an identity comparable across issuers.

Cloning capture access duplicates only the same capture file description and
does not transfer the display lifetime. Retaining a capture descriptor therefore
cannot prevent the display owner from requesting release. Renderer authority is
held by the separate, privileged CastKMS renderer service, not by Mutter or
Pronk.

`KernelDisplay` owns attachment, route observation, and detach. It establishes
observation before mutating the monitor. Cancellation before attachment avoids
the mutation; cancellation after the operation began waits for it, detaches
when needed, and releases the session. Synchronous monitor calls run on a
blocking worker rather than on the async scheduler.

Explicit session release closes local capture access and then releases display
control. Ordinary drop requests cleanup too, but does not wait for it or promise
successful recovery after a process crash.

## Authority is not constraints selection

The privileged CastKMS renderer service issues its own endpoints and publishes
renderer constraints. The compositor discovers those constraints through the
generic KMS list and selects a compatible entry with an atomic update. Mutter
does not issue renderer endpoints; Pronk receives only final-image capture
authority. Neither publication nor selection acknowledges GPU completion.

CastKMS binds renderer work to the compositor's DRM master identity. A temporary
transfer to another master suspends access to compositor sources. The renderer
service observes the new owner interval and builds a fresh generation when
authority returns. Pronk separately observes final-image capture authority.

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

`CaptureSource::FinalImage` is the production source. It clones capture access
without acquiring a renderer endpoint, publishing a backend, or changing display
constraints. The installed renderer service composes the image independently;
Pronk receives only the final image selected by KMS.

The network backend receives final images and media configuration, not monitor
control, renderer source descriptors, or issuer revocation authority. Buffer
reuse and capture namespace lifetimes are described in
[the capture transport contract](drm-capture.md).

## Tests

Application tests use an independent fake issuer to exercise attach, observe,
cancel, detach, and final-image selection. Private D-Bus tests additionally
cover the Mutter adapter's ownership transfers, late replies, original-owner
cleanup, and bounded release. They do not start a compositor or prove a live
kernel issuance path.

```sh
cargo test --locked -p pronk --lib
cargo test --locked -p pronk-capture-broker
cargo test --locked -p castkms-monitor -p drm-capture -p pronk-capture
```
