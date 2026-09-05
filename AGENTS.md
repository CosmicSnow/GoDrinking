# goDrinking engineering guide

`GoLive` is the repository directory; the product is **goDrinking**. Read `CONTEXT.md` before changing product language or behavior. Its domain terms are normative.

## Product boundary

- goDrinking is P2P WebRTC screen sharing. Media never transits the Rendezvous.
- Join modes are **LAN**, **Direct**, and **Stunar**. Preserve their meanings from `CONTEXT.md`.
- Stunar is signaling-only and best-effort P2P. There is no TURN in this version, so symmetric-NAT failure is an expected, diagnosable limitation.
- This reliability program supports packaged goDrinking Viewers only: WKWebView on macOS and WebView2 on Windows. External-browser compatibility is out of scope.
- Broadcast is the migration contract for all join modes. Sala parity for LAN/Direct is deferred; do not accidentally imply it exists.

## Media compatibility contract

- Product codec: **H.264 Constrained Baseline**, packetization mode 1, with an explicit SDP `profile-level-id` and exact emitted-bitstream validation.
- One codec does not mean one encoder. Keep VideoToolbox, Media Foundation, and OpenH264 fallback only when each emits the same compatible H.264 contract.
- Do not add or re-enable H.264 High, HEVC, AV1, or codec selection without an approved cross-platform compatibility plan.
- The initial supported envelope ends at 60 fps. Validate permitted resolution/fps/profile/level combinations; reject unsupported combinations before a Session starts.
- Preserve arbitrary aspect ratios. Dimensions delivered to encoders must be even and conform to the backend’s documented alignment requirements; never assume 16:9.
- A live capture source/resolution/scale change restarts the local Share slot. The surrounding Session stays open. Do not attempt in-place encoder reconfiguration until it has dedicated design, tests, and cross-platform evidence.
- Treat color as an API contract: record and validate pixel format, matrix, range, and transfer function. The canonical screen-media path is NV12 BT.709 limited range; the Host preview must not be used as proof of Viewer color correctness.

## Architecture and lifecycle rules

- Keep platform capture and encoder adapters separate from signaling and WebRTC sender logic. The primary seam is normalized `EncodedAccessUnit` / encoded audio packets.
- Use a serialized owner for mutable Session, Share slot, and link state. A snapshot is observational: it must never advance signaling or lifecycle work.
- Fence asynchronous work by Session epoch, Share epoch, and link ID. Discard stale completions after Stop/restart.
- Start transactionally: validate config/capabilities, acquire resources, then publish ready state. On any failure, roll back capture, audio, join service, Rendezvous state, and peers deterministically.
- A stopped Share slot owns no capture or system-audio resources.
- Remove peer handles under synchronization, but stop/join peer workers outside global state locks.
- One Viewer failure must not stop other Viewers or the Host capture. Queue overflow/recovery must force a new keyframe for the affected link.

## Diagnosing reliability

Every Session/Share/link must emit correlated, redacted milestones with IDs and platform/backend/codec/dimensions metadata. Never log Passwords, Tokens, or complete SDP.

Minimum milestones: join service ready; admission; offer/answer; ICE candidate and selected-pair state; peer connected; first capture frame; first encoder input/output; first valid IDR with parameters; first RTP sent; and Viewer packet, decode, and presentation milestones.

Classify failures from evidence rather than “connection failed”:

- no selected candidate pair → signaling/ICE/network;
- capture but no encoded access unit → capture/encoder;
- access unit but no accepted sample/RTP → sender/profile/queue;
- RTP sent but not received → network path;
- packets received but no decoded frames → codec/packetization/parameters;
- decoded but not presented → Viewer playback.

## Required validation before merging media changes

- Unit/integration tests for H.264 SDP/profile contract, SPS/PPS + IDR recovery, timestamps, color conversion, fitted/aligned ultrawide dimensions, queue recovery, and epoch fencing.
- Packaged-app interoperability matrix: macOS Host/Viewer and Windows Host/Viewer in every pairing, with VideoToolbox, Media Foundation, and forced OpenH264 fallback where applicable.
- Exercise LAN, Direct, and Stunar Broadcast; include late Viewer join, reconnect, capture restart, encoder failure, PLI/FIR, rejected video section, and repeated Start/Stop.
- Test 16:9, 16:10, 21:9/32:9 including 5120×1440, static text, motion, saturated colors/gradients, and 720p30/1080p30/1080p60.
- For macOS capture, use the packaged `.app`, not only `tauri dev`, so TCC Screen Recording behavior is representative.
