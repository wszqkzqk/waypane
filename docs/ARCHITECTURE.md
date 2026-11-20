# Waypane architecture

This document records the protocol and lifecycle invariants behind Waypane. For installation and
usage, see the [README](../README.md).

## Data path

```text
Wayland application
  -> stock waypipe server on the application host
  -> SSH streamlocal forwarding (russh)
  -> waypipe-client (frames, shared memory, files, pipes)
  -> waypane-core (Wayland compositor state)
  -> waypane-render / AndroidEngine (layer canvas, cursors, viewport transform)
  -> native host (winit windows on desktop, SurfaceView + wgpu on Android)
```

Each accepted `waypipe client` gets its own `SessionId`, `Connection`, Wayland object space, and
bounded I/O queues. Native windows are keyed by `(SessionId, WindowId)`, so identical object IDs in
two sessions cannot collide.

## Workspace layout

| Crate or app | Responsibility |
| --- | --- |
| `crates/waypipe-client` | Waypipe wire protocol versions 16/17, RID/fd mapping, SHM replication, pipes, virtual files, LZ4/Zstd, and copy-on-write frame snapshots |
| `crates/waypane-core` | UI-independent Wayland compositor state generated from the official `wayland.xml`, `xdg-shell.xml`, and `text-input-v3.xml` schemas |
| `crates/waypane-transport` | `russh` authentication, host-key verification, remote directory discovery, `waypipe server` startup, and `streamlocal` forwarding |
| `crates/waypane-render` | Damage-aware layer canvas, pixel-format conversion, viewport math, wgpu textures, and the optional winit desktop backend |
| `crates/waypane-android` | Session actor, `AndroidEngine`, JNI handle registry, Android key mapping, and `ANativeWindow` presentation |
| `apps/waypane-desktop` | Desktop CLI, multi-session routing, native windows, pointer/keyboard input, and system clipboard integration |
| `apps/android` | Kotlin/Jetpack Compose UI, key management, session ViewModel, `SessionService`, `RemoteSurfaceView`, touch, IME, and clipboard integration |
| `vendor/waypipe` | Unmodified upstream Waypipe 0.11.3 used as the protocol reference and integration-test peer |

## Wire protocol and resource ownership

`waypipe-client` handles `OpenFile`, `ExtendFile`, `BufferFill`, `BufferDiff`, `Close`, and header
negotiation for protocol versions 16/17. Legacy `AckNblocks` frames are ignored as upstream Rust
does; `Restart` is rejected. Compression can be `none`, `lz4`, or `zstd`, with independent limits on
frame size, allocation size, and decompression output.

Shared memory has two lifetimes that must not be confused:

- Waypipe transport files may receive asynchronous diffs even after the Wayland pool/buffer objects
  that referenced them have been destroyed. Mappings therefore follow transport lifetime, bounded to
  256 MiB per file and 1 GiB per connection.
- A committed `BufferView` owns a read-only `ShmData` snapshot. Later diffs and `ExtendFile` updates
  use `Arc` copy-on-write, so a frame waiting behind a resize cannot lose its RID when the remote
  client replaces a buffer or closes a pool. Unique allocations queued by one receive batch are
  bounded to 256 MiB.

`wl_shm_pool.resize` keeps the logical pool size separate from a potentially larger backing fd and
does not shrink live mappings, matching Waypipe C behavior. Pipes are released independently after
EOF/shutdown via `close_pipe`; `close_transport_file` is reserved for explicit transport removal.
Unknown RID errors include the frame type and a bounded 64-entry closed/not-open history so protocol
bugs remain diagnosable without unbounded logs.

Clipboard transfers run over bidirectional Waypipe pipes and are bounded to 16 MiB. Outbound virtual
files such as the `wl_keyboard.keymap` use the same reverse-file path.

## Wayland compositor invariants

Advertised interfaces at the moment are `wl_display`, `wl_registry`, `wl_compositor`,
`wl_subcompositor`, `wl_shm`, `wl_seat`, `wl_output`, `wl_data_device_manager`, `xdg_wm_base`,
`xdg_toplevel`, `xdg_popup`, `xdg_positioner`, and `zwp_text_input_manager_v3`. A host test
enumerates every non-destructor request in those generated interfaces and rejects missing or stale
routing policies.

### Surfaces, regions, and subsurfaces

- Surface state is double-buffered. Attachments support ARGB/XRGB formats, all eight buffer
  transforms, and damage-only upload regions.
- `Canvas` reallocates a layer whenever its committed dimensions change. Empty commits without
  attach, damage, callback, or z-order changes do not manufacture frames.
- `wl_region` operations use disjoint rectangle algebra bounded to 1024 rectangles and apply
  transactionally. `set_input_region` follows committed-copy semantics.
- Input hit testing is half-open over the root toplevel, its subsurfaces, and popups. Surfaces track
  output membership and emit `wl_surface.enter`/`leave` on mapping, unmapping, late output binding,
  cursor replacement, and output release.
- Subsurfaces support `place_above`/`place_below`, `set_sync`/`set_desync`, synchronized parent
  caches, and published z-order deltas.
- Destroying a window dismisses descendant popups and cleans up subsurfaces, touch contacts,
  pointer/keyboard focus, cursor mapping, and z-order state.

### XDG Shell

- Toplevels enforce min/max size constraints and interactive resize/fullscreen/maximize rules. An
  empty initial commit is allowed before the first `ack_configure`, while real attachments remain
  gated on it. Creating an `xdg_surface` after attaching a buffer is rejected.
- Positioner validation covers anchors, gravity, and Flip → Slide → Resize constraint precedence.
  `xdg_popup.reposition` responses are emitted as `repositioned`, `xdg_popup.configure`, then
  `xdg_surface.configure`. Popup geometry activates after its single-use configure acknowledgment.
- `xdg_popup.grab` requires the owning seat and a recent input serial. The topmost popup receives
  focus, and an outside press or Escape emits `popup_done`.

### Input, cursor, and clipboard

- Keyboard events carry unmodified evdev codes in the full 16-byte `wl_keyboard.key` payload.
  Duplicate transitions are suppressed, and focus changes clear pressed keys and modifiers.
- Pointer events track enter serials, motion, buttons, `axis`, and `axis_value120`. Touch supports
  down, motion, up, and cancel.
- `wl_pointer.set_cursor` is tied to each pointer object's focused surface and latest enter serial.
  NULL hides the cursor; role conflicts and stale serials are rejected; committed
  `wl_surface.offset` shifts the hotspot. Popups can composite into their parent surface before
  presentation.
- `wl_data_offer.accept` accepts NULL or an offered MIME type; `receive` requires an exact offered
  text MIME type. Implementing those requests does not imply drag-and-drop support.
- `text-input-v3` uses commit-count serials, surrounding text, Unicode preedit, and commit strings.
  Android falls back to ASCII key/delete events for applications that do not support it.

### Native scaling

`Compositor` exposes `wl_output.scale` and `wl_surface.preferred_buffer_scale` (v6), bounded to
1x–8x. Every window inherits the output scale unless it has a window-specific override. Popups and
subsurfaces inherit the nearest window override without affecting siblings. Runtime changes notify
existing surfaces and bound outputs, including an explicit scale of 1 when leaving HiDPI.

- **Android** starts with `ceil(DisplayMetrics.density / 2)` clamped to 1x–4x, with an explicit
  manual override. Auto follows configuration changes without reconnecting.
- **Desktop** computes each native window's scale independently as
  `ceil(Window::scale_factor())` when it is created and after `Resized`/`ScaleFactorChanged`. The
  window keeps its host logical size, so fractional system scales get crisp integer-resolution
  buffers without changing layout.

Android Auto-resize converts the physical view size to window-geometry logical units with
`ceil(px / effective_scale)`. Clients may adjust or ignore configure requests; rendering always
follows the buffer size the client actually commits.

## Presentation and Android lifecycle

`wl_buffer.release` and `wl_callback.done` are emitted only after the associated native frame has
been submitted for presentation, preserving backpressure for occluded windows.

`AndroidEngine` runs on a single-threaded Tokio actor. JNI calls resolve opaque IDs through a
synchronized registry into `Arc` snapshots, so an in-flight call stays valid while `nativeDestroy`
removes its public handle. GPU state is serialized by `Mutex<GpuRenderer>`:

- Rendering runs on a dedicated `RenderThread`; dirty waits use versioned condition variables and
  new frames align to VSync with `Choreographer`. Static content does not run a 60 Hz polling loop.
- Adapter selection, capability negotiation, surface configuration, and presentation all target the
  same `ANativeWindow`. `SurfacePlan` requires an RGBA/BGRA 8-bit format and `RENDER_ATTACHMENT`
  usage, and respects devices that only advertise the `Inherit` alpha mode.
- Resources drop in order: textures/blitter, wgpu surface, `ANativeWindow`, Java surface. `Outdated`
  and `Timeout` retry while dirty; occluded surfaces wait for visibility callbacks.
- If a window is already lost during `surfaceCreated` while an Activity is being replaced,
  attachment is retried on the subsequent `surfaceChanged` with a valid size.
- Each `AndroidSurface` holds its `Canvas`, `Viewport`, view size, touch/button state, scale, and
  queued frame IDs. Viewport and view-size updates mutate that state directly instead of consuming
  the actor's bounded command queue; the surface lock is held through presentation.

`ViewportTransform` fits the buffer without cropping, clears the `ANativeWindow` to black, and maps
input from view fractions back through `wl_surface.set_buffer_scale`. Touches outside the content
are ignored or cancel an active contact, never clamped into the desktop. Viewport-only redraws reuse
the current frame version so they do not manufacture Wayland frame callbacks. On Android, remote
cursor RGBA data is decoded once and rendered in a separate alpha-blended pass sharing the clipped
transform.

### Android UI geometry

Two UI invariants keep presentation from generating resize storms:

- The portrait status strip has a fixed single-line height. Remote stderr updates its text without
  changing the view height, so log messages cannot feed back into Auto-resize.
- Soft-keyboard insets drive `Modifier.offset` placement of `RemoteSurfaceView` rather than
  `imePadding` measurement changes. The view's bottom edge follows the keyboard top edge while its
  measured size stays constant, avoiding per-animation-frame GPU surface reallocations and remote
  configure requests. Android View local bounds remain unchanged, so input coordinates still match
  the visible translated view.

In landscape, the session hides the system bars and places controls in a fixed-width end rail. The
remote view pads only stable display-cutout insets, so transient system-bar visibility cannot alter
its geometry.

On Android, the application's `WaypaneViewModel` lives in `WaypaneApplication`'s `ViewModelStore`,
not in an Activity `ViewModelStore`. A `specialUse` foreground `SessionService` keeps the process
alive across Home/Recents switches, task swipes, and Activity recreation. Only an explicit confirmed
disconnect, the notification's Disconnect action, session failure, or process death destroys the
native handle. If Android recreated the process after a saved session, the recreated Activity reports
that the previous session was reclaimed rather than silently showing Setup.

## SSH transport and key handling

`waypane-transport` supports password and public-key authentication, including encrypted private
keys whose passphrases come from memory. RSA keys negotiate `rsa-sha2-512`/`rsa-sha2-256`, never
legacy SHA-1 `ssh-rsa`. Authentication errors report `user@host:port`, the signature algorithm,
public-key SHA-256 fingerprint, server-offered methods, and partial-success state.

Host keys are always verified against `known_hosts`. Desktop `--accept-new-host-key` persists only
unknown keys and still rejects changed keys. Android displays the algorithm and SHA-256 fingerprint
before saving a new key, and requires a separate explicit replacement confirmation for a changed
key.

Remote commands are tokenized as an application plus arguments, separately from Waypipe options;
Waypane invokes `env XDG_RUNTIME_DIR=<dir> waypipe --no-gpu --compress <mode> --socket <path>
server -- <command>`. A writable socket directory is selected
from `TMPDIR`, `XDG_RUNTIME_DIR`, and `/tmp` in that order, and exported as `XDG_RUNTIME_DIR` for
non-login/headless sessions. Sockets are unique and length-bounded; an explicit remote socket must
be an absolute path of at most 100 bytes. If the command exits before the first Waypipe stream is
accepted, the client reports the command, a human-readable exit code when available, and up to 4 KiB
of trailing remote stderr.

Android imports private keys atomically through the Storage Access Framework with a 64 KiB size
limit, sets owner-only permissions, and excludes keys and `known_hosts` from backups.

## Tests and upstream interoperability

Standard local checks:

```sh
cargo check --workspace --all-targets --features jni-check --locked
cargo test --workspace --locked
cargo test -p waypane-android --features jni-check --locked
cargo clippy --workspace --all-targets --features jni-check --locked -- -D warnings
```

The `jni-check` feature uses null `ANativeWindow` stubs on desktop hosts, so JNI and surface-format
logic runs without linking Android's `libandroid`.

With a host-native upstream Waypipe binary, the opt-in integration suites run against real
frames:

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

These cover SHM replication and damage, reverse keymap descriptors, two-way clipboard transfers,
Unicode preedit/commit serials, xdg-toplevel creation, native 2x-to-3x buffer scaling, cursor
lifecycle, and exact evdev key delivery. `upstream_ssh` starts an isolated `sshd` (requiring
`ssh-keygen` and `/run/sshd` on Ubuntu), exercises encrypted Ed25519 and RSA keys with the server
restricted to `ssh-ed25519,rsa-sha2-512,rsa-sha2-256`, interactive TOFU, Termux-style `TMPDIR`
discovery, and real Wayland frames over `streamlocal` forwarding.

`.github/workflows/build.yml` runs formatting and Clippy, macOS and Windows check/test/release
builds, all upstream integration suites on Ubuntu, and a complete Android APK build. Android SDK,
NDK, JDK, and Gradle installations are kept on disposable CI runners.
