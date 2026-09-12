# golive-platform — capture abstraction

Structural requirement: the core never imports OS bindings. Capture is a
replaceable backend behind a pure trait; Windows (or any new API) plugs in
without touching transport, signaling, or lifecycle.

## Module map

```text
platform/                  golive-platform (pure, zero OS deps)
  src/lib.rs               re-exports + module map
  src/error.rs             PlatformError (typed; user copy lives here)
  src/types.rs             SourceKind/Info, BgraFrame, PlanarYuv,
                           PixelFormat, CaptureConfig
  src/traits.rs            VideoSource trait, FrameStream, NextError
  src/convert.rs           bgra_to_i420 (BT.709 limited) + vectors
  src/capability.rs        CapabilitySet (compile-time facts + reasons)
  src/mock.rs              MockSource (no OS; shared lifecycle driver)
  src/windows.rs           #[cfg(windows)] stub — see below

platform-macos/            golive-platform-macos (macOS only)
  src/lib.rs               ScSource: enumerate/open/start over
                           ScreenCaptureKit via pure objc2 bindings
                           (no Swift toolchain — undeployable here, see below)

core (media::VideoSource::External)
  consumes std-channel I420Frame only. Knows no backend.

app (screen.rs)
  owns the glue: enumerate → open/start → bridge thread
  (BGRA→I420) → core channel. Teardown on stop_share/leave.
```

## Where Windows plugs in

1. **File**: `platform/src/windows.rs` (already `#[cfg(target_os = "windows")]`,
   compiles — verified with `cargo check --target x86_64-pc-windows-msvc`).
2. **Trait methods**: `VideoSource::enumerate/open/start` + `FrameStream::stop`
   semantics (deadline-bounded join; indicators released). Reuse
   `mock::drive_lifecycle` in tests to lock the semantics.
3. **Expected dependencies**: `windows` crate (`Windows::Graphics::Capture`
   for WGC frame acquisition, `Windows::Win32::Graphics::{Dxgi,Direct3D11}`
   for the device), reusing `BgraFrame` + `bgra_to_i420` unchanged.
   Permission model differs (graphics-capture picker, no TCC equivalent) —
   surface it as `PermissionDenied` with its own hint.
4. **App side**: `app/src/screen.rs::open_stream` gains a
   `#[cfg(target_os = "windows")]` arm constructing the backend; UI needs no
   changes (same `SourceInfo` shapes).

## Rules

- Adapters stay small: frames out, errors typed. **No RTP/WebRTC/lifecycle
  inside backends** — the core owns pacing/encode, the app owns lifecycle.
- Titles never enter logs (ids + counts only); payloads through
  `PlatformError::Display` are UI-safe by construction.
- Denial is typed (`PermissionDenied` + Settings copy), never silence and
  never an empty list on a real desktop.
- Stale beats queued: bounded latest-only channels everywhere; a frozen
  stream repeats its last frame instead of timing out into `SourceGone`.
- New bundle id ⇒ new TCC prompt. First OS contact happens at
  `enumerate()`/`start()` (UI calls both from explicit gestures only).

## Why objc2, not a Swift bridge

Evaluated `screencapturekit-rs` (safe Swift bindings) and rejected: its
build compiles Swift bridge objects needing `libswiftCompatibility*`
static archives plus a Swift-concurrency dylib that exists nowhere on
CLT-only machines — the packaged app would abort at launch (`dyld`).
`objc2` links plain system frameworks present on every Mac; builds are
pure `rustc`, faster, and debuggable without a Swift toolchain.
