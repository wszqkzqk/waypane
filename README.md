<div align="center">
  <img src="assets/waypane-logo.svg" alt="Waypane logo" width="144" height="144">

  # Waypane
</div>

Waypane is **window-level Wayland forwarding over SSH** for **Android, macOS, and Windows**. Pick a
Linux application to launch, and its windows appear on your device instead of a video stream of an
entire desktop.

The host needs only `sshd` and stock `waypipe`. Waypane starts `waypipe server -- <command>` for
you; no Waypane daemon, monitor, running compositor, desktop environment, GPU, or VNC/RDP server is
required on the host.

## Why Waypane

### Run Linux apps on macOS, Windows, or Android

Connect to any headless server, VM, or Linux machine and start an application such as `foot`,
`nautilus`, or `gtk4-demo`. CPU shared-memory rendering (`--no-gpu`) works without an X server,
display manager, or GPU. Windows, popups, menus, cursors, integer display scaling, keyboard/pointer
input, and text clipboard all travel through one SSH connection. Authentication supports passwords
and (encrypted) private keys, with SHA-256 host-key verification.

### Use Waypane as a local display for Android Termux

Over local SSH, Waypane turns Termux into a Wayland environment without a separate VNC viewer or X
server:

- Start `weston` for a full local desktop shell and launch more apps from its terminal.
- Start `foot` or another Wayland app directly when you only need a terminal or one GUI tool.

Waypane discovers `$TMPDIR` automatically and sets `XDG_RUNTIME_DIR`, so `/tmp` is not needed.
Larger desktop environments still require their usual D-Bus and session services on the host.

### Use desktop Linux apps comfortably on a phone

The same per-window model works from Android to remote Linux. Density-aware native scaling makes
text and controls readable, Auto-resize fits apps to the visible area, and touch, soft-keyboard,
pinch-zoom, and landscape layouts replace the experience of viewing a shrunken desktop. A foreground
service keeps the session and windows alive when you leave the app with Home or Recents.

VNC/RDP forwards a preconfigured desktop; Waypane starts just the app you name and forwards its
windows. It targets interactive GUI applications through CPU-rendered shared memory, not GPU
pass-through or game streaming.

## Quick start

Install `waypipe` and `sshd` on the host and ensure OpenSSH allows remote Unix-socket forwarding
(`AllowStreamLocalForwarding yes`, the OpenSSH default). On Termux:

```sh
pkg install openssh waypipe weston
sshd
whoami
```

Use `passwd` for password authentication or add a public key to `~/.ssh/authorized_keys`.

### Android

- **Termux:** connect to `127.0.0.1:8022`; **remote Linux:** enter your SSH host and port (`22`).
- Set the user from `whoami`, import a private key or enter a password, and set the application
  command (`weston` for a desktop, or `foot`, `env LANG=C gtk4-demo`, etc.). Arguments are allowed,
  but Waypipe options are managed by Waypane.
- Tap **Connect**, compare the SHA-256 host-key fingerprint, and choose **Trust and save**. Changed
  host keys always require explicit confirmation. Imported keys stay owner-only in app storage.

Auto-resize and Auto display scale (`ceil(display density / 2)`, clamped to 1x–4x) are enabled by
default. Setup and **Menu** offer 1x–4x manually, along with compression (`lz4`/`zstd`/`none`),
Direct touch, and an optional absolute remote-socket path override.

### macOS and Windows

[Build the desktop client](#building), or download a CI artifact, then run:

```sh
waypane-desktop --ssh user@linux-host:22 --identity ~/.ssh/id_ed25519 \
  --accept-new-host-key -- foot
```

Use `--password-env WAYPANE_PASSWORD` for password authentication and `--passphrase-env
WAYPANE_PASSPHRASE` for an encrypted key. Host keys are read from `~/.ssh/known_hosts` unless
`--known-hosts PATH` is given; `--accept-new-host-key` saves unknown keys but never accepts changed
keys. See `waypane-desktop --help` for compression, remote-socket, custom-waypipe, and local-socket
options. `lz4` is the default; use `zstd` on slow links. Explicit-command mode does not read
`~/.ssh/config`: pass the user (`USER@HOST` or `--user`) and a real host name or IP instead of an
SSH alias. With an explicit command, Waypane exits once that command and its windows have closed.

On macOS and other Unix hosts, omitting `-- <COMMAND>` starts an interactive system OpenSSH shell
instead of the built-in `russh` launcher:

```sh
waypane-desktop --ssh work-server
```

This form uses `~/.ssh/config`, host aliases, ProxyJump, ssh-agent, OpenSSH's host-key prompts, and a
PTY. Waypipe starts your login shell on the remote host; run `nautilus`, `gtk4-demo`, or another
`foot` from that shell and each GUI client is forwarded as a separate Waypipe session. Closing a
forwarded window only ends that remote application; Waypane keeps the login shell and SSH connection
until the shell exits or you quit Waypane. Explicit
`--port`/`:port`, `--identity`, `--known-hosts`, and `--accept-new-host-key` are mapped to OpenSSH
options, while `--password-env` and `--passphrase-env` are available only in explicit-command mode.
Rust diagnostics are written to a separate log file instead of corrupting the terminal UI; the file
is kept and printed only when errors occur. Windows currently requires `-- <COMMAND>` because
interactive mode uses a local Unix-socket listener.

## Android controls

| Gesture | Default action |
| --- | --- |
| Tap | Left click |
| One-finger drag | Left-button drag |
| Long-press | Right click |
| Two-finger drag at fit | Scroll |
| Pinch and drag | 1x–8x zoom and pan |
| Quick two-finger tap | Toggle 2.5x focus; **Fit** resets |

**Direct touch** in **Menu** sends native `wl_touch` instead of pointer emulation. **Keys** opens the
soft keyboard and pans the view above it without resizing the remote window; touching the view never
opens the keyboard. **Menu** also provides fullscreen, Fit, and Close remote window.

Landscape uses a compact side rail and hides the system bars for full-height display. Back asks for
confirmation before disconnecting; the session notification also offers **Open** and **Disconnect**.
If Android reclaims the process, reconnect the session.

## Downloads

Recent [CI runs](https://github.com/wszqkzqk/waypane/actions/workflows/build.yml) on `main` upload
these artifacts:

- `waypane-android-release-arm64`: arm64 APK (`minSdk 26`), installable in place with
  `adb install -r app-release.apk`
- `waypane-desktop-macOS-ARM64`
- `waypane-desktop-Windows-X64`

## Building

Rust 1.90 or later is required:

```sh
cargo build --release --locked -p waypane-desktop
# target/release/waypane-desktop (waypane-desktop.exe on Windows)
```

Android builds additionally need an Android SDK/NDK, JDK, Gradle, `cargo-ndk`, the
`aarch64-linux-android` Rust target, and `ANDROID_NDK_HOME`:

```sh
cd apps/android
gradle :app:assembleDebug
# app/build/outputs/apk/debug/app-debug.apk
gradle :app:assembleRelease
# app/build/outputs/apk/release/app-release.apk
```

For a stable release signature, keep the keystore out of version control and set
`WAYPANE_KEYSTORE_FILE`, `WAYPANE_KEYSTORE_PASSWORD`, `WAYPANE_KEY_ALIAS`, and
`WAYPANE_KEY_PASSWORD` before running `assembleRelease`. Local builds can instead load `storeFile`,
`storePassword`, `keyAlias`, and `keyPassword` from `apps/android/signing/keystore.properties`;
environment variables take precedence. CI reads the same values from repository secrets, with the
keystore itself supplied as `WAYPANE_KEYSTORE_BASE64` (for example, the output of
`base64 -w 0 waypane-release.keystore`). All four secrets must be set. Without them, release builds
fall back to debug signing so that local builds and fork pull requests still work; these fallback
signatures are not stable across machines.

CI runs all platform builds and the upstream Waypipe interoperability tests on hosted runners; see
[`.github/workflows/build.yml`](.github/workflows/build.yml) for the current commands and toolchain
setup.

## Limitations

- `wl_shm` only: no DMABUF/GPU passthrough, hardware video codecs, or audio forwarding.
- Text clipboard only; drag-and-drop is not implemented. Native scaling is integer-only
  (`wp_fractional_scale_v1` and `wp_viewporter` are not advertised).
- Desktop keeps the host OS cursor and has no host IME/preedit integration yet. The explicit-command
  `russh` mode does not read `~/.ssh/config` or use ssh-agent; interactive OpenSSH mode uses both.
  Xwayland shell integration and automatic reconnection are not implemented.

## Development

Reusable code lives in `crates/` (`waypipe-client`, `waypane-core`, `waypane-transport`,
`waypane-render`, `waypane-android`), and native hosts in `apps/`. Upstream Waypipe 0.11.3 in
`vendor/waypipe` is the protocol reference and interoperability-test peer.

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --features jni-check --locked -- -D warnings
```

Dependencies follow current stable releases via grouped Dependabot updates; `Cargo.lock` is
committed for reproducible builds.

## License

Waypane's own code is licensed under **GPL-3.0-or-later**. Vendored dependencies retain their own
licenses.
