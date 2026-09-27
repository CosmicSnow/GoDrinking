# Sala Wire Contract (v1)

Normative for `server/server.mjs`. JSON only, UTF-8. The server is
signaling-only: it routes envelopes, never carries media, never parses or
modifies SDP/candidate semantics, and never logs passwords, tokens, SDP or
candidates.

## Transport

- HTTP + WebSocket. `BIND` default `127.0.0.1`; Docker sets `0.0.0.0` (or `::`)
  behind a reverse proxy. Binding a specific public address is refused.
  `PORT` env, default `18790` (`PORT=0` picks an ephemeral port for tests).
- `GET /health` → `200 {"ok":true}`. No other GET routes exist.
- Malformed HTTP request targets that Node rejects at the parser layer have
  their connection closed; a target rejected by URL parsing gets `400 invalid`
  (and a malformed WebSocket upgrade target gets `400` then close). These
  failures do not terminate the server.

### Optional reverse-proxy client identity

By default, client identity for per-IP limits and authentication penalties is
the direct TCP socket peer. The server ignores all proxy identity headers.
Deployments may opt in to `TRUSTED_PROXY_PEER`, whose value must be the exact
IP literal of the immediate trusted reverse proxy peer (for example, the
Nginx Proxy Manager (NPM) address that connects directly to this server). When
the socket peer matches that configured address, the request must include the
`X-Real-IP` header. Its value must be exactly one valid IPv4 or IPv6 literal:
missing, malformed, or non-single values (including comma-separated lists) are
rejected with HTTP `400`. This rule applies to both HTTP requests and WebSocket
upgrade requests. A peer that does not match `TRUSTED_PROXY_PEER` is untrusted
and its `X-Real-IP` header is ignored; its socket peer remains its identity.
`X-GoDrinking-Client-IP`, `X-Forwarded-For`, and `CF-Connecting-IP` are never
used as request identity. If the variable is unset, all requests use the
socket peer, as before.

Enabling this setting requires a coordinated deployment, in this order:

1. Configure NPM's real-IP handling so `$remote_addr` is derived from
   `CF-Connecting-IP` only when the TCP peer is in Cloudflare's maintained,
   verified CIDR ranges (`set_real_ip_from`); do not trust that header from
   arbitrary peers. Restrict NPM ingress to Cloudflare as appropriate. NPM's
   included `proxy.conf` must overwrite `X-Real-IP` with `$remote_addr` (rather
   than append or pass through a client-supplied value).
2. Restrict access to the origin and backend so clients cannot bypass the
   trusted proxy and so only the intended immediate proxy can reach the server.
3. Only after the proxy behavior and network restrictions are in place, set
   `TRUSTED_PROXY_PEER` to that exact NPM peer IP in `.env.production` or the
   deployment secret store, then deploy/restart the server.

Do not trust `X-Forwarded-For` or `CF-Connecting-IP` directly as backend
identity, and do not enable `TRUSTED_PROXY_PEER` before NPM's Cloudflare-only
real-IP trust and `X-Real-IP` overwrite behavior are verified and protected.
These are deployment requirements; this documentation does not assert that
any current production deployment has been configured or fixed.

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
  At most four password scrypt jobs run concurrently; further create/join
  requests fail fast with `429 busy` until capacity is available.
- `POST /v1/rooms/:code/leave` — `{token}` → `200 {ok:true}`. The token is
  invalidated. If the master leaves, the oldest remaining accepted member
  (`joinedAt`, lexical id tie-break) becomes master; an empty room is
  destroyed.

## WebSocket

- Connect with `?token=<opaque-token>`. Unknown tokens get `404` at upgrade.
  One socket per member; reconnect replaces the old one. On connect, pending
  members receive `{t:"pending"}`; accepted members receive
  `{t:"admitted", member_id}` plus the roster.
- Max frame 64 KiB; oversize frames are dropped by the transport. WebSocket
  parsing is configured for at most 32 fragments and 1,024 buffered chunks;
  permessage-deflate is disabled.
- Every valid WebSocket message refreshes the member heartbeat. Heartbeats
  receive the current roster no more often than once per 250 ms per socket.
  Per-socket inbound token buckets allow a burst of 1,024 messages and refill
  at 256 messages/second, plus a burst of 8 MiB and refill at 1 MiB/second.
  Exceeding either per-socket bucket terminates the socket; these are token
  buckets, not fixed one-second windows. A process-wide ingress budget shared
  by all sockets additionally allows a burst of 8,192 messages refilling at
  2,048 messages/second and a burst of 32 MiB refilling at 4 MiB/second.
  WebSocket data messages and ping/pong control frames both consume this global
  budget. Control frames also have separate per-socket buckets: 32-frame burst,
  refilling at 8 ping or pong frames/second. The server disables automatic
  pong and answers accepted pings manually. The server sends pings every 30 s.
  An outbound send is rejected by terminating the destination socket if its
  queued output plus the message would exceed 2 MiB per socket or 32 MiB across
  all sockets. This overload handling terminates the socket being sent to or
  queued on; it is not a guarantee of room-level fairness. Signaling overload
  can therefore fail silently to the sender: the server does not send a
  delivery failure, and the core currently ignores WebSocket send errors.
- Closing the current socket starts a disconnect grace (default 8 s,
  `DISCONNECT_GRACE_MS`). A reconnect replaces the prior socket and cancels
  that timer; if the replacement later closes, a fresh grace starts from that
  latest close. If the member does not reconnect before the active grace
  expires, they are removed and the roster is broadcast (`{t:"gone"}`).
  Reconnect within the grace keeps the same member and token.

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

Admission is room opt-in at creation (`admission:true`). Joins are pending
until the master admits them; pending members may heartbeat but cannot signal
or otherwise use accepted-member actions. Rooms cap at 8 accepted and 8
pending members; accepting beyond the accepted cap returns `full`.

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
stale); answers, candidates and ice-complete must match the current attempt.
Candidates may arrive before their offer and are preserved against that
attempt. Max 64 candidates per attempt; max 8 KiB per candidate; candidates
containing `typ relay` are rejected (no TURN in this product). Signaling
history is bounded to 256 offer-link records and 256 candidate-attempt records
per room, 4,096 offer-link records and 8,192 candidate-attempt records
globally. These are retained history quotas, not caps on currently concurrent
links: records remain until a participant is removed or the room is destroyed.
When a state quota is exhausted, a new record is refused with WebSocket
`{t:"error", error:"full"}`.

## Limits

256 rooms; 512 sockets; 8 accepted + 8 pending members per room; 64 KiB HTTP
body/WebSocket frame; 4 concurrent password scrypt jobs; 64 candidates per
attempt; 8 KiB per candidate. Heartbeat expiry is 5 min (heartbeat expected
every 30 s); disconnect grace is 8 s after socket close without reconnect.
Per-IP rate limits use the request identity (the direct TCP peer by default;
see [Optional reverse-proxy client identity](#optional-reverse-proxy-client-identity)
for the explicitly configured trusted-proxy opt-in), per rolling 60 s: create
10, join 20, leave 30, and WebSocket upgrades 30. Only these supported REST
routes are rate-limited; unmatched REST routes are denied before rate limiting.
Five authentication failures in 10 min ignore that identity for 5 min. Without
the opt-in, clients behind one proxy share its peer identity, so limits and
authentication penalties can aggregate across them. Do not treat these IP-based
limits or penalties as per-end-user production protections.
Heartbeat TTL, GC interval (15 s default), and disconnect grace are tunable via
`HEARTBEAT_TTL_MS`, `GC_INTERVAL_MS`, and `DISCONNECT_GRACE_MS`.

The Docker Compose `/health` check reports container health only; Docker does
not automatically restart a container merely because it becomes unhealthy.
