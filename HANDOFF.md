# GoLive — Frontier Handoff

You are taking over a nearly-ready Sala-first video rooms app
(native Rust core + Tauri shell + React frontend + Node rendezvous server,
Windows + macOS) to research open issues, fix details, polish QOL, and
resolve bugs. Work one thing at a time, verify everything.

Starting point: branch `fresh/native-core`. Everything below assumes it.

## 0. Read first (normative, in this order)

1. `AGENTS.md` — architecture, cross-platform law, commands, conventions.
   Treat it as binding; update it if you learn a new house rule.
2. `server/PROTOCOL.md` — Sala wire v1 (signaling contract, normative).
3. `BUGS.md` — open bugs with status. Rule: a bug that is 100% fixed AND
   verified is REMOVED from `BUGS.md`, never marked done-in-place.
4. `platform/README.md` — capture backend notes (incl. macOS TCC).

## 1. Architecture snapshot

- `app/` (`golive-app`): thin Tauri shell. Commands validate input, drive
  the core, return. Core events become Tauri events. NO product logic.
  Two bins: `golive-app` (owns main thread) and `golive-video` (one helper
  process per watched link: winit event loop + softbuffer CPU blit, RGBA
  over IPC, `0x01` ack per present).
- `app/src/video/`: shared GLV1 IPC (protocol, handshake, frame/ack,
  timeouts, feed loop) + per-OS transports selected ONLY here
  (`transport_unix.rs` filesystem socket / `transport_windows.rs` TCP
  loopback). Same framing both sides.
- `app/src/screen.rs`: enumerate → open/start → bridge pump (BGRA→I420).
  Delegates per OS to the platform crates.
- `app/src/pump.rs`: signal↔media orchestration (offer/answer, BILATERAL
  trickle, fences, watch routing). Orchestration only.
- `core/`: room/owner state machine, SignalClient, media (WebRTC H.264,
  OpenH264/VideoToolbox, quality profiles, PLI loss recovery). Knows NO
  backend and NO Tauri.
- `platform/`: `VideoSource` trait + shared types + converters. ZERO OS
  dependencies. `platform-macos/`: ScreenCaptureKit via pure objc2.
  `platform-windows/`: DXGI (displays) + WGC (windows), recently
  implemented — least battle-tested area.
- `server/server.mjs`: rendezvous signaling ONLY (routes envelopes, never
  media, never parses SDP). `app/web/`: React+TS+Vite, Tauri commands for
  intents, snapshots on demand, `signal-event`/`media-event` for live
  updates. `mock.ts` drives the pure-browser path — keep intact.

## 2. Current state (what works)

Lobby (create/join with server+nick fields), roster auto-sync on entry
(`get_roster` pull — Tauri events have no backlog), share/watch with
bilateral ICE trickle, macOS source previews (`preview_source`),
faithful goDrinking-style lobby UX, per-OS video IPC, QOL pass on the
share modal (select-then-confirm) and quality panel (sanitized numeric
inputs). Recent: PLI-triggered keyframe recovery on packet loss.

## 3. Open bugs (also in BUGS.md — start here)

- **BUG-001 slow motion after PLI** (Mac→Mac and Mac→Win), status open.
  Prime suspect: stuck `force_intra` flag (IDR every frame explodes
  bitrate/CPU) or PLI storm (debounce too loose). Confirm via
  `link_stats`: high bitrate + high `keyframes_decoded` during slowness.
- **BUG-002 Windows slow + ~40% of an RTX 3090 just watching**, status
  open (windows). Suspects: software decode path, present loop without
  vsync, per-frame texture upload.
- Pending live verifications: roster filled on entry, previews in the
  share modal, Mac→Win watch after the trickle fix, PLI effect on stalls.

## 4. QOL axes (suggested — research and prioritize yourself)

Lobby/room polish, empty/loading/error states, toast discipline,
first-run permission guidance (Screen Recording on macOS 26 needs the
PARENT process or the signed `.app`, plus restart after grant),
diagnostics visibility (link stats, encoder badge, generations), keyboard
shortcuts, settings persistence, Windows capture UX.

## 5. Hard rules (from AGENTS.md — do not break)

- Protocols are the law (`PROTOCOL.md`, GLV1). Behavior changes need
  tests. OS code only behind `VideoSource`; `core` never imports OS
  bindings. Capabilities disable UI with reasons, never silent failure.
- Event-driven UI: commands never poll (only snapshot pulls +
  `signal-event`/`media-event`; the video helper's frozen check is its
  own clock). Errors are redacted strings: passwords, tokens,
  SDP/candidates never reach logs or frontend.
- No TURN (`typ relay` rejected). Bilateral trickle mandatory (host→
  watcher AND viewer→target, plus `ice-complete` both ways).
- Commit style: short `type: subject`, lowercase, no trailing period.

## 6. Environment gotchas (learned the hard way)

- Web build FIRST (`npm run build` in `app/web/`): `beforeBuildCommand`
  is EMPTY, Tauri never builds the frontend; stale `dist/` = stale UI.
- Building OUTSIDE the Tauri CLI (plain `cargo`, xwin cross) REQUIRES
  `--features tauri/custom-protocol` — without it the exe points at
  `devUrl` instead of the bundled UI (proven by reading tauri 2.11.5).
- Dev needs `-- --bin golive-app` (two bins confuse `cargo run`).
- `cargo tauri dev` watches `app/` only — Rust changes in `core/` do
  NOT trigger rebuild; relaunch manually.
- Server binds loopback-ONLY by design (non-local refused at startup).
  For LAN/ZeroTier tests run a loopback forwarder on the host; test
  setup used ZeroTier (`10.144.221.7:18790` via forwarder). Never loosen
  the bind in code.
- Cross-compile Windows on macOS: `cargo xwin build --target
  x86_64-pc-windows-msvc` (plain `cargo` fails on C deps like `ring`).
  NSIS has NO portable mode — portable = plain exes (`--no-bundle`
  equivalent), shipping `golive-video.exe` next to `golive-app.exe`.
- Identity limits (server-enforced): nickname ASCII `[A-Za-z0-9 _.-]`
  2–24 chars (no accents — `"Você"` fails!); password 4–64 chars.
- Test matrix: `npm test` (web) + `cargo test` (workspace) + `npm test`
  in `server/`; cross-check Windows arms with
  `cargo xwin check --target x86_64-pc-windows-msvc`.

## 7. Definition of done (per item)

1. Failing behavior reproduced or root-caused in code (cite paths).
2. Minimal fix, no behavior change beyond the item, tests added/updated.
3. `cargo check` + `cargo test` + `npm run typecheck` + `npm test` green;
   no new warnings. Live-verify where the item demands it (native
   window, two instances, or the second peer over ZeroTier).
4. `BUGS.md` entry REMOVED once an item is 100% fixed and verified.
