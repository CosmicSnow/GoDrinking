# Sala Wire Contract (v1)

Normative for `server/server.mjs`. JSON only, UTF-8. The server is
signaling-only: it routes envelopes, never carries media, never parses or
modifies SDP/candidate semantics, and never logs passwords, tokens, SDP or
candidates.

## Transport

- HTTP + WebSocket on a loopback address only (`BIND`, default `127.0.0.1`;
  non-local binds are refused at startup). `PORT` env, default `18790`
  (`PORT=0` picks an ephemeral port for tests).
- `GET /health` → `200 {"ok":true}`. No other GET routes exist.

## REST

All POST bodies are JSON, max 64 KiB. Errors: `400 {"ok":false,"error":"invalid"}`,
`404 {"ok":false,"error":"denied"}`, `429 {"ok":false,"error":"busy"|"full"}`.

- `POST /v1/rooms` — `{nickname, password, admission?}` →
  `200 {ok:true, code, memberId, token}`. The creator is master, accepted.
  `code` is 6 crypto-random uppercase alphanumerics (up to 10 collision
  retries). `password` is mandatory (4–64 chars).
- `POST /v1/rooms/:code/join` — `{nickname, password}` →
  `200 {ok:true, status:"accepted"|"pending", memberId, token}`.
  `pending` only when the room was created with `admission:true`.
  Unknown code and wrong password are indistinguishable: same scrypt cost,
  same 50–80 ms delay, same `404 denied`; both count as auth failures.
- `POST /v1/rooms/:code/leave` — `{token}` → `200 {ok:true}`. The token is
  invalidated. If the master leaves, the oldest remaining accepted member
  (`joinedAt`, lexical id tie-break) becomes master; an empty room is
  destroyed.

## WebSocket

- Connect with `?token=<opaque-token>`. Unknown tokens get `404` at upgrade.
  One socket per member; reconnect replaces the old one. On connect, pending
  members receive `{t:"pending"}`; accepted members receive
  `{t:"admitted", member_id}` plus the roster.
- Max frame 64 KiB; oversize frames are dropped by the transport.
- Every message refreshes the member heartbeat. `{t:"heartbeat"}` is
  answered with the current roster.

Client → server (all require an accepted member unless noted):

- `{t:"heartbeat"}` (pending members allowed)
- `{t:"announce-share"}` / `{t:"stop-share"}` — sets own share flag.
- `{t:"watch", to}` / `{t:"unwatch", to}` — forwarded to `to` as
  `{t, from, to}`; `to` must be another accepted member.
- `{t:"offer"|"answer"|"candidate"|"ice-complete", to, payload}` — routed to
  `to` as `{t:"signal", from, to, payload}` (see envelopes).
- `{t:"kick", target}` — master only. The target's token is invalidated, its
  socket closed with `{t:"kicked"}`.
- `{t:"admit", member, accept}` — master only. Admits (capacity-checked) or
  removes a pending member.

Server → client: `{t:"admitted"}`, `{t:"pending"}`, `{t:"roster", entries,
master_id}` with `entries:[{id, nickname, master, share}]`,
`{t:"signal", from, to, payload}`, `{t:"pending", member_id, nickname}` (to
master), `{t:"kicked"}` / `{t:"gone"}`, `{t:"ok"}`,
`{t:"error", error:"invalid"|"forbidden"|"stale"|"full"}`.

## Signal envelopes

Versioned, exact keys only — extra keys are rejected:

- offer/answer: `{type, session, share, link, attempt, sdp}`
- candidate: `{type, session, share, link, attempt, candidate}`
- ice-complete: `{type, session, share, link, attempt}`

`session/share/link/attempt` are opaque ids (string 1–256 or non-negative
integer). `to` must be another accepted member. Fences: an offer establishes
the current attempt for a member pair (a numerically lower offer attempt is
stale); answers, candidates and ice-complete must match the current attempt;
max 64 candidates per attempt; max 8 KiB per candidate; candidates containing
`typ relay` are rejected (no TURN in this product).

## Limits

256 rooms; 512 sockets; 8 accepted + 8 pending members per room; 64 KiB per
message; 64 candidates per attempt; 8 KiB per candidate; heartbeat expected
every 30 s, members expire after 5 min without heartbeat; per-IP rate limits
per route; 5 auth failures in 10 min ignores the IP for 5 min. Tunable for
tests via `HEARTBEAT_TTL_MS` and `GC_INTERVAL_MS`.
