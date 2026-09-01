# goDrinking — PRDs (2026-09-01)

Independent, testable requirements. Implement in order. Do not skip PRD-8.

## PRD-8 — Watch theater: fullscreen and video-only

**Problem:** Fullscreen does nothing. Video only paints a black window.

**Cause:** Tauri v2 denies `setFullscreen` without an explicit capability. Cinema mode collapses the absolutely positioned `<video>` because the flex parent has no height.

**Done when:**
- Fullscreen toggles the native window (not a black rectangle).
- Video only fills the window with the live stream visible.
- Esc exits video-only first, then fullscreen.
- Hover controls still work in both modes (zoom, fullscreen, exit video-only).
- Connection loss restores chrome and exits immersive modes.

## PRD-9 — Host app-audio exclude list

**Problem:** Host must keep selected apps (especially Discord) out of the shared mix. Viewer must still hear everything else.

**Rules:**
- System audio is opt-in (checkbox). Video works without it.
- Exclude list is a multi-select of running apps (name + all PIDs).
- Always exclude goDrinking’s own PID.
- Discord (and similar) has multiple processes: match by name contains **or** bundle id contains, case-insensitive; exclude **every** matching PID, including helpers with no windows.
- Do not use `getShareableContent` to build the list.
- If the tap fails, the video session still starts; surface a warning.

**Done when:** With Discord excluded, the viewer hears system/game audio and does **not** hear Discord.

## PRD-10 — Viewer audio + volume

**Done when:**
- If the host enabled system audio, Watch plays the Opus track (same `<video>` / MediaStream).
- Watch has a volume control (mute + slider 0–100).
- Volume does not affect host capture.
- No audio when the host left system audio off.

## PRD-11 — Transmission quality presets

Replace “only 1080p / 60fps” with three host presets:

| Preset | Capture cap | Target fps | Video bitrate |
|--------|-------------|------------|---------------|
| Low    | 1280×720    | 30         | ~1.5 Mbps     |
| Medium | 1920×1080   | 30         | ~4 Mbps       |
| High   | 1920×1080   | 60         | ~8 Mbps       |

Encoder still follows the **actual** pixel-buffer size (do not force 1920×1080). Preset is a cap + bitrate + fps.

**Done when:** Host UI shows Low / Medium / High, session starts with the matching cap, and Watch still plays.

## PRD-12 — Low latency

Keep glass-to-glass delay as low as practical on LAN, no public STUN.

**Host encode:** VideoToolbox realtime, no B-frames, Baseline, keyframe interval ≤ 1s, tight data-rate limit. Opus in VoIP/low-delay mode when audio is on.

**Watch:** `playsInline` + `autoPlay`; do not add extra JS buffering. Prefer currentTime catching up if `video.buffered` runs ahead by >250ms.

**Done when:** Motion on the host appears on Watch without a multi-second buffer. Quality High may use more bitrate; it must not add extra buffering on purpose.
