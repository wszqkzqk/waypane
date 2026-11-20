<div align="center">
  <img src="assets/waypane-logo.svg" alt="Waypane logo" width="144" height="144">

  # Waypane
</div>

Waypane displays Wayland applications from a remote Linux host or local Termux environment as
native windows on **Android, macOS, and Windows**, over SSH. The host only needs `sshd` and the
stock `waypipe` binary; Waypane starts `waypipe server -- <command>` automatically.

There is no Waypane daemon to install on the host, and the host does not need a running desktop,
compositor, GPU, or root access.

> **Status:** early development. The SSH/Waypipe/SHM path and the Android app have been exercised
> on-device against upstream Waypipe 0.11.3. macOS and Windows are built and tested in CI, but
> interactive desktop coverage is still limited, especially on physical Windows machines.

## Features

- **Headless-friendly:** works with a minimal SSH server through shared memory (`--no-gpu`).
- **Real Wayland windows:** multiple toplevels, popups, subsurfaces, cursors, and window metadata.
- **Native display scaling:** integer `wl_surface.preferred_buffer_scale` on Android and per-window
  host scale tracking on desktop, alongside pinch zoom and fit-to-view presentation.
- **Input and clipboard:** keyboard, pointer, touch, scrolling, text-input-v3 with soft-keyboard
  preedit on Android, and two-way text clipboard synchronization.
- **Mobile-ready controls:** pointer emulation or direct touch, auto-resize, landscape side rail,
  fullscreen mode, and a keyboard-aware view that pans without resizing the remote window.
- **SSH security and diagnostics:** password or public-key authentication, encrypted private keys,
  SHA-256 host-key confirmation, and clear remote command errors including exit code and stderr.

## Quick start

### 1. Prepare the host

Install `waypipe` and `sshd` on the Linux or Termux environment that will run the application.
OpenSSH must allow remote Unix-socket forwarding (`AllowStreamLocalForwarding yes`, the default on
standard OpenSSH configurations). No manual `waypipe server` command is needed.

For local Termux on Android:

```sh
pkg install openssh waypipe weston
sshd
whoami
```

Use `passwd` for password authentication, or add your public key to
`~/.ssh/authorized_keys`. Termux's SSH server listens on port `8022` by default.

### 2. Connect from Android

1. Open the **Waypane** app.
2. For local Termux, keep **Host** `127.0.0.1` and **Port** `8022`; for remote Linux, enter the
   SSH host and its port (usually `22`).
3. Set **SSH user** to the output of `whoami` on the host.
4. Choose **Private key** and import a key whose public key is in `~/.ssh/authorized_keys`, or choose
   **Password**. Imported keys are stored with owner-only permissions in app-private storage.
5. Set **Remote application command**, for example `weston`, `foot`, or `env LANG=C gtk4-demo`.
   Enter only the application and its arguments; Waypane manages Waypipe's own options.
6. Tap **Connect**. On first contact, compare the displayed SHA-256 host-key fingerprint with the
   host, then choose **Trust and save**. A changed key always requires explicit confirmation.

**Auto-resize** and **Wayland display scale: Auto** are enabled by default. Auto scale selects
`ceil(display density / 2)` clamped to 1x–4x (for example 2x on a 480dpi phone); 1x–4x can also be
selected manually from Setup or **Menu**.

### 3. Connect from macOS or Windows

Build the desktop client as described in [Building](#building), then run:

```sh
waypane-desktop --ssh user@linux-host:22 --identity ~/.ssh/id_ed25519 \
  --accept-new-host-key -- foot
```

Password and key passphrases can be supplied through environment variables instead of the command
line:

```sh
export WAYPANE_PASSWORD='...'
waypane-desktop --ssh user@linux-host --password-env WAYPANE_PASSWORD -- gtk4-demo
```

For an encrypted private key, add `--identity ~/.ssh/id_ed25519 --passphrase-env
WAYPANE_PASSPHRASE` instead of `--password-env`.

Host keys are checked against `~/.ssh/known_hosts` by default. `--accept-new-host-key` saves an
unknown host key but still rejects a key that changes later; verify fingerprints out-of-band when
using it. Override the file with `--known-hosts PATH` when necessary.

## Common options

- **Remote socket directory:** Waypane discovers a writable directory in `TMPDIR`, then
  `XDG_RUNTIME_DIR`, then `/tmp`, so Termux works without a `/tmp` directory. If automatic discovery
  is unsuitable, set **Remote socket path** in Android or pass `--remote-socket /absolute/path.sock`
  on desktop. The path must be absolute and at most 100 bytes.
- **Compression:** `lz4` (default) favors low CPU and latency on loopback and fast networks; `zstd`
  saves more bandwidth on slow or metered links; `none` minimizes CPU on local transports. Use
  `--compression none|lz4|zstd` on desktop or the matching chips in Android Setup.
- **Waypipe binary:** desktop uses `waypipe` from `PATH` unless `--waypipe-bin PATH` is supplied.
- **Window sizing:** Android Auto-resize asks applications to match the visible view while the GPU
  still fits whatever buffer size the client actually commits. Disable it to preserve the original
  remote geometry; black letterbox areas are never treated as remote input.

A remote message such as `could not load cursor 'dnd-none'` comes from the Xcursor theme on the
host, not from Waypipe. Install or configure a cursor theme on the host if custom cursor shapes are
needed; it does not affect desktop frames.

## Android controls

| Gesture | Default action |
| --- | --- |
| Tap | Left click at the touched position |
| One-finger drag | Left-button drag starting at the original position |
| Long-press | Right click |
| Two-finger drag at 1x fit | Smooth scrolling |
| Pinch, then drag | 1x–8x presentation zoom and pan |
| Quick two-finger tap | Toggle 2.5x focus at the fingers |

- **Direct touch input** in **Menu** sends native `wl_touch` events instead of pointer emulation.
- **Keys / Keyboard** opens the soft keyboard. The remote view translates upward to stay above the
  keyboard without reallocating its GPU surface or resizing the remote window. Touching the view
  never opens the keyboard by itself.
- **Fit** resets pinch zoom and pan. **Menu** provides display scale, Auto-resize, Direct touch,
  fullscreen, and **Close remote window**.
- Portrait uses a top bar; landscape uses a narrow end-side rail and hides the system bars so the
  remote window gets the full usable height. In fullscreen, a translucent Menu remains at the top
  right and Back first leaves fullscreen.
- **Back** asks for confirmation before disconnecting. Leaving with Home or Recents keeps the SSH
  session and windows alive through a foreground service with **Open** and **Disconnect**
  notification actions. If Android reclaims the process, the remote session must be reconnected.

## Downloads

Every CI run on `main` uploads artifacts to the
[Actions workflow](https://github.com/wszqkzqk/waypane/actions/workflows/build.yml) for
14 days:

- `waypane-android-debug-arm64`: Android arm64 APK (`minSdk 26`). Consecutive non-PR debug builds
  share a CI signing key, so `adb install -r app-debug.apk` updates in place and keeps app data.
- `waypane-desktop-macOS-ARM64`: macOS Apple Silicon desktop binary.
- `waypane-desktop-Windows-X64`: x86-64 Windows desktop binary.

These are CI debug artifacts, not signed releases.

## Building

The Rust workspace requires Rust 1.90 or later; CI tracks the latest stable toolchain.

Desktop:

```sh
cargo build --release --locked -p waypane-desktop
# target/release/waypane-desktop (waypane-desktop.exe on Windows)
```

Android builds require an Android SDK/NDK, JDK, Gradle, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked
export ANDROID_NDK_HOME=/path/to/ndk
cd apps/android
gradle :app:assembleDebug
# app/build/outputs/apk/debug/app-debug.apk
```

CI builds Windows, macOS, upstream Waypipe interoperability, and the Android APK on hosted runners,
so local development does not require those SDK installations.

## Current limitations

- Rendering uses `wl_shm` (`--no-gpu`). DMABUF/GPU passthrough, hardware video codecs, and audio
  forwarding are not supported.
- Clipboard synchronization is text-only. Drag-and-drop is not implemented.
- Native scaling is integer-only; `wp_fractional_scale_v1` and `wp_viewporter` are not advertised.
  Fractional desktop ratios are handled by host presentation, and Android also supports pinch zoom.
- Desktop keeps the host OS cursor and does not yet integrate native OS IME/preedit. Android
  composites remote cursor images and supports text-input-v3 through the soft keyboard.
- SSH agent authentication, Xwayland shell integration, and automatic reconnection after the client
  process or network is lost are not implemented.

## Development

The workspace separates the Waypipe wire protocol (`crates/waypipe-client`), Wayland compositor
state (`crates/waypane-core`), SSH transport (`crates/waypane-transport`), software/GPU composition
(`crates/waypane-render`), Android engine and JNI bindings (`crates/waypane-android`), and the two
native hosts (`apps/waypane-desktop`, `apps/android`). Upstream Waypipe 0.11.3 is vendored as the
protocol reference and interoperability-test peer.

Start with the crate-level Rust docs and the unit tests beside each crate. The upstream Waypipe
interoperability suites live under `crates/*/tests`, and
[`.github/workflows/build.yml`](.github/workflows/build.yml) is the authoritative definition of the
platform builds and the commands used to run them.

Local checks:

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo test -p waypane-android --features jni-check --locked
cargo clippy --workspace --all-targets --features jni-check --locked -- -D warnings
```

Dependencies track current stable releases with grouped Dependabot updates, while `Cargo.lock`
stays committed for reproducible builds.

## License

Waypane's own code is licensed under **GPL-3.0-or-later**. Vendored dependencies retain their own
licenses.
