# Waypane

Waypane is a native Wayland client receiver for non-Linux platforms. Its goal is
to display applications from headless Linux servers as local windows on
macOS, Windows, and Android, using the stock Waypipe server over SSH.

The first milestone targets Waypipe's shared-memory transport (`--no-gpu`), with
an all-Rust, in-memory client. It does not require local file-descriptor passing,
which is unavailable on Windows and restricted by application sandboxes on
Android.

## Workspace

- `crates/waypipe-client`: portable Waypipe wire protocol and shared-memory client
- `crates/waypane-core`: UI-independent Wayland protocol and buffer state
- `crates/waypane-transport`: SSH sessions and automatic remote Waypipe startup
- `crates/waypane-render`: native window presentation and input handling
- `apps/waypane-desktop`: macOS and Windows application
- `vendor/waypipe`: pinned upstream Waypipe Rust implementation, used as the
  protocol reference and integration-test peer

## Status

The repository is being rebuilt from an early prototype. The previous prototype
is preserved in the `archive/prototype-v1` branch.

`waypipe-client` currently implements version 16/17 connection negotiation,
incremental wire frame decoding, RID injection, Wayland message framing, and
uncompressed shared-memory fills, diffs, and file extension. Compression, pipes,
DMABUF, the Wayland compositor state machine, SSH, and native rendering are not
implemented yet.

## Build and test

```sh
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## License

GPL-3.0-only for Waypane's own code. Vendored dependencies retain their own
licenses.
