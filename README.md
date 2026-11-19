# Waypane

Waypane is a native Wayland application receiver for macOS, Windows, and Android. It
displays Wayland applications running on local Termux or remote Linux hosts as native
windows over SSH, using the stock `waypipe server` on the application host. No compositor,
desktop environment, GPU, or root access is required on that host.

The client drives the remote `waypipe server` command automatically; a separate Waypane
daemon is not required.

## Architecture

- `crates/waypipe-client`: portable Waypipe wire protocol, RID/fd mapping, shared memory,
  pipes, outbound virtual files, LZ4/Zstd compression, and copy-on-write SHM snapshots for
  queued frames, with allocation and size limits
- `crates/waypane-core`: UI-independent Wayland compositor state machine generated from the
  official `wayland.xml`, `xdg-shell.xml`, and `text-input-v3.xml` definitions
- `crates/waypane-transport`: `russh` authentication, SHA-256 host-key verification and TOFU,
  OpenSSH `streamlocal` forwarding, remote temporary-directory discovery, and automatic
  `waypipe server` startup; if the remote command exits before the first Waypipe stream is
  accepted, Android reports the command, human-readable exit code (when available), and up to
  4 KiB of trailing remote stderr instead of leaving `Connecting over SSH…` running indefinitely
- `crates/waypane-render`: damage-aware software layer composition and `wgpu` textures; the
  `desktop` feature adds `winit` native windows
- `apps/waypane-desktop`: desktop CLI, multi-session routing, native windows, input, and
  system clipboard synchronization
- `crates/waypane-android`: platform-independent `AndroidEngine`, single-threaded Tokio
  actor, handle-registry/Arc JNI lifecycle, and `ANativeWindow`/`wgpu` presentation
- `apps/android`: Kotlin/Jetpack Compose Android host with connection settings, host-key
  confirmation, private-key management, window switching, `RemoteSurfaceView`, touch, soft
  keyboard, and clipboard integration. The session-owned native handle lives in an
  application-scoped store rather than an Activity `ViewModel`, and a `specialUse` foreground
  `SessionService` with an ongoing Open/Disconnect notification keeps SSH sessions alive when
  the Activity is destroyed or moved to the background
- `vendor/waypipe`: unmodified upstream Waypipe `v0.11.3`, used as the protocol reference and
  integration-test peer

Every accepted application connection gets its own `SessionId`, `Connection`, Wayland object
space, and bounded I/O queues. Native windows are indexed by `(SessionId, WindowId)`, so
concurrent clients cannot collide.

The Android actor owns all connection and engine state on a current-thread Tokio runtime.
JNI calls resolve opaque IDs through a synchronized registry into `Arc` snapshots, so an
in-flight call keeps state alive while `nativeDestroy` removes its public handle. GPU access
is serialized by `Mutex<GpuRenderer>`. Rendering runs on a dedicated `RenderThread`; dirty-frame
waits use versioned condition variables, new frames are aligned to VSync with `Choreographer`,
and static content does not run a 60 Hz polling loop. Viewport and view-size changes update
their shared `AndroidSurface` directly instead of consuming the SSH actor's bounded command
queue; GPU holds that surface lock through presentation, so a concurrent change cannot be
cleared as stale.

Android presents every `SurfaceView` at its full size. `ViewportTransform` fits the remote
buffer without cropping, and `ViewportBlitter` clears the `ANativeWindow` to black and draws a
transformed, linearly filtered quad inside it. The same transform maps touch and pointer input
back into content coordinates and divides by `wl_surface.set_buffer_scale`, so letterbox clicks
are ignored and a touch that leaves the content is cancelled rather than delivered at a clamped
position. Viewport-only redraws reuse the current frame version and do not manufacture extra
Wayland frame callbacks.

## Implemented

- Waypipe protocol versions 16/17, including `OpenFile`/`ExtendFile`/`BufferFill`/
  `BufferDiff`, `Close` status, and version negotiation; legacy 0x10 `AckNblocks` frames used
  by the Waypipe 0.11.3 C implementation are ignored as upstream Rust does, while `Restart`
  is rejected and the negotiated version is logged
- Pipe creation, transfers and shutdown in both directions, plus outbound read-only virtual
  files such as `wl_keyboard.keymap`
- `none`, `lz4`, and `zstd` compression with frame, allocation, and decompression limits
- `wl_display`, `wl_registry`, `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`,
  `wl_output`, `wl_data_device_manager`, `xdg_wm_base`, `xdg_toplevel`, `xdg_popup`,
  `xdg_positioner`, and `zwp_text_input_manager_v3`
- Double-buffered surface state, ARGB/XRGB conversion, all eight buffer transforms, alpha
  composition, and damage-only uploads; `wl_shm_pool` keeps its logical size independent of
  a potentially larger backing fd, with non-shrinking bounded resizes as used by Waypipe C
- Each `BufferView` owns a read-only `ShmData` snapshot when it is committed. Later diffs and
  `ExtendFile` updates use `Arc` copy-on-write, so retiring a quickly superseded buffer and
  closing its pool cannot leave a queued resize frame with a missing RID. Unique SHM
  allocations queued by one receive batch are bounded to 256 MiB; Android and desktop both
  render from the frame-owned snapshot instead of looking up the RID later. As in upstream
  Waypipe, remote SHM mappings follow transport lifetime rather than `wl_shm_pool` object
  lifetime, so an asynchronous `BufferDiff` arriving after all pools/buffers are retired is
  still applied. Remote SHM remains bounded to 256 MiB per file and 1 GiB per connection;
  exceeding the total is an explicit resource-limit error. Pipes are released independently via
  `close_pipe` after EOF/shutdown, while `close_transport_file` is reserved for explicit
  transport-level removal. Unknown RID errors include the frame type and a bounded 64-entry
  `recently closed`/`not open` state. Canvas replaces a
  layer's pixel allocation whenever its committed dimensions change, and empty commits without
  attach, damage, callback, or z-order changes do not manufacture frames
- Disjoint `wl_region` rectangle algebra (bounded to 1024 rectangles, transactional on error),
  double-buffered `wl_surface.set_input_region` copy semantics, and half-open input hit testing for
  toplevels, popups, and subsurfaces. Each surface tracks its outputs and emits `wl_surface.enter`
  and `leave` on mapping, unmapping, late output binding, cursor replacement, and output release
- Subsurface `place_above`/`place_below`, `set_sync`/`set_desync`, synchronized parent
  caches, published z-order deltas, and popup/subsurface hit testing. Rearranging a stack updates
  the published order only when it changes
- XDG positioner validation, all anchor and gravity values, Flip → Slide → Resize constraint
  precedence, reactive parent configurations, and `xdg_popup.reposition` responses ordered as
  `repositioned`, `xdg_popup.configure`, and `xdg_surface.configure`. Popup geometry becomes active
  after its single-use `ack_configure`. `xdg_popup.grab` validates seat and recent input serial,
  focuses the topmost popup, and emits `popup_done` on outside press or Escape
- Toplevel min/max size constraints and reconfiguration, maximize/fullscreen/minimize, interactive
  resize and drag requests. Empty initial xdg-surface commits are allowed before `ack_configure`,
  while real buffer attachments remain gated by the initial ack; creating an xdg_surface after a
  buffer attachment is rejected. Destroying a window dismisses descendant popups and cleans up
  associated subsurfaces, touch contacts, pointer/keyboard focus, cursor mapping, and z-order state
- Keyboard focus/keymap/modifiers, unmodified evdev key codes for `wl_keyboard.key`, duplicate
  press/release suppression, focus-change key/modifier clearing, pointer enter/motion/buttons,
  `axis`/`axis_value120`, and touch events. A host test enumerates every non-destructor request in
  the generated official XML interfaces at the advertised versions and rejects missing or stale
  routing policies
- Native integer Wayland scaling through `wl_output.scale` and `wl_surface.preferred_buffer_scale`
  (v6). New surfaces inherit their window/output preference, while popups and subsurfaces inherit
  the nearest window override without changing sibling windows. Runtime changes notify existing
  surfaces and bound outputs, including an explicit scale of 1 when returning from HiDPI
- Android offers **Auto** (`ceil(displayMetrics.density / 2)`, clamped to 1x–4x) plus manual
  1x–4x selections in Setup and the window menu. Auto follows display configuration changes without
  reconnecting. Auto-resize converts physical View pixels to window-geometry logical units using
  `ceil(px / effective_scale)`, then the GPU continues fitting the client's actual committed buffer
- Desktop uses each native window's `ceil(Window::scale_factor())` as a per-window
  `preferred_buffer_scale` when the window is created and after `Resized`/`ScaleFactorChanged`.
  High-resolution buffers are presented into the existing host logical geometry, so 125%/150% and
  other fractional system scales preserve layout while glyphs are rendered at a higher integer
  buffer scale instead of being blurred by a 1x upscale
- Android uses pointer-emulating touch by default: tap sends a left click, drag sends a left-button
  drag, and long-press sends a right click. A window-menu switch selects native Direct touch
  instead without delivering both protocols. Pointer presses set keyboard focus, and tracked
  buttons are released even when the finger or mouse is lifted in a letterbox area. The soft
  keyboard opens only from the dedicated **Keyboard** button or **Show keyboard**, never on every touch
- `wl_pointer.set_cursor` tracks each pointer object's focus and latest `enter` serial, ignores
  stale serials, rejects conflicting surface roles, supports `NULL` hiding, and decrements
  hotspots on committed `wl_surface.offset`. Destroying an active cursor surface or pointer hides
  it. Android decodes cursor buffers once and draws them in an independent RGBA8,
  alpha-blended pass using the same clipped `ViewportTransform`; moving a cursor updates only
  its small vertex buffer and never re-uploads the desktop texture. Desktop keeps the native OS
  cursor
- Frame backpressure: `wl_buffer.release` and `wl_callback.done` are sent after the associated
  native frame is submitted for presentation; occluded desktop windows retain backpressure
- Bidirectional text clipboard synchronization over Waypipe pipes, bounded to 16 MiB, using
  `arboard` on desktop and `ClipboardManager` on Android; `wl_data_offer.accept` supports NULL
  or an actually offered MIME type, and `receive` requires an exact offered text MIME type
- `text-input-v3` commit-count serials, surrounding text, Unicode preedit and commit strings,
  plus Android soft-keyboard integration; ASCII key and delete fallback is used when the
  remote application does not support `text-input-v3`
- SSH password or public-key authentication, including encrypted private keys with an
  in-memory passphrase, strict `known_hosts` verification by default, and RSA keys negotiated
  as `rsa-sha2-512`/`rsa-sha2-256` without falling back to legacy `ssh-rsa` (SHA-1)
- Detailed authentication errors with `user@host:port`, the actual signature algorithm,
  public-key SHA-256 fingerprint, server-offered remaining methods, and partial-success
  state; Android mirrors these messages to the `Waypane` Logcat tag (`adb logcat -s Waypane`)
- Host-key TOFU: desktop `--accept-new-host-key` saves only unknown keys and still rejects
  changed keys; Android displays the SHA-256 fingerprint before saving a new key and requires
  explicit confirmation before replacing a changed key
- Automatic writable remote directory discovery (`TMPDIR`, then `XDG_RUNTIME_DIR`, then `/tmp`)
  for Termux and Linux, with `XDG_RUNTIME_DIR` set to that directory for headless/non-login SSH
  sessions, unique length-bounded sockets, and explicit `--remote-socket` or Android
  remote-socket overrides; application commands remain separate from Waypipe options
- Android atomic SAF private-key import with a 64 KiB limit, owner-only app-private storage,
  backup exclusion for keys and `known_hosts`, and a true delete-imported-key action
- Android acquires each `ANativeWindow` once and runs adapter selection, capability
  negotiation, configuration, and presentation on the same surface. `SurfacePlan` requires an
  RGBA/BGRA 8-bit format and `RENDER_ATTACHMENT` usage, keeps alpha/present mode/color space in
  `Auto` (including devices advertising only `Inherit`), and turns configure failures into
  ordinary errors through wgpu error scopes. Resources drop in strict order: texture/blitter,
  wgpu surface, `ANativeWindow`, Java surface. `Outdated`/`Timeout` results retry while dirty,
  occluded results wait for visibility callbacks, and RenderThread JNI errors are shown in the
  UI instead of crashing; `WaypaneRust` and `WaypaneRender` expose adapter/format and GPU errors.
  Any remaining actor panic is caught, logged with a forced backtrace, and forwarded to the UI as
  an error event; presentation completion distinguishes an already-closed actor from a full queue
  so the secondary queue error cannot mask the root failure
- Android always fits remote content without cropping, supports 1x–8x pinch zoom with panning,
  two-finger scrolling at fit, two-finger tap to toggle 2.5x focus, a Fit reset action, immersive
  fullscreen, and keep-screen-awake during sessions. Auto-resize defaults to on for phone-sized
  displays and can be disabled to preserve remote geometry; zooming never resizes the remote
  application, and actual committed buffer sizes drive fitting
- Android survives Home/Recents switches, task swipes, and Activity recreation without dropping
  the SSH session or remote windows. Back or **Back** asks for confirmation before disconnecting;
  if Android still kills the process despite the foreground service, the recreated Activity
  explains that the previous session was reclaimed instead of silently showing Setup

## Build and test

Rust 1.90 or later is required by the workspace; CI tracks the latest stable release.

```sh
cargo check --workspace --all-targets --features jni-check --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --features jni-check --locked -- -D warnings
cargo test -p waypane-android --features jni-check --locked
```

The host `jni-check` test uses null ANativeWindow stubs so JNI and surface-format logic can be
run without linking Android's `libandroid`; Android builds still link the real NDK library.

Integration tests that require a host-native upstream Waypipe binary:

```sh
WAYPIPE_TEST_BINARY=/path/to/waypipe WAYPIPE_TEST_COMPRESSIONS=none,lz4,zstd \
  cargo test -p waypipe-client --test upstream_shm -- --ignored --nocapture

WAYPIPE_TEST_BINARY=/path/to/waypipe \
  cargo test -p waypane-core --locked \
    --test upstream_clipboard --test upstream_text_input --test upstream_toplevel \
    -- --ignored --nocapture

WAYPIPE_TEST_BINARY=/path/to/waypipe WAYPIPE_SSHD=/usr/sbin/sshd \
  cargo test -p waypane-core --locked --test upstream_ssh -- --ignored --nocapture
```

These upstream tests check SHM pixel replication, damage, reverse keymap file descriptors,
two-way clipboard transfers (including `wl_data_offer.accept`), Unicode preedit/commit serials,
xdg-toplevel lifecycle, `wl_pointer.set_cursor(NULL)` after pointer enter, and exact evdev keycode
delivery against the original Waypipe implementation. `upstream_ssh` also
starts an isolated `sshd`, logs in with encrypted Ed25519 and RSA private keys while the
server is restricted to `ssh-ed25519,rsa-sha2-512,rsa-sha2-256`, exercises interactive TOFU,
discovers a Termux-style `TMPDIR`, and drives two real Wayland frames over SSH `streamlocal`
forwarding. It requires `ssh-keygen` and a `/run/sshd` directory on Ubuntu.

### CI builds and artifacts

`.github/workflows/build.yml` runs the platform builds on GitHub-hosted runners so local
development does not require Android, NDK, Gradle, or Windows SDK installations:

- `quality`: formatting and workspace Clippy, including the host-side `jni-check` bindings
- `desktop`: workspace checks, tests, and release builds on macOS and Windows
- `upstream-interop`: builds vendored Waypipe with only SHM/LZ4/Zstd support and runs all
  five integration tests against it on Ubuntu, including real `sshd` plus encrypted-key auth
- `android`: resolves the latest LTS JDK and uses current Gradle, the runner's latest NDK,
  and the latest cargo-ndk to build the AGP 9 built-in-Kotlin arm64 APK (`minSdk 26`,
  `targetSdk 37`)

Workflow runs upload the desktop binaries as `waypane-desktop-<OS>-<ARCH>` and the APK as
`waypane-android-debug-arm64`. Heavy SDK installations and their licenses live only on
disposable CI runners. GitHub Actions, Cargo, and Gradle dependencies use current stable
versions, with Dependabot proposing grouped updates every week. `Cargo.lock` remains
committed so individual CI runs stay reproducible. Non-PR Android builds also reuse a
CI-only debug-signing cache, so consecutive debug APKs can update over a previously installed
CI build without uninstalling its app data. The vendored Waypipe version stays fixed as the
protocol reference and test peer.

Local development only needs a Rust toolchain for workspace checks and unit tests. The
upstream integration tests remain opt-in when a host-native `waypipe` binary is available.

## Run over SSH

The application host needs `waypipe` and `sshd` with OpenSSH remote Unix-socket forwarding
enabled (`AllowStreamLocalForwarding yes` and `AllowTcpForwarding yes` or `remote`). Waypane
automatically executes `waypipe server -- <command>` remotely.

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
app. Waypane discovers Termux's `$TMPDIR` over SSH automatically, so `/tmp` is not required.
For a fixed path, both desktop and Android accept an explicit remote socket, for example:

```sh
waypane-desktop --ssh "$(whoami)@127.0.0.1:8022" \
  --remote-socket /data/data/com.termux/files/usr/tmp/waypane.sock -- foot
```

Host keys are verified against `~/.ssh/known_hosts` unless `--known_hosts` is supplied.
`--accept-new-host-key` saves unknown host keys to that file but never accepts a changed key.
On Android, the first connection to a host displays its algorithm and SHA-256 fingerprint;
choosing **Trust and save** records it in app-private `known_hosts`. A changed fingerprint
shows an explicit warning before **Replace saved key** is allowed. The Android command field
accepts only the remote application and its arguments; Waypipe flags are managed by Waypane.

A remote Weston message such as `could not load cursor 'dnd-none'` comes from the cursor theme
installed in the Termux/Linux environment, not from the Waypipe stream. Install or configure an
Xcursor theme on that host if cursor shapes are needed; it does not affect desktop frames.

### Android display controls

On Android, **Auto-resize remote windows** defaults to on so phone displays are used fully.
Disable it in Setup or **Menu** to preserve the compositor's fixed output and each client's own
window geometry. Without Auto-resize, the window is fitted entirely inside the available display
area without changing its aspect ratio; black bars are presentation-only and never become remote
input.

**Wayland display scale** defaults to **Auto**, computed as `ceil(DisplayMetrics.density / 2)` and
clamped to 1x–4x. On a 480dpi (`density = 3`) phone this selects 2x. Setup and **Menu** also offer
manual 1x–4x choices. Native scale drives `wl_output.scale` and
`wl_surface.preferred_buffer_scale`, changing how remote applications lay out and size text and
controls; pinch zoom remains a separate presentation transform. Selecting Auto in the window menu
recomputes from the current display metrics, and configuration changes re-synchronize while Auto
is active.

- By default, one finger emulates a pointer at the touched position: a quick tap sends a left
  click, moving beyond the touch slop starts a left-button drag at the original point, and a
  stationary long-press sends a right click. This works with traditional Wayland applications
  that do not listen for `wl_touch`.
- Enable **Direct touch input** in the window menu to send native `wl_touch` down, motion, and
  up events instead. Beginning a two-finger viewport gesture cancels the active touch or drag so
  the remote application is not left with a stuck contact or button.
- Ordinary touches never open the soft keyboard. Use the dedicated **Keyboard** button in the app
  bar or fullscreen overlay (or **Menu → Show keyboard**) when text input is needed; focus is
  requested only after the action's window/dialog has released it. Hardware keyboard and pointer
  input remain available.
- Add a second finger to pinch between 1x and 8x and drag to pan any enlarged axis. Beginning this
  gesture cancels an active remote touch instead of leaving a contact stuck down.
- At 1x fit, where there is nothing to pan, a two-finger drag is forwarded as smooth scrolling.
- A quick two-finger tap focuses at 2.5x around the fingers; another two-finger tap or **Fit**
  restores the complete window. Single-finger double-clicks remain ordinary remote clicks.
- **Menu** provides Direct touch, Wayland display scale, Show keyboard, Auto-resize, Fit,
  fullscreen, and closing the remote window. In fullscreen the system bars, app bar or side rail,
  and window tabs are hidden and a small translucent **Menu** remains at the top right; Back first
  leaves fullscreen and then asks for confirmation before disconnecting from the session.
- Portrait uses a top app bar and a fixed-height single-line status strip. The status strip never
  changes height when remote log lines arrive, avoiding a feedback loop where logging resized the
  view, triggered Auto-resize, and produced more log lines. Landscape places Back, Keyboard, Fit,
  Menu, and multi-window switching in a 72dp end-side rail; both the rail and the remote view stay
  clear of status bars, navigation bars, and display cutouts, and landscape status text is shown
  inside **Menu** instead of consuming vertical space.
- Opening the soft keyboard translates the remote `SurfaceView` upward in lockstep with
  `WindowInsets.ime` until its bottom edge rests above the keyboard; the container clips the top
  edge, and closing the keyboard moves it back. `Modifier.offset` only changes placement and never
  remeasures the view, so there are no GPU surface reallocations, stretched old frames, or
  Auto-resize reconfigurations. Moving only the remote view keeps the app bar and side rail
  reachable, and `RemoteSurfaceView` keeps its own Android View bounds so touch and pointer
  coordinates stay accurate.
- The screen stays awake while a session is open and the original screen-on/insets behavior is
  restored when the session closes or the UI is destroyed.

Enabling **Auto-resize remote windows** sends a configure request whose window-geometry logical
size is `ceil(available_view_pixels / effective_scale)`, so native scaling does not get canceled by
an oversized remote window. The available pixels come from `RemoteSurfaceView.onSizeChanged`
rather than rounded density-independent dimensions. Changing native scale while Auto-resize is
active schedules an updated request after the same 250ms debounce. Wayland clients may ignore or
adjust these requests; Waypane always uses the buffer size the client actually commits. Pinch zoom
and pan never trigger remote resize requests.

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
- GitHub Actions runs `44484669930`, `44513696621`, and `44524897577` passed formatting/Clippy,
  all upstream Waypipe integration tests, native Windows and macOS checks/tests/release builds,
  and complete Android APK builds with JDK 25, Gradle 9.9.1, AGP 9.5.1, Kotlin 2.4.21, SDK 37.2,
  and NDK 30.0.16248370. Run `44484669930` includes `wl_data_offer.accept`, `set_cursor` with the
  actual latest pointer-enter serial, and 16-byte evdev 42 keyboard payloads; run `44524897577`
  includes command and stderr reporting for a remote command that exits before the first stream.
- CI-produced APKs containing `lib/arm64-v8a/libwaypane_android.so` were installed on a physical
  Xiaomi Android 16 device with a Mali-G610 GPU. They negotiated RGBA8 with Vulkan, connected to
  Termux Waypipe 0.11.3 over an imported encrypted RSA key, and presented Weston frames across
  portrait, landscape, and immersive sizes. Device testing also confirmed readable `waypipe server
  -- <command>` errors with the command's own stderr, while `weston` with Auto-resize no longer
  reports `missing pool`, layer-allocation panics, or `UnknownRid` after retiring its old SHM
  pools. The native integer scaling feature documented above was added after run `44524897577`;
  its Kotlin APK build and on-screen Auto/1x/2x/3x behavior require a new CI and device run
  before being described as device-verified.
- The Windows desktop binary has not been launched on a physical Windows device, so its
  interactive GUI, input, clipboard, and lifecycle behavior remain runtime-unverified.

## Known limitations

- DMABUF/GPU passthrough and video codecs are not supported; remote applications use
  `wl_shm` (`--no-gpu`), with software rendering on the application host if necessary.
- Clipboard support currently targets text MIME types, not arbitrary file or image formats.
  Drag-and-drop (`wl_data_device.start_drag`, action negotiation, and `wl_data_offer.finish`) is
  not implemented; accepting those requests is only a no-op and must not be confused with working
  drag-and-drop.
- `wl_surface.set_opaque_region` is accepted but currently ignored; it is only an optimization hint
  and does not change input hit testing or visual correctness.
- Android two-finger scrolling currently emits continuous `wl_pointer.axis` wheel events without
  finger-axis origin, `axis_stop` inertia, or `axis_value120` steps. Native hardware-wheel events
  do include `axis_value120` when supported.
- Desktop keeps the native OS cursor instead of compositing remote `set_cursor` images; custom
  cursor textures are currently composited only on Android.
- `wp_fractional_scale_v1` and `wp_viewporter` are not advertised, so native scaling is currently
  integer-only; fractional ratios are handled by host presentation and Android pinch zoom. Other
  optional globals that Waypane does not advertise (for example pointer constraints, tablet,
  Xwayland shell, and DMABUF protocols) fall back on the client's own non-extension path and are
  not presented as implemented capabilities.
- Desktop input uses physical/evdev key forwarding; native desktop IME/preedit integration is
  not implemented yet. Unicode soft-keyboard integration is implemented on Android.
- SSH agent authentication, Xwayland session management, audio forwarding, and session
  reconnection are not implemented.

The prototype preceding this implementation is preserved in the `archive/prototype-v1`
branch.

## License

GPL-3.0-or-later for Waypane's own code. Vendored dependencies retain their own licenses.
