# goDrinking — Handoff for another LLM

Read this entire file before editing. The product is a **LAN P2P screen-share app** (Discord screen share, but without Discord, without a cloud server). The current macOS build **freezes when the user clicks Start native session** and never shows the system screen picker.

Date of this handoff: 2026-09-01  
Repo: `/Users/jouydurao/projetos/personal/GoLive`  
Bundle ID: `com.cosmicsnow.godrinking`  
Tauri 2 + React 19 + Rust. macOS is the only implemented capture path.

---

## 1. What the app is

**goDrinking** is a desktop app for sharing a screen or window with someone on the same local network, with a short session code instead of Discord.

It is **not** a cloud product. No STUN/TURN is configured. Signaling is LAN-only (UDP discovery + TCP offer/answer). Capture, encode, and send are native (Rust), not browser `getDisplayMedia`.

### Goal (product)

1. Host chooses how to share (whole screen or a window).
2. Host starts a session.
3. Host gets a **6-character code**.
4. Viewer opens the same app, **Watch**, types the code.
5. Viewer sees (and optionally hears) the host, P2P, on the LAN.
6. On macOS 14.2+, host can share **system audio** and **exclude apps** (e.g. Discord) from that mix via Core Audio process taps — not via ScreenCaptureKit audio.

### What “done” looks like

- Host clicks Start → macOS content picker appears (not a freeze, not the lock TCC dialog on every launch).
- Host picks a display/window → preview canvas updates with RGB thumbnails.
- A session code is shown and copyable.
- A second machine on the same LAN joins with that code and plays H.264 (and Opus if audio is on) in a `<video>` via **browser WebRTC** (WKWebView), while the host sends from **webrtc-rs**.
- Stop releases ScreenCaptureKit, encoder, peer, LAN room, and any audio tap.
- Granting Screen Recording once (for this Apple Development-signed `.app`) persists across rebuilds.

---

## 2. How it is supposed to work (happy path)

```
Host UI  --invoke-->  MediaEngine (Rust)
                         |  create_media_session
                         |-- NativePipeline (preview + VideoToolbox H.264)
                         |-- PeerTransport (webrtc-rs, local ICE only)
                         |-- ScreenCaptureKit (SCContentSharingPicker → SCStream)
                         |-- optional ProcessTap (system audio minus excluded apps → Opus)
                         |-- LanRoom (UDP 17424 discovery + TCP offer/answer)
                         |
Host UI  --invoke-->  create_media_peer_offer
                         |-- publishes SDP offer into LanRoom

Viewer UI  --invoke-->  discover_media_room(code)
                         |-- UDP FIND → TCP GET_OFFER
Viewer UI  --JS RTCPeerConnection--> setRemoteDescription(offer)
                         |-- createAnswer, gather ICE
Viewer UI  --invoke-->  submit_media_room_answer({ host, answer })
Host engine polls LanRoom.take_answer() on snapshot() and set_remote_description
Viewer <video> plays remote track
```

### Roles

| Role | Capture | WebRTC | UI |
|------|---------|--------|----|
| Host | Native SCK + VT | webrtc-rs **sender** (`TrackLocalStaticSample`) | Share screen |
| Viewer | None | **WKWebView `RTCPeerConnection` receiver** | Watch |

Do **not** use native `accept_media_peer_offer` for the viewer. There is no native H.264 decoder/renderer. Viewer must be browser WebRTC.

---

## 3. How to run (macOS)

Do **not** use `npm run tauri dev` for capture. That binary is not a stable `.app`; TCC will not stick.

```bash
# From repo root
npm run macos:app
```

This runs `src-tauri/build-debug-app.sh`:

1. `tauri build --debug --bundles app --no-sign`
2. `codesign` with the first `Apple Development` or `Developer ID Application` identity
3. `open "src-tauri/target/debug/bundle/macos/goDrinking.app"`

Current signing identity on this machine: `Apple Development: Suguimoto Edwilson (M4HA2WUX34)`.

Reset TCC (only if the user denied the lock dialog):

```bash
killall "goDrinking" 2>/dev/null
tccutil reset ScreenCapture com.cosmicsnow.godrinking
tccutil reset All com.cosmicsnow.godrinking
```

Tests:

```bash
cd src-tauri && cargo test --offline --lib
```

Frontend typecheck:

```bash
npx tsc --noEmit
```

---

## 4. Repository map

```
src/App.tsx                          Host/Watch UI, invoke commands, viewer RTCPeerConnection
src/App.css                          Existing visual design — keep it
src-tauri/src/lib.rs                 Tauri commands
src-tauri/src/media/engine.rs        Session lifecycle, worker thread, snapshot
src-tauri/src/media/pipeline.rs      Preview worker + encoder worker
src-tauri/src/media/screen_capture_kit.rs   SCK actor, picker, stream, TCC probes
src-tauri/src/media/video_toolbox.rs        FFI to Swift VT encoder
src-tauri/native/video_toolbox_encoder.swift
src-tauri/src/media/access_unit.rs   AVCC → Annex-B, Baseline profile check
src-tauri/src/media/peer_transport.rs webrtc-rs sender + optional Opus
src-tauri/src/media/process_tap.rs   CATapDescription global tap minus apps
src-tauri/src/media/room.rs          LAN UDP/TCP signaling
src-tauri/src/media/types.rs         IPC DTOs
src-tauri/src/media/capabilities.rs  Platform capability report
src-tauri/Info.plist                 NSScreenCaptureUsageDescription, ATS local networking
src-tauri/Entitlements.plist         audio-input only (not sandboxed)
src-tauri/tauri.conf.json            identifier, infoPlist, signingIdentity "-"
src-tauri/build-debug-app.sh         debug .app + codesign
```

Tauri commands (all in `lib.rs`):

- `get_media_capabilities`
- `request_media_screen_recording_permission`
- `get_media_capture_sources` (currently returns **empty** on purpose)
- `get_media_running_apps`
- `create_media_session` (**sync**, blocks — see P0)
- `stop_media_session`
- `get_media_session_state`
- `get_media_preview`
- `create_media_peer_offer` / `accept_media_peer_offer` / `set_media_peer_answer` / `close_media_peer_transport`
- `discover_media_room` / `submit_media_room_answer`

---

## 5. P0 — App freezes on Start (current user-facing bug)

### Symptom

Click **Start native session**. UI freezes. System screen picker never appears. No lock dialog. Force-quit required.

### Root cause (almost certain)

`create_media_session` is a **synchronous** Tauri command. Tauri 2 runs sync commands on the **main thread**.

Call chain:

1. Main thread: `create_media_session` → `MediaEngine::create_session` → `sync_channel` wait on media worker.
2. Media worker: `adapter.start_capture` → `pick_filter_with_system_picker`.
3. Picker does `DispatchQueue::main().exec_sync { picker.presentPickerUsingContentStyle(...) }`.
4. Main is blocked in (1), so `exec_sync` never runs → **deadlock**.
5. Even if you switch to `exec_async`, picker observer callbacks also need the main run loop. If main stays blocked on `recv()`, the picker UI never processes events.

Location: `src-tauri/src/media/screen_capture_kit.rs` (`pick_filter_with_system_picker`, ~1098) and `src-tauri/src/lib.rs` (`create_media_session`).

### Required fix

1. Make capture start **non-blocking on main**:
   - `#[tauri::command]` **async** for `create_media_session`, **or**
   - return immediately after spawning, poll `get_media_session_state` until running/failed.
2. Present the picker with `DispatchQueue::main().exec_async` (never `exec_sync` from a thread that can be waited on by main).
3. Keep `SCContentSharingPicker` observer alive until selected/cancelled/failed.
4. Do **not** call `SCShareableContent.getShareableContentWithCompletionHandler` or `CGRequestScreenCaptureAccess` on launch, focus, source list, or start. Those APIs show the lock dialog (“deseja gravar a tela e o áudio”) even when Screen Recording is already enabled. That was a long user-pain loop. Source list is empty by design; the **system picker** is the source selector.

### Acceptance

- Start does not freeze the window.
- Within ~1s the macOS content picker is visible.
- Cancel picker → error `screen picker was cancelled`, UI recovers.
- Confirm picker → preview frames (`preview_frame_count` > 0) and session `state === "running"`.

---

## 6. Product requirements (PRDs)

Each PRD is independently testable. Implement in order. Do not skip P0.

### PRD-0 — Process and packaging

- macOS capture testing **only** via `npm run macos:app` (signed `.app`).
- Keep bundle id `com.cosmicsnow.godrinking`.
- Sign with Apple Development / Developer ID when present (already in `build-debug-app.sh`).
- `Info.plist` must keep `NSScreenCaptureUsageDescription`.
- Never commit secrets.

### PRD-1 — Screen Recording consent (no nag loop)

- Launch/focus must **not** show the lock TCC dialog.
- Permission probe: `CGPreflightScreenCaptureAccess` **or** non-empty foreign window titles via `CGWindowListCopyWindowInfo` (already in `screen_recording_is_granted`).
- If not granted: open System Settings Screen Recording pane; tell user to enable goDrinking, Cmd+Q, reopen.
- Start uses **only** `SCContentSharingPicker` for content selection.
- After one grant on the Apple Development-signed app, rebuilds should not require re-grant (Team ID stable). Ad-hoc (`codesign -s -`) **must not** be used for TCC testing.

### PRD-2 — Host capture + preview

- Start → picker (display or window style from UI toggle).
- `SCStream` BGRA frames.
- Bounded preview: 160×90 RGB8 thumbnail over IPC (`get_media_preview`). Never send native pixel buffers over Tauri IPC.
- Encoder is created from the **first real CVPixelBuffer size**, not forced 1920×1080 (display aspect is often 16:10). See `pipeline.rs` encoder worker.
- VideoToolbox H.264 Baseline. Accept constrained baseline SPS (`42c02a` etc.), not only `42e02a`. Do not fail the whole session on one encode error.
- Stop must stop the stream, join workers, and leave Screen Recording indicator off.

### PRD-3 — Session code + LAN join

- On session start, `LanRoom` binds TCP `0.0.0.0:0`, UDP discovery port **17424**.
- Code: 6 uppercase alphanumeric chars.
- Snapshot fields: `session_code`, `lan_addresses`, `lan_port`.
- Host auto-creates offer after capture is running and publishes it to the room.
- Viewer: `discover_media_room` → browser `RTCPeerConnection({ iceServers: [] })` → `submit_media_room_answer`.
- Host applies answer without holding the engine mutex during the 8s signaling timeout (`apply_room_answer` pattern already exists).
- ICE: empty `ice_servers` (host candidates only). Do not add public STUN unless the user asks.

### PRD-4 — Viewer playback

- Watch mode in `App.tsx`: `<video autoPlay playsInline>`.
- Interop: webrtc-rs H.264 `profile-level-id=42e02a`, packetization-mode=1, PLI/FIR/REMB.
- Viewer must work in a **second** goDrinking process on the same Mac or another LAN Mac.
- If no second machine: document a two-instance test (two `.app` copies is messy; prefer two user accounts or another Mac).

### PRD-5 — System audio exclude list (macOS 14.2+)

- Default **off** (`systemAudio` false in UI). Video must work without audio.
- When on: `CATapDescription initStereoGlobalTapButExcludeProcesses` + aggregate device + IOProc → 48 kHz stereo Opus → extra WebRTC audio track.
- Exclude list from window-list app names/PIDs (not `getShareableContent`).
- Always exclude goDrinking’s own PID.
- Discord has multiple processes; match by name contains / bundle id; exclude all matching PIDs.
- If tap fails, **video session still starts**; surface a warning. Do not fail create.
- Process taps can trigger extra TCC; never start a tap unless the checkbox is on.

### PRD-6 — UI

- Keep current visual language (`App.css`).
- Share vs Watch in the sidebar.
- Start must stay in “Starting…” until picker completes or errors; never freeze the whole app.
- Show session code after offer is ready.
- Empty source dropdown is OK; copy should say the macOS picker will appear on Start.
- Show `preview_error` / `detail` / `peer_detail` in the hint line.

### PRD-7 — Quality bar

- `cargo test --lib` stays green.
- No `getShareableContent` / `CGRequestScreenCaptureAccess` on startup.
- No main-thread deadlock.
- Cmd+Q / Stop does not leave a Screen Recording orange/purple indicator.

---

## 7. Architecture notes (do not regress)

### Threads

- `godrinking-media-control` — serial create/stop.
- `godrinking-screencapturekit-actor` — owns `SCStream`.
- `godrinking-media-preview` / `godrinking-media-encoder`.
- `godrinking-webrtc-peer` — tokio runtime.
- SCK sample callback on a DispatchQueue.

Main thread is for AppKit/TCC/picker only. Never `recv()` on main waiting for those.

### Why getShareableContent was banned

On current macOS (“Gravação do Áudio do Sistema e da Tela”), `getShareableContentWithCompletionHandler` and `CGRequestScreenCaptureAccess` show a lock dialog *even if* the app is already enabled in Settings. Users granted permission, clicked the dialog again, denied by accident, got `SCStreamErrorDomain:-3801` (“O usuário recusou os TCCs…”). Window-title preflight can still look “granted”. Treat SCK picker as the only interactive capture entry point.

### Encoder vs capture size

`stream_configuration` fits inside 1080p/720p preserving aspect (`fitted_even_size`). Creating VT at 1920×1080 then encoding a 1662×1080 buffer fails and used to flip UI back to Start while macOS still showed capturing. Encoder must follow the pixel buffer.

### Viewer vs host WebRTC

Host = webrtc-rs send. Viewer = JS RTCPeerConnection receive. `accept_media_peer_offer` is the native answerer path and has **no renderer**.

---

## 8. How to revise (review protocol)

Work in this order. After each PRD, write what you ran and the evidence.

1. **Unfreeze Start** (PRD-1/2 + P0). Evidence: picker visible, no deadlock, cancel recovers.
2. **Preview frames**. Evidence: `get_media_session_state` shows `preview_frame_count > 0` and canvas updates.
3. **Offer + code**. Evidence: 6-char code in UI after start.
4. **Two-peer video**. Evidence: Watch plays host screen.
5. **Audio exclude** only after video is solid.
6. Run `cargo test --lib` and `npx tsc --noEmit`.

### Definition of done for a revision

- [ ] Start never freezes the WebView.
- [ ] Picker appears without the lock TCC dialog.
- [ ] Preview works at non-1080p display sizes.
- [ ] Session code works on LAN.
- [ ] Viewer JS WebRTC plays video.
- [ ] Stop is clean.
- [ ] Tests pass.
- [ ] No new `getShareableContent` on hot paths.

### What not to do

- Do not “fix” TCC by calling `getShareableContent` more often.
- Do not use `tauri dev` as the capture test vehicle.
- Do not add public STUN/TURN unless requested.
- Do not send raw frames over IPC.
- Do not fail the session because SPS is `42c02a` instead of `42e02a`.
- Do not implement a native viewer decoder unless JS WebRTC is proven insufficient.
- Do not `exec_sync` onto main from a worker that main is waiting on.

---

## 9. Suggested first patch (for the next LLM)

Minimal unfreeze:

1. Change `create_media_session` to `async` and `spawn_blocking` the engine call **or** restructure so main never waits on the picker.
2. In `pick_filter_with_system_picker`, replace `exec_sync` with `exec_async`.
3. Ensure the picker observer `Retained` lives until the channel fires.
4. Frontend: keep `sessionAction === "starting"` until invoke returns; it already does — the freeze is native.

Then verify with `npm run macos:app`.

---

## 10. Open risks

- `SCContentSharingPicker` requires macOS 14+. Fine for this user.
- Process tap + Opus compile depends on `opus` crate / libopus on the machine; audio is optional.
- UDP 17424 bind fails if two hosts; second host still has TCP + shown IP:port.
- WKWebView H.264 + webrtc-rs interop is unproven in this repo; treat as PRD-4 work, not assumed done.
- `create_media_peer_offer` ICE gather 5s + 8s request timeout can make Start feel slow once capture works; split “capturing” vs “code ready” in the UI.
- Windows: never ship an exe built with raw `cargo build` — without the CLI's `custom-protocol` feature the binary boots in dev-mode and loads `http://localhost:1420` (ERR_CONNECTION_REFUSED with no dev server; proven 2026-09-03 via WebView2 UA trap). Ship only via `tauri build` (e.g. `npm exec tauri build -- --no-bundle --ci` when NSIS/WiX are missing); the portable is a copy of `src-tauri/target/release/godrinking.exe`.
- Windows black viewer with working host preview + connected ICE + zero inbound stats (seen 2026-09-03, RTX 3070, Stunar same-PC): the MF hardware MFT can "succeed" with a session-incompatible SPS profile (or no IDR), and the transport then drops 100% of samples silently. Since 2026-09-03 `MfH264Encoder::new` validates decodability (SPS profile + IDR) in the self-test and falls back to OpenH264, logging `mf encoder` lines to the session file. Instant workaround on any build: Encoder = Software.
- ROOT CAUSE of the Windows black screen (found 2026-09-03): `AccessUnitQueue` had a `Drop` impl calling `close()`, and the queue is `Clone`-shared. `create_windows_encoder` cloned the session queue into the encoder and the dropped parameter closed the shared state, so every `try_push` returned `Closed` forever — zero frames to any viewer on Windows only (mac moves the queue, no clone). Fix: no `Drop` on the queue (explicit `close()` + shutdown flags only). Session logs now also record `encoder:` backend lines and `pump:` first-unit/keyframe/write lines, and the Host status popup shows `Session detail`.
- Win→Mac over internet Stunar dies at ICE `checking` while Mac→Win and same-PC work (proven 2026-09-04): the Windows side is behind SYMMETRIC NAT (two STUN bindings from adjacent local ports mapped 186.205.17.51:6830 vs :6831), so the srflx candidate in the offer is useless inbound. Firewall ruled out (rules cover all profiles). Internet keying REQUIRES a TURN relay (not implemented); same-network is unaffected.

---

## 11. User language and constraints

The user communicates in Portuguese. UI copy can stay English (current design) unless asked. Do not lecture about permissions. Prefer fixing the deadlock over more TCC dialogs.

The user has already enabled **goDrinking** under System Settings → Privacy & Security → Screen & System Audio Recording. Do not tell them to toggle that again unless you have evidence the running binary is a different code signature than the one in the list.
