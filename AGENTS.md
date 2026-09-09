# GoLive — Agent Guide

Sala-first video rooms: native Rust core + Tauri shell + React frontend +
Node rendezvous server. Read this before touching code. Protocols are law;
behavior changes need tests; platform code stays behind traits.

## 1. Architecture and crate roles

```text
app/                  golive-app: thin Tauri shell. Commands validate input,
                      drive the core, return. Core events become Tauri events.
                      NO product logic here. Two bins:
                      - golive-app   (src/main.rs: Tauri app, owns main thread)
                      - golive-video (src/bin/video.rs: one helper process per
                        watched link, owns its winit event loop + softbuffer
                        CPU blit; fed RGBA over IPC, acks 0x01 per present)
app/src/video/        shared GLV1 IPC module (protocol, handshake, frame/ack,
                      timeouts, feed loop) + per-OS transports selected ONLY
                      here: transport_unix.rs (filesystem socket) /
                      transport_windows.rs (TCP loopback). cfg(unix/windows)
                      lives at this selection point, never scattered.
app/src/screen.rs     glue the app owns: enumerate → open/start → bridge pump
                      (BGRA→I420) → core channel. Delegates per OS to the
                      platform crates below.
app/src/pump.rs       signal↔media orchestration (offer/answer + bilateral
                      trickle, fences, watch routing). Future core::runtime
                      candidate — keep it orchestration-only.
core/                 golive-core: room/owner state machine, SignalClient,
                      media (WebRTC, H.264 OpenH264/VideoToolbox, quality
                      profiles). Knows NO backend and NO Tauri.
platform/             golive-platform: VideoSource trait, shared types
                      (SourceInfo/Kind, BgraFrame, CaptureConfig,
                      CapturePacket, GpuPixelBuffer), converters, CapabilitySet,
                      MockSource. ZERO OS dependencies — builds everywhere.
platform-macos/       golive-platform-macos: ScreenCaptureKit backend via pure
                      objc2 (no Swift toolchain — undeployable here, see
                      platform/README.md). enumerate/open/start + thumbnail.
platform-windows/     golive-platform-windows: DXGI Desktop Duplication
                      (displays) + Windows.Graphics.Capture (windows).
                      Same VideoSource shape as macOS. Wired target-gated
                      in app/Cargo.toml.
server/               server.mjs: rendezvous signaling ONLY (routes envelopes,
                      never carries media, never parses SDP/candidates).
                      Contract: server/PROTOCOL.md (normative).
app/web/              React+TS+Vite frontend. Intents via Tauri commands;
                      snapshots on demand + signal-event/media-event. Builds
                      to web/dist = Tauri frontendDist (git-ignored).
```

## 2. Cross-platform requirement (Windows + macOS)

- ALL OS code lives in its platform crate behind `golive_platform::VideoSource`
  (`enumerate` / `open` / `start`). `golive-core` never imports OS bindings;
  `app` only delegates (`screen.rs` per-OS arms).
- Shared shapes live in `platform::types` (`SourceInfo`, `BgraFrame`,
  `CaptureConfig`, `CapturePacket`). UI needs no changes for a new backend.
- Capabilities are compile-time facts with honest reasons
  (`platform/src/capability.rs`): unsupported disables UI options with the
  explanation — never silent failure, never empty lists on a real desktop.
- Protocols are the law: `server/PROTOCOL.md` (Sala wire v1) and the GLV1
  helper IPC (`GLV1` + u32 w/h/title_len + title, then u32 len + RGBA per
  frame, `0x01` ack; feeder BINDS, helper CONNECTS; EOF = clean shutdown).
  Feeder + helper ship together — any wire change versions BOTH sides.
- Denial is typed (`PlatformError::PermissionDenied` + Settings copy), never
  silence. Titles/pixels/tokens/SDP never reach logs.

## 3. Windows screen capture

Implemented in `platform-windows/`: DXGI outputs for displays, WGC
(`CreateForWindow`) for windows, one-shot `thumbnail()`, typed denial.
Keep the adapter small: frames out, errors typed. No RTP/WebRTC/lifecycle
in the backend; pacing/encode belong to the core, lifecycle to the app.

## 4. Commands per OS

All Cargo commands run from `app/` (that is where `Cargo.toml` + `tauri.conf.json`
live); web commands from `app/web/`; server commands from `server/`.

| What        | Command |
|-------------|---------|
| Web dev     | `npm run dev` in `app/web/` (Vite `:1420`, strict — must match `devUrl`) |
| Desktop dev | `cargo tauri dev -- --bin golive-app` in `app/` (picks the app bin; helpers spawn automatically) |
| Web build   | `npm run build` in `app/web/` — **ALWAYS FIRST**: `tsc --noEmit && vite build` → `web/dist`. `beforeBuildCommand` is EMPTY, so Tauri never builds the frontend; a stale `dist/` means a stale UI. |
| Tauri build | `cargo tauri build` in `app/` (after the web build; the `golive-video` helper must sit next to the binary — `scripts/e2e-packaged.sh` copies it into `Contents/MacOS` for the packaged app) |
| Windows native | on a Windows machine: `cargo tauri build` (same order: web build first) |
| Windows cross (macOS host) | `cargo xwin build --target x86_64-pc-windows-msvc` in `app/` (needs `cargo-xwin`; plain `cargo` cannot compile the C deps for msvc) |

`tauri/custom-protocol` flag: `tauri`'s default features do NOT include
`custom-protocol` (verified against the registry manifest), and `app/Cargo.toml`
adds no features. Building OUTSIDE the Tauri CLI (plain `cargo build`, xwin
cross) therefore REQUIRES `--features tauri/custom-protocol` — without it the
packaged exe points at `devUrl` instead of the bundled `frontendDist`.

## 5. Tests

- `npm test` in `app/web/` — vitest (`api.test.ts`, `e2e.test.ts`; Tauri APIs mocked).
- `cargo test` in `app/` — lib unit tests (video math/protocol, bridge pump,
  trickle envelopes; all must pass unaltered) + `tests/smoke.rs` (shell commands
  against a real local `server/`, needs `node`). The feeder→helper test needs a
  window server and the built helper; it skips honestly without one.
- `npm test` in `server/` — `node test_*.mjs` (rooms, auth, admit, signaling,
  limits, kick, succession, heartbeat, ratelimit, nomedia).
- Packaged end-to-end: `scripts/e2e-packaged.sh` (server + host + viewer, asserts
  presented frames where windows exist).

## 6. Server

- Run: `node server.mjs` in `server/` (`PORT` default `18790`, `PORT=0` for an
  ephemeral test port; `BIND` default `127.0.0.1`).
- `BIND` is loopback-ONLY by design: non-local binds are refused at startup.
  To reach the room over the LAN, run a loopback forwarder on the host or put
  both peers on the same ZeroTier network — never loosen the bind in code.
- Signaling-only: JSON/UTF-8, 64 KiB caps, scrypt password checks with
  timing-uniform denied responses. Never logs passwords, tokens, SDP, or candidates.

## 7. Conventions (do not break)

- Event-driven UI: commands never poll. Snapshots are explicit pulls
  (`get_snapshot`, `get_roster`, `get_media_counters`); live updates arrive via
  `signal-event` / `media-event`. The one exception is the video helper's frozen
  check, which is its own local clock — not UI polling.
- Errors are redacted `String`s. Passwords, tokens, SDP/candidates never reach
  logs or the frontend (envelopes carry kinds/counts only; session_log bans the
  same keys — its tests enforce this).
- Browser mock preserved: `app/web/src/mock.ts` drives the pure-browser path
  (active when Tauri globals are absent). Keep it intact — change only on explicit
  order. Same formats as `api.ts`.
- Screen capture permission (macOS): first OS contact happens at
  `enumerate()`/`start()` from explicit user gestures only. Denial codes `-3810`
  (SCK user-declined) and `-3801` (TCC on macOS 26) map to `PermissionDenied`;
  needs System Settings → Privacy → Screen Recording (new bundle id = new prompt).
- Identity limits (server-enforced, `server/server.mjs`): nickname ASCII
  `[A-Za-z0-9 _.-]`, 2–24 chars; password 4–64 chars.
- `get_roster` is the roster pull (same `RoomMember` shape as the roster event);
  call it to sync state on entry, never to poll.
- Bilateral trickle ICE is mandatory: host candidates forward to the adopted
  watcher AND viewer candidates forward to the watch target (`pump.rs`), plus
  `ice-complete` both ways per `PROTOCOL.md`. No TURN (`typ relay` rejected).

## 8. Bugs (BUGS.md)

- Known bugs live in `BUGS.md` (repo root), each with a status — review
  them before touching related code.
- A bug that is 100% fixed AND verified is REMOVED from `BUGS.md`.
  Never mark done-in-place; the list holds open bugs only.

## 9. Commit style

Short `type: subject` headers, lowercase, no trailing period. Usual types:
`feat:`, `fix:`, `refactor:`, `release:`. `sala:` prefix for Sala-lane work,
`fix(ci):` for CI. Examples from history:

```text
feat: forward host ICE candidates to viewer (fix stuck negotiating)
refactor: simplify stage UI, restrict inputs to numeric values, ...
sala: quality engine + set_quality camelCase fix + e2e quality gate + ...
```
