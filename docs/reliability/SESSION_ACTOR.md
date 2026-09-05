# Session actor migration contract

This document defines phase 1 of the goDrinking reliability program. It changes control-plane ownership only; it must not change the Media wire protocol, capture pixel conversion, encoder configuration, or RTP packetization.

## Scope and invariants

- Broadcast must retain its LAN, Direct, and Stunar semantics.
- The `SessionActor` is the only mutable owner of Session, join-service, Share slot, and Viewer link lifecycle state.
- `MediaSessionSnapshot` is an immutable observation. Calling it must never process Signaling, Admission, or lifecycle work.
- Frames, encoded access units, RTP, and high-rate statistics never enter the actor mailbox.
- A Session has a `SessionEpoch`; each Share start has a `ShareEpoch`; every Viewer link has a `LinkId`. A completion is accepted only when all applicable IDs still match.
- A stopped Share slot owns no capture or system-audio resource.
- Start publishes readiness only after all required resources are acquired. Stop publishes Idle only after their cleanup completes.

## Transition table

| Current state | Command or event | Next state | Side effects |
|---|---|---|---|
| Idle | Start Session | Opening | Validate request and capabilities; allocate SessionEpoch and provisional Start transaction. |
| Opening | Join service + Share ready | Open / Share Live | Publish immutable snapshot and diagnostics. |
| Opening | Any acquisition failure or Stop | Closing | Cancel transaction; clean acquired resources. |
| Open | Start Share | Share Starting | Allocate ShareEpoch; acquire capture, audio, pipeline. |
| Share Starting | Ready | Share Live | Attach fanout and allow fresh Viewer links. |
| Share Live | source/resolution/scale changed | Share Stopping | Close links, stop capture/audio/encoder, preserve SessionEpoch. |
| Share Stopping | cleanup complete | Open | Publish new ShareEpoch only on later Start. |
| Open / Share Live | Viewer request | Link Negotiating | Allocate LinkId; Signaling produces offer/answer events directly to actor. |
| Link Negotiating | ICE/peer failure | Link Closed | Remove link handle then stop it outside actor synchronization. |
| Any non-idle | Stop Session | Closing | Cancel pending work; detach all handles; cleanup outside actor state mutation. |
| Closing | cleanup complete | Idle | Publish final snapshot and resource counters. |

## Resource ledger

| Resource | Provisional owner | Running owner | Stop operation | Cleanup evidence |
|---|---|---|---|---|
| LAN listener / Direct listener / Stunar membership | `StartTransaction` | `JoinService` | close listener or leave Rendezvous | service stopped milestone |
| Screen capture | `ShareStartTransaction` | `ShareSlot` | adapter stop | capture stopped + callback quiesced |
| System audio tap | `ShareStartTransaction` | `ShareSlot` | tap stop/join | tap stopped counter |
| Encoder pipeline | `ShareStartTransaction` | `ShareSlot` | close input, join workers | worker exit + queue empty |
| Viewer sender | link transaction | `SenderLink` | remove handle then close/join | link closed milestone |

## Epoch policy

| Event or completion | Required fence | Stale behavior |
|---|---|---|
| Join-service open/close | SessionEpoch | release returned resource; emit redacted discard milestone |
| Capture/pipeline ready or failure | SessionEpoch + ShareEpoch | stop returned resources; do not alter snapshot |
| Encoder or capture mode change | SessionEpoch + ShareEpoch | discard output and restart only current Share slot |
| Offer/answer, ICE, peer state | SessionEpoch + ShareEpoch + LinkId | close stale peer; never insert its handle |
| Viewer queue recovery / keyframe request | SessionEpoch + ShareEpoch + LinkId | discard; current link owns recovery |
| Stop completion | SessionEpoch | do not publish Idle for a newer Session |

## Rollback matrix

| Failure or cancellation point | Required cleanup |
|---|---|
| invalid configuration or unsupported H.264 envelope | no resource acquired; remain Idle |
| capture picker cancelled | release provisional join service and audio; return Open only when a Session was intentionally opened without Share |
| LAN bind / Direct listener failure | release all provisional resources; do not discard error with `.ok()` |
| Stunar open failure | leave any partial Rendezvous membership; close heartbeat task |
| encoder initialization failure | stop capture and audio; drain queues; Session remains Open only if join service was already intentionally established |
| Stop during Start | cancel transaction, wait for cleanup, then publish Idle |
| stale offer after Share restart | close its peer and record epoch-discard event |
| Viewer failure | close only that link; retain Host capture and other Viewer links |

## Phase-1 acceptance evidence

1. State-transition and rollback tests cover every row above.
2. Tests prove stale Session/Share/Link events are discarded and all resource counters return to zero after rollback.
3. Signaling completes when frontend snapshot polling is disabled; polling remains presentation-only.
4. Repeated Start/Stop, Stop during Start, and Share restart succeed without leaked capture/audio resources.
5. A failing Viewer link does not interrupt another Viewer or the Host capture.
6. Logs contain correlated, redacted milestones sufficient to classify control-plane failures.
7. Existing packaged Broadcast smoke runs retain LAN, Direct, and Stunar behavior before Media-core changes begin.
