# Waypane

Waypane is a native Wayland application receiver for macOS, Windows, and Android. It
displays Wayland applications running on local Termux or remote Linux hosts as native
windows over SSH, using the stock `waypipe server` on the application host. No compositor,
desktop environment, GPU, or root access is required on that host.

The client drives the remote `waypipe server` command automatically; a separate Waypane
daemon is not required.

## Architecture

- `crates/waypipe-client`: portable Waypipe wire protocol, RID/fd mapping, shared memory,
  pipes, outbound virtual files, and LZ4/Zstd compression, with allocation and size limits
- `crates/waypane-core`: UI-independent Wayland compositor state machine generated from the
  official `wayland.xml`, `xdg-shell.xml`, and `text-input-v3.xml` definitions
- `crates/waypane-transport`: `russh` authentication and host-key verification, OpenSSH
  `streamlocal` forwarding, and automatic remote `waypipe server` startup
- `crates/waypane-render`: damage-aware software layer composition and `wgpu` textures; the
  `desktop` feature adds `winit` native windows
- `apps/waypane-desktop`: desktop CLI, multi-session routing, native windows, input, and
  system clipboard synchronization
- `crates/waypane-android`: platform-independent `AndroidEngine`, single-threaded Tokio
  actor, JNI bindings, and `ANativeWindow`/`wgpu` presentation
- `apps/android`: Kotlin/Jetpack Compose Android host with connection settings, window
  switching, `RemoteSurfaceView`, touch, soft keyboard, and clipboard integration
- `vendor/waypipe`: unmodified upstream Waypipe `v0.11.3`, used as the protocol reference and
  integration-test peer

Every accepted application connection gets its own `SessionId`, `Connection`, Wayland object
space, and bounded I/O queues. Native windows are indexed by `(SessionId, WindowId)`, so
concurrent clients cannot collide.

The Android actor owns all connection and engine state on a current-thread Tokio runtime.
Rendering runs on a dedicated `RenderThread`; dirty-frame waits use versioned condition
variables, new frames are aligned to VSync with `Choreographer`, and static content does not
run a 60 Hz polling loop.

## Implemented

- Waypipe protocol versions 16/17, including `OpenFile`/`ExtendFile`/`BufferFill`/
  `BufferDiff`, `Close` status, and version negotiation
- Pipe creation, transfers and shutdown in both directions, plus outbound read-only virtual
  files such as `wl_keyboard.keymap`
- `none`, `lz4`, and `zstd` compression with frame, allocation, and decompression limits
- `wl_display`, `wl_registry`, `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`,
  `wl_output`, `wl_data_device_manager`, `xdg_wm_base`, `xdg_toplevel`, `xdg_popup`,
  `xdg_positioner`, and `zwp_text_input_manager_v3`
- Double-buffered surface state, ARGB/XRGB conversion, all eight buffer transforms, alpha
  composition, and damage-only uploads
- Subsurface `place_above`/`place_below`, `set_sync`/`set_desync`, synchronized parent
  caches, z-order composition, and popup/subsurface hit testing
- Toplevel and popup native windows, maximize/fullscreen/minimize, interactive resize, and
  drag requests
- Keyboard focus/keymap/modifiers, pointer enter/motion/buttons, `axis`/`axis_value120`, and
  touch events
- Frame backpressure: `wl_buffer.release` and `wl_callback.done` are sent after the associated
  native frame is submitted for presentation; occluded desktop windows retain backpressure
- Bidirectional text clipboard synchronization over Waypipe pipes, bounded to 16 MiB, using
  `arboard` on desktop and `ClipboardManager` on Android
- `text-input-v3` commit-count serials, surrounding text, Unicode preedit and commit strings,
  plus Android soft-keyboard integration; ASCII key and delete fallback is used when the
  remote application does not support `text-input-v3`
- SSH public-key or password authentication and strict `known_hosts` verification by default
- Android lifecycle-safe surface attach/detach, native-handle single destruction, SAF private
  key import into owner-only app-private storage, and backup exclusion for host keys and keys

## Build and test

Rust 1.90 or later is required by the workspace; CI tracks the latest stable release.

```sh
cargo check --workspace --all-targets --features jni-check --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --features jni-check --locked -- -D warnings
```

Integration tests that require a host-native upstream Waypipe binary:

```sh
WAYPIPE_TEST_BINARY=/path/to/waypipe WAYPIPE_TEST_COMPRESSIONS=none,lz4,zstd \
  cargo test -p waypipe-client --test upstream_shm -- --ignored --nocapture

WAYPIPE_TEST_BINARY=/path/to/waypipe \
  cargo test -p waypane-core --locked \
    --test upstream_clipboard --test upstream_text_input --test upstream_toplevel \
    -- --ignored --nocapture
```

These upstream tests check SHM pixel replication, damage, reverse keymap file descriptors,
two-way clipboard transfers, Unicode preedit/commit serials, xdg-toplevel lifecycle, and
keyboard/pointer delivery against the original Waypipe implementation.

### CI builds and artifacts

`.github/workflows/build.yml` runs the platform builds on GitHub-hosted runners so local
development does not require Android, NDK, Gradle, or Windows SDK installations:

- `quality`: formatting and workspace Clippy, including the host-side `jni-check` bindings
- `desktop`: workspace checks, tests, and release builds on macOS and Windows
- `upstream-interop`: builds vendored Waypipe with only SHM/LZ4/Zstd support and runs all four
  integration tests against it on Ubuntu
- `android`: resolves the latest LTS JDK and uses current Gradle, the runner's latest NDK,
  and the latest cargo-ndk to build the AGP 9 built-in-Kotlin arm64 APK (`minSdk 26`,
  `targetSdk 37`)

Workflow runs upload the desktop binaries as `waypane-desktop-<OS>-<ARCH>` and the APK as
`waypane-android-debug-arm64`. Heavy SDK installations and their licenses live only on
disposable CI runners. GitHub Actions, Cargo, and Gradle dependencies use current stable
versions, with Dependabot proposing grouped updates every week. `Cargo.lock` remains
committed so individual CI runs stay reproducible. The vendored Waypipe version stays fixed
as the protocol reference and test peer.

Local development only needs a Rust toolchain for workspace checks and unit tests. The
upstream integration tests remain opt-in when a host-native `waypipe` binary is available.

## Run over SSH

The application host needs `waypipe` and `sshd` with OpenSSH Unix-socket forwarding enabled.
Waypane automatically executes `waypipe server -- <command>` remotely.

```sh
cargo run -p waypane-desktop -- \
  --ssh user@linux-host:22 --identity ~/.ssh/id_ed25519 -- foot
```

Environment variables may supply secrets without putting them on the command line:

```sh
export WAYPANE_PASSWORD='...'
cargo run -p waypane-desktop -- \
  --ssh user@linux-host --password-env WAYPANE_PASSWORD -- gtk4-demo
```

For local Termux, start `sshd` (normally on port 8022), configure public-key or password
authentication, then connect to `127.0.0.1:8022` from either the desktop CLI or the Android
app. Host keys are verified against `~/.ssh/known_hosts` unless `--known_hosts` is supplied.
`--accept-new-host-key` disables verification of unknown or unreadable host keys and should
only be used deliberately. The Android app has an explicit toggle for the same trust choice.

## Local integration testing

On Unix hosts, `--socket` listens on a local Unix socket. In another process, start the
upstream server:

```sh
waypane-desktop --socket /tmp/waypane.sock --compression lz4

XDG_RUNTIME_DIR=/tmp waypipe --no-gpu --compress lz4 \
  --socket /tmp/waypane.sock --display waypane-test server -- foot
```

`--exit-after-frames N` is a hidden testing option that exits after at least `N` GPU frames
have been presented and the remote client has closed. Startup, protocol, transport, and
premature-disconnect failures return a nonzero exit status. An occluded or minimized native
window is not counted as presented.

## Verification status

- Host workspace tests, doc tests, and Clippy with `jni-check` and `-D warnings` pass.
- The `waypipe-client` upstream test passes for `none`, `lz4`, and `zstd` compression.
- The three `waypane-core` upstream tests for clipboard, `text-input-v3`, and xdg-toplevel pass.
- On this macOS host, a full desktop run against upstream Waypipe has created native windows
  on `Apple M4 Pro (Metal)` and presented two frames before a clean shutdown. A later
  automated run of the current code reached frame upload and `RedrawRequested`, but AppKit
  reported the 64x64 CLI-created window as `Occluded`, so that run did not claim a new on-screen
  presentation.
- The Android Rust/JNI code passed `cargo ndk` checking with NDK r27d for
  `aarch64-linux-android` at API 26 during development. That native check predates the
  subsequent AGP 9/Kotlin 2.4/API 37 migration. The full APK is built only in CI and that
  workflow has not run remotely yet; no physical Android device has been tested.
- `waypane-desktop` passed local `cargo check` and Clippy with `-D warnings` for
  `x86_64-pc-windows-msvc`. Native Windows check, test, and release-build jobs are in CI and
  have not run remotely yet; interactive behavior has not been verified on Windows.

## Known limitations

- DMABUF/GPU passthrough and video codecs are not supported; remote applications use
  `wl_shm` (`--no-gpu`), with software rendering on the application host if necessary.
- Clipboard support currently targets text MIME types, not arbitrary file or image formats.
- Desktop input uses physical/evdev key forwarding; native desktop IME/preedit integration is
  not implemented yet. Unicode soft-keyboard integration is implemented on Android.
- SSH agent authentication, Xwayland session management, audio forwarding, and session
  reconnection are not implemented.

The prototype preceding this implementation is preserved in the `archive/prototype-v1`
branch.

## License

GPL-3.0-only for Waypane's own code. Vendored dependencies retain their own licenses.
