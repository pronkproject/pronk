# Pronk

Pronk turns a CastKMS virtual display into a media stream for a receiver such
as a Chromecast. It runs inside the graphical login, obtains narrowly scoped
display capabilities from Mutter, and keeps display control, rendering, media
transport, encoding, and network delivery in separate components.

The default path is:

```text
Mutter session broker → CastKMS monitor and capture files
CastKMS built-in renderer or privileged GPU renderer → final image
Final-image capture
  → private PipeWire connection
  → media backend and encoder
  → receiver
```

Pronk does not open-endedly delegate the DRM primary node. Anonymous kernel
files carry monitor and capture authority for one display session. A separate
privileged service can read complete scene descriptions and produce the final
image. The network backend receives neither compositor source buffers nor DRM
authority.

## Requirements

- the Rust CastKMS driver and its generic DRM capture support;
- the privileged CastKMS renderer service for GPU composition;
- Mutter with the `org.gnome.Mutter.CastKms` session broker;
- PipeWire and WirePlumber;
- GStreamer with the plugins required by the selected backend;
- Rust, Meson, Ninja, and the development packages used by the workspace.

The Chromecast backend currently needs H.264 or VP8 encoding, Opus for audio
when audio is enabled, and the corresponding GStreamer parser and transport
elements. GPU encoding also requires access to the selected render node from
the backend service sandbox.

## Build and test

```sh
meson setup build
ninja -C build
meson test -C build --print-errorlogs
```

The Meson build compiles the Rust workspace and validates the installed D-Bus,
systemd user-unit, backend, PipeWire, and WirePlumber contracts. Rust tests can
also be run directly:

```sh
cargo test --workspace --locked
```

## Run

The installed D-Bus service activates `pronkd` on the session bus. Backend
sockets are systemd user units, so normal control does not require root or a
separate daemon.

List discovered receivers and create a display:

```sh
pronkctl list-devices
pronkctl add-display --device chromiacast:DEVICE-ID --no-audio
pronkctl list-displays
```

The current session bundle is video-only, so display creation must include
`--no-audio` until the broker publishes a separate audio capability.

The default `final-image` source selects generic capture without publishing a
renderer backend or selecting its display constraints. It uses CPU-mappable linear
destinations from `/dev/dma_heap/system`, which must be accessible to the
service account.
The installed service uses this source. Delegated GPU composition is provided
by the separate CastKMS renderer service, which publishes constraints that
Mutter can select without giving renderer authority to Pronk.

Remove the display by the identifier printed by `add-display` or
`list-displays`:

```sh
pronkctl remove-display DISPLAY-ID
```

Display setup is asynchronous. `pronkctl` reports validation, authorization,
device preparation, attachment, and final activation as the operation moves
through those stages. Interrupting setup requests cancellation and waits for
owned resources to retire.

## Component boundaries

`pronkd` owns device inventory and display-session state. It discovers CastKMS
outputs but receives their authority only through Mutter's session broker. A
display session contains independent monitor-control and capture files. The
renderer service obtains its own privileged endpoint, so a Pronk session cannot
acquire access to compositor sources.

The renderer worker consumes bounded, versioned complete-scene descriptions.
It stages compositor sources into private GPU images before an exported output
can wait on downstream reuse. That boundary lets CastKMS retire compositor
sources without depending on PipeWire or encoder progress.

The capture pipeline registers destination DMA-BUFs with the generic DRM
capture interface. Final-image capture uses CPU-mappable heap allocations.
Userspace rendering also uses mapped storage for software encoding, or
allocates the exact negotiated format and modifier on its selected Vulkan
device when the backend selects graphics storage. The capture source, backend
encoder, and fixed PipeWire caps must agree on the complete storage, format,
and modifier tuple. Both paths transport completed images over a private
PipeWire remote; the public desktop PipeWire instance is not the authority
boundary for raw display pixels.

Backends run as socket-activated user services. Their peer identity and
protocol version are checked before device inventory is accepted. The
Chromecast backend owns receiver discovery, encoding, and network transport,
but it never receives renderer source descriptors.

## Synchronization and lifetime rules

- Preparation readiness means that source-read admission has closed and all
  admitted reads have been accounted for. Their GPU work can still be pending;
  readiness is neither GPU completion nor presentation completion.
- GPU completion fences describe work already submitted to the native driver;
  userspace responses are not represented as future fences.
- A reusable renderer-private destination must be reserved before a
  source-reading job acquires its scene. Downstream destination reuse is not
  part of that job, so encoder backpressure cannot retain a compositor source.
- Exported DMA-BUF storage remains within one authorization scope for its
  lifetime. A new protocol identity does not revoke an old descriptor.
- Closing a session capability stops new work and begins bounded cleanup. A
  renderer crash is handled like a failed execution device: Pronk attempts an
  orderly handback and reports failure without claiming perfect recovery.

More detail is available in:

- [`docs/drm-capture.md`](docs/drm-capture.md)
- [`docs/kernel-sessions.md`](docs/kernel-sessions.md)
- [`docs/display-executor.md`](docs/display-executor.md)
- [`docs/gpu-media.md`](docs/gpu-media.md)
- [`docs/faq.md`](docs/faq.md)
- [`tests/vm/README.md`](tests/vm/README.md)

## Source layout

- `crates/pronk`: session daemon and display lifecycle;
- `crates/pronk-capture-broker`: Mutter broker client;
- `crates/castkms-monitor`: monitor-control operations without an issuer;
- `crates/castkms-renderer`: checked renderer protocol;
- `crates/pronk-renderer-worker`: scene execution worker;
- `crates/pronk-renderer-service`: renderer generation and native-thread lifetime;
- `crates/drm-capture`: generic final-image capture client;
- `crates/pronk-pipewire`: private media transport;
- `crates/pronk-chromiacast`: Chromecast backend;
- `crates/pronk-dbus`: public control API;
- `crates/pronkctl`: command-line client.

## License

Pronk is distributed under the MIT license. See [`LICENSE`](LICENSE).
