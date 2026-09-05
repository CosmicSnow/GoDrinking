# Packaged-app interoperability matrix (MANUAL)

Environment-dependent manual evidence for the goDrinking reliability program. Every row is MANUAL: it requires real
macOS/Windows machines, real displays, and the packaged goDrinking Viewer (WKWebView on macOS, WebView2 on Windows).
External-browser results are out of scope and must not be recorded here.

## How to run a row

1. Build and install the packaged `.app` on Host and Viewer. For macOS capture rows, always use the packaged `.app`,
   never `tauri dev`, so TCC Screen Recording behavior is representative.
2. Start a Session with the row's join mode, encoder, resolution, fps, and content. Record Host/Viewer platform,
   encoder backend, and network path.
3. Mark PASS only when all three hold: (a) non-black media renders on the Viewer; (b) the redacted milestone
   sequence is complete (join/admission, offer/answer with matching attempt, ICE selected pair, link connected,
   first capture frame, first encoder input/output, first valid IDR with parameters, first RTP sent, Viewer
   packets → decode → presentation); (c) keyframe recovery works (late join decodes within one intra period;
   PLI/FIR and queue-overflow recovery force a new IDR for the affected link).
4. Never paste Passwords, Tokens, or SDP payloads into results. Milestone IDs (Session/Share/Link/attempt) are safe.

## A. Broadcast connectivity: Host × Viewer × join mode (16 rows)

| # | Host | Viewer | Join mode | Status |
|---|---|---|---|---|
| A1 | macOS (VideoToolbox) | macOS (WKWebView) | LAN | MANUAL |
| A2 | macOS (VideoToolbox) | macOS (WKWebView) | Direct | MANUAL |
| A3 | macOS (VideoToolbox) | macOS (WKWebView) | Stunar | MANUAL |
| A4 | macOS (VideoToolbox) | Windows (WebView2) | LAN | MANUAL |
| A5 | macOS (VideoToolbox) | Windows (WebView2) | Direct | MANUAL |
| A6 | macOS (VideoToolbox) | Windows (WebView2) | Stunar | MANUAL |
| A7 | Windows (Media Foundation) | macOS (WKWebView) | LAN | MANUAL |
| A8 | Windows (Media Foundation) | macOS (WKWebView) | Direct | MANUAL |
| A9 | Windows (Media Foundation) | macOS (WKWebView) | Stunar | MANUAL |
| A10 | Windows (Media Foundation) | Windows (WebView2) | LAN | MANUAL |
| A11 | Windows (Media Foundation) | Windows (WebView2) | Direct | MANUAL |
| A12 | Windows (Media Foundation) | Windows (WebView2) | Stunar | MANUAL |
| A13 | macOS (forced OpenH264) | Windows (WebView2) | Stunar | MANUAL |
| A14 | Windows (forced OpenH264) | macOS (WKWebView) | Stunar | MANUAL |
| A15 | macOS (forced OpenH264) | macOS (WKWebView) | LAN | MANUAL |
| A16 | Windows (forced OpenH264) | Windows (WebView2) | Direct | MANUAL |

Forced-OpenH264 rows apply where the fallback is available; each must still emit the H.264 Constrained Baseline
contract (`42e02a`, packetization mode 1).

## B. Stunar Sala (existing behavior, 4 rows)

| # | Host (Master) | Member | Scenario | Status |
|---|---|---|---|---|
| S1 | macOS | Windows | join + watch + unwatch, roster shrinks | MANUAL |
| S2 | Windows | macOS | join + watch + unwatch, roster shrinks | MANUAL |
| S3 | macOS | macOS | Master leaves, crown passes to oldest remaining member | MANUAL |
| S4 | Windows | Windows | member WS kill, ghost pruned, cap not eaten | MANUAL |

## C. Resilience scenarios (7 rows, Broadcast reference pairing + Stunar spot-check)

| # | Scenario | Pass criteria | Status |
|---|---|---|---|
| C1 | Late Viewer join after Share replacement | uses current Share fence; decodes within one intra period | MANUAL |
| C2 | Viewer reconnect (WS/TCP drop, same Session) | re-offers with fresh attempt; stale attempt rejected without side effects | MANUAL |
| C3 | Capture source/resolution/scale change | local Share slot restarts, Session stays open, Viewers recover via new IDR | MANUAL |
| C4 | Encoder failure mid-Session | affected link closed in isolation; other Viewers keep media | MANUAL |
| C5 | PLI/FIR during Session | new keyframe forced for the affected link only | MANUAL |
| C6 | Viewer answers with rejected video section | Host logs rejection with IDs; other Viewers unaffected | MANUAL |
| C7 | Repeated Start/Stop (≥5 cycles) | each cycle commits cleanly; no leaked workers or ghost roster entries | MANUAL |

## D. Aspect ratios and resolutions (5 rows)

| # | Format | Notes | Status |
|---|---|---|---|
| D1 | 16:9 | e.g. 1920×1080 | MANUAL |
| D2 | 16:10 | e.g. 2560×1600 | MANUAL |
| D3 | 21:9 ultrawide | e.g. 3440×1440 | MANUAL |
| D4 | 32:9 ultrawide | e.g. 3840×1080 | MANUAL |
| D5 | 5120×1440 → fitted even/aligned | delivered as 1920×528 (or backend-aligned equivalent); aspect preserved, never 16:9-forced | MANUAL |

## E. Content types (4 rows)

| # | Content | What to check | Status |
|---|---|---|---|
| E1 | Static text / desktop | legible text, no smearing after IDR | MANUAL |
| E2 | Motion / scrolling | no persistent artifacts; recovery after burst loss | MANUAL |
| E3 | Saturated colors | no clipping vs Host intent; BT.709 limited-range path | MANUAL |
| E4 | Gradients | no banding beyond encoder baseline; Host preview is not proof of Viewer color | MANUAL |

## F. Performance envelope (3 rows)

| # | Preset | Criteria | Status |
|---|---|---|---|
| F1 | 720p30 | stable encode + render, no queue-overflow IDR storms | MANUAL |
| F2 | 1080p30 | stable encode + render, no queue-overflow IDR storms | MANUAL |
| F3 | 1080p60 | stable encode + render within the 60 fps envelope (120 fps out of scope) | MANUAL |

Total: 16 + 4 + 7 + 5 + 4 + 3 = **39 MANUAL rows**. Automated suites (cargo 180, Rendezvous 6, frontend 45) cover
contracts, fencing, and lifecycle logic; they do not replace a single row above.
