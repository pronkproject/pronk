# CastKMS live tests

This package contains opt-in tests that require a graphical VM with the
matching CastKMS 0.11 module and Mutter CastKMS D-Bus API. The Rust binary owns
`io.github.pronkproject.Pronk1` on the session bus, requests a normal grant from
Mutter, validates the returned holder, and then exercises real attachment,
capture, mode-change, grant-state, DMA-BUF, and PipeWire paths.

Build it with the workspace:

```sh
cargo build --workspace --locked
```

Invoke it through one of the orchestrators in [`../vm`](../vm/README.md). The
binary deliberately does not open a DRM primary node or acquire elevated
privileges; if Mutter is absent, its API is unavailable, or another process
owns the Pronk bus name, grant acquisition fails.

`pipewire-video-source-consumer.c` is a deterministic PipeWire consumer used by
the direct source gate. Meson builds it as
`pronk-pipewire-video-source-consumer`; it is test-only and is not installed.
