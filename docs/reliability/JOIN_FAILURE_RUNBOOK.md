# Join/viewer failure runbook (classify from evidence)

Never diagnose from "connection failed" alone. Walk the milestone chain in order and stop at the first missing
milestone; the table below maps that gap to the owning subsystem. All milestones are correlated by
Session/Share/Link/attempt IDs.

## Classification table

| First missing evidence | Verdict | Next step |
|---|---|---|
| No selected ICE candidate pair (no `selected-pair` / connected state) | signaling / ICE / network | check offer/answer attempt match, then firewall, NAT type, and reachability on the Direct port |
| First capture frame present, but no encoded access unit | capture / encoder | check capture source/TCC, encoder backend init, and SPS/profile output (`42e02a` expected) |
| Access unit produced, but no accepted sample / RTP sent | sender / profile / queue | check profile gate (High dropped in Baseline Session), queue overflow, and epoch fencing discards |
| RTP sent, but no packets received on the Viewer | network path | check routing/firewall between Host and Viewer; on Stunar, suspect symmetric NAT (best-effort, no TURN) |
| Packets received, but no decoded frames | codec / packetization / parameters | check `profile-level-id`, packetization mode 1, SPS/PPS+IDR delivery, and rejected video section (`m=video 0`) |
| Decoded frames, but nothing presented | Viewer playback | check Viewer track state, `ontrack` → packets → decoded → presentation milestones, and WebView rendering |

## Per-step log and milestone pointers

- Join service: `golive2 conn` (LAN/Direct HELLO/AUTH/NICK, `OK`/`PENDING`/`REJECT`/`ERR RETRY`), `stunar open` /
  `stunar prepare` / `stunar commit` / `stunar abort` (Stunar publish lifecycle), `lan find` / `direct connect` /
  `stunar ask` (Viewer side).
- Admission: `admission pending` (queued with Viewer ID and depth), `admission decide` (accept/delivered),
  `stunar decide` / `stunar kick` (Rendezvous verdicts).
- Offer/answer: `golive2 offer` (minted, with fence summary), `golive2 answer` (accepted with fence, or dropped
  stale with reason), `mint offer` (engine mint, resend of unanswered offers, full-session guard), `room answer`
  (drain counts, applied vs dropped with Viewer ID), `stunar offer` / `stunar answer` (WS send/replacement/accept/drop).
- Link/ICE/media: ICE candidate and selected-pair state, peer connected/failed, first capture frame, first encoder
  input/output, first valid IDR with parameters, first RTP sent, Viewer packet/decode/presentation milestones.
- Stale completions: `epoch discard` (event + Session/Share/Link that no longer match — expected after
  Stop/restart, Share replacement, or link replacement; only alarming when the current fence is discarded).

## Redaction statement

Logs and pasted diagnostics must never contain Passwords, Tokens, or complete SDP. Safe to share: Room codes,
Viewer/Nickname IDs, Session/Share/Link/attempt IDs, milestone counts, error classes (`ERR AUTH`, `denied`,
`full`, `m=video 0`), and redacted fence summaries.

## Stunar symmetric-NAT note

Stunar is signaling-only best-effort P2P; there is no TURN in this version. A Session that completes signaling
(offer/answer exchanged, fence accepted) but never selects an ICE pair across the internet is an expected,
diagnosable symmetric-NAT limitation, not a bug in the media path. Record it as `RTP-sent-not-received → network`
with the NAT types of both ends.
