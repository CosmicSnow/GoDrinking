// goDrinking Rendezvous — the whole server in one file.
//
// RAM only. node:http + ws + node:crypto. No DB, no Redis, no disk.
// Protocol: docs/connectivity/PROTOCOL.md section C.
// Rules that must not be "simplified": docs/connectivity/SECURITY.md.
//
//   PORT=8787 BIND=127.0.0.1 node server.mjs
//   TRUST_PROXY=1  -> trust X-Forwarded-For (only behind a proxy you control)

import http from "node:http";
import { randomBytes, scrypt, timingSafeEqual } from "node:crypto";
import { WebSocketServer } from "ws";

/** Master succession for Sala. Oldest remaining joinedAt, then id. */
function nextMaster(members, leavingId) {
  const rest = members
    .filter((member) => member.id !== leavingId)
    .slice()
    .sort((left, right) => {
      if (left.joinedAt !== right.joinedAt) return left.joinedAt - right.joinedAt;
      return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
    });
  return rest[0] ? rest[0].id : null;
}

const PORT = Number(process.env.PORT || 8787);
const BIND = process.env.BIND || "127.0.0.1";
const TRUST_PROXY = process.env.TRUST_PROXY === "1";

const MAX_ROOMS = 256;
const MAX_WS = 512;
const MAX_ACCEPTED = 8;
const MAX_PENDING = 8;
// Overridable so the 5-minute expiry can be tested quickly.
const HEARTBEAT_TTL_MS = Number(process.env.HEARTBEAT_TTL_MS || 5 * 60 * 1000);
const GC_INTERVAL_MS = 15 * 1000;
const BODY_LIMIT = 64 * 1024;
const HEADER_TIMEOUT_MS = 5 * 1000;
const REQUEST_TIMEOUT_MS = 15 * 1000;
const WS_PING_MS = 30 * 1000;
const SCRYPT_N = 16384;
const SCRYPT_R = 8;
const SCRYPT_P = 1;
const SCRYPT_KEYLEN = 32;
const SALT_LEN = 16;
const RATE_WINDOW_MS = 60 * 1000;
const RATE_LIMITS = {
  "host/open": 5,
  "host/heartbeat": 6,
  "viewer/ask": 10,
  "member/leave": 10,
  "member/heartbeat": 12,
  "master/kick": 10,
  ws: 20,
  rest: 60,
};
const IGNORE_WINDOW_MS = 10 * 60 * 1000;
// Escalating ignore: 5,10,15,20 fails in the window -> 15min,1h,6h,24h.
const IGNORE_PENALTIES = [15 * 60 * 1000, 60 * 60 * 1000, 6 * 60 * 60 * 1000, 24 * 60 * 60 * 1000];
const FAKE_TOKENS_MAX = 100;
const FAKE_TOKEN_TTL_MS = 2 * 60 * 1000;
const FAKE_TOKEN_GC_MS = 30 * 1000;
const TARPIT_SOCKET_MS = 60 * 1000;
const NICK_RE = /^[A-Za-z0-9 _\-.]+$/;
const ROUTES = new Set([
  "/v1/host/open",
  "/v1/host/heartbeat",
  "/v1/host/rotate",
  "/v1/host/close",
  "/v1/viewer/ask",
  "/v1/host/decide",
  "/v1/member/leave",
  "/v1/member/heartbeat",
  "/v1/master/kick",
]);

// --- State (RAM) -----------------------------------------------------------

/** @type {Map<string, object>} code -> Room */
const rooms = new Map();
/** @type {Map<string, {role: "host"|"viewer", code: string, viewerId?: string}>} */
const tokens = new Map();
/** @type {Map<string, Map<string, number[]>>} ip -> route -> hit timestamps */
const rate = new Map();
/** @type {Map<string, {fails: number[], until: number, level: number}>} ip -> ignore entry */
const ignore = new Map();
/** @type {Map<string, number>} fake viewer_token -> createdAt (tarpit) */
const fakeTokens = new Map();

// Room = { code, passwordHash, passwordSalt, admission, hostNickname,
//          hostToken, heartbeatAt, hostWs, viewers: Map<id, Viewer> }
// Viewer = { id, nickname, token, state: "pending"|"accepted", ws, inbox }

// --- Helpers ---------------------------------------------------------------

function ipOf(req) {
  let ip = req.socket.remoteAddress || "-";
  if (TRUST_PROXY) {
    const forwarded = req.headers["x-forwarded-for"];
    if (typeof forwarded === "string" && forwarded.length) {
      ip = forwarded.split(",")[0].trim();
    }
  }
  // Normalize IPv4-mapped IPv6 ("::ffff:1.2.3.4" -> "1.2.3.4").
  if (ip.startsWith("::ffff:")) ip = ip.slice(7);
  return ip;
}

function log(level, ip, event, code = "-", viewer = "-", error = "-") {
  const line = [new Date().toISOString(), level, ip, event, code, viewer, error].join(" ");
  if (level === "warn") console.warn(line);
  else console.log(line);
}

function randomToken() {
  return randomBytes(32).toString("hex");
}

function randomViewerId() {
  return randomBytes(4).toString("hex");
}

function randomCode() {
  // 4 random bytes -> 6 base36 chars (A-Z0-9). Mask to 31 bits so the
  // base36 form never exceeds 6 chars.
  return (randomBytes(4).readUInt32BE(0) & 0x7fffffff).toString(36).toUpperCase().padStart(6, "0");
}

function scryptAsync(password, salt) {
  return new Promise((resolve, reject) => {
    scrypt(password, salt, SCRYPT_KEYLEN, { N: SCRYPT_N, r: SCRYPT_R, p: SCRYPT_P }, (err, key) => {
      if (err) reject(err);
      else resolve(key);
    });
  });
}

function normalizeCode(code) {
  return typeof code === "string" ? code.trim().toUpperCase() : "";
}

function validNickname(nickname) {
  if (typeof nickname !== "string") return false;
  const trimmed = nickname.trim();
  const len = [...trimmed].length;
  return len >= 2 && len <= 24 && NICK_RE.test(trimmed);
}

function validPassword(password) {
  if (typeof password !== "string") return false;
  const len = [...password].length;
  return len >= 4 && len <= 64;
}

function sendJson(res, status, obj) {
  res.writeHead(status, { "Content-Type": "application/json", "Connection": "close" });
  res.end(JSON.stringify(obj));
}

function ok(res, obj) {
  sendJson(res, 200, obj);
}

function deny(res) {
  sendJson(res, 404, { ok: false, error: "denied" });
}

function invalid(res) {
  sendJson(res, 400, { ok: false, error: "invalid" });
}

function busy(res) {
  sendJson(res, 429, { ok: false, error: "busy" });
}

function full(res) {
  sendJson(res, 429, { ok: false, error: "full" });
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// --- Rate limit + Ignore list ----------------------------------------------

function rateLimit(ip, route, limit) {
  const now = Date.now();
  let byRoute = rate.get(ip);
  if (!byRoute) {
    byRoute = new Map();
    rate.set(ip, byRoute);
  }
  let hits = byRoute.get(route);
  if (!hits) {
    hits = [];
    byRoute.set(route, hits);
  }
  while (hits.length && now - hits[0] > RATE_WINDOW_MS) hits.shift();
  if (hits.length >= limit) return false;
  hits.push(now);
  return true;
}

function isIgnored(ip) {
  const entry = ignore.get(ip);
  if (!entry) return false;
  if (Date.now() < entry.until) return true;
  // Ban expired: keep the entry while it still carries fail history (the
  // window may still be open); drop it once it is fully stale.
  if (!entry.fails.length) ignore.delete(ip);
  return false;
}

// Tarpit: penalized IPs (level >= 1) get fake `pending` answers instead of
// `denied`, so scanners cannot tell a live room from a dead one.
function isTarpitted(ip) {
  const entry = ignore.get(ip);
  return !!entry && entry.level >= 1 && Date.now() < entry.until;
}

function noteAuthFailure(ip) {
  const now = Date.now();
  let entry = ignore.get(ip);
  if (!entry) {
    entry = { fails: [], until: 0, level: 0 };
    ignore.set(ip, entry);
  }
  entry.fails = entry.fails.filter((t) => now - t < IGNORE_WINDOW_MS);
  entry.fails.push(now);
  const fails = entry.fails.length;
  const nextLevel = fails >= 20 ? 4 : fails >= 15 ? 3 : fails >= 10 ? 2 : fails >= 5 ? 1 : 0;
  if (nextLevel > entry.level) {
    // Escalate: 5,10,15,20 fails in the window -> 15min,1h,6h,24h.
    entry.level = nextLevel;
    entry.until = now + IGNORE_PENALTIES[nextLevel - 1];
    entry.fails = [];
  } else if (nextLevel === entry.level && entry.level > 0) {
    // Penalty expired and the IP is still failing: re-apply, count restarts.
    entry.until = now + IGNORE_PENALTIES[entry.level - 1];
    entry.fails = [];
  }
}

function storeFakeToken(token) {
  if (fakeTokens.size >= FAKE_TOKENS_MAX) {
    const oldest = fakeTokens.keys().next().value;
    if (oldest !== undefined) fakeTokens.delete(oldest);
  }
  fakeTokens.set(token, Date.now());
}

// --- Room lifecycle --------------------------------------------------------

function rosterEntries(room) {
  if (room.mode === "room") {
    return [...room.members.values()].map((member) => ({
      id: member.id,
      nickname: member.nickname,
      state: member.share ? "sharing" : "accepted",
      master: member.id === room.masterId,
      share: member.share === true,
    }));
  }
  return [...room.viewers.values()].map((v) => ({
    id: v.id,
    nickname: v.nickname,
    state: v.state,
  }));
}

function broadcastRoster(room) {
  const entries = rosterEntries(room);
  send(room.hostWs, { t: "roster", entries, master_id: room.masterId ?? null, mode: room.mode });
  if (room.mode === "room") {
    for (const member of room.members.values()) {
      send(member.ws, { t: "roster", entries, master_id: room.masterId, mode: "room" });
    }
  }
}

function send(ws, obj) {
  if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
}

/** Deletes the room and invalidates every token. Viewers get `gone`. */
function destroyRoom(room) {
  rooms.delete(room.code);
  tokens.delete(room.hostToken);
  for (const viewer of room.viewers.values()) {
    tokens.delete(viewer.token);
    send(viewer.ws, { t: "gone" });
    if (viewer.ws) viewer.ws.close(4000, "gone");
  }
  if (room.members) {
    for (const member of room.members.values()) {
      tokens.delete(member.token);
      send(member.ws, { t: "gone" });
      if (member.ws) member.ws.close(4000, "gone");
    }
  }
  if (room.hostWs) room.hostWs.close(4000, "gone");
}

function promoteMaster(room, newMasterId) {
  const member = room.members.get(newMasterId);
  if (!member) return;
  room.masterId = newMasterId;
  room.hostToken = member.token;
  const tokenMeta = tokens.get(member.token);
  if (tokenMeta) tokenMeta.role = "host";
  member.role = "host";
  send(member.ws, { t: "you-are-master", member_id: member.id });
  broadcastRoster(room);
}

function dropMember(room, memberId) {
  const member = room.members.get(memberId);
  if (!member) return;
  const leavingWasMaster = room.masterId === memberId;
  room.members.delete(memberId);
  tokens.delete(member.token);
  send(member.ws, { t: "gone" });
  if (member.ws) member.ws.close(4000, "gone");
  if (room.members.size === 0) {
    destroyRoom(room);
    return;
  }
  if (leavingWasMaster) {
    const next = nextMaster(
      [...room.members.values()].map((item) => ({ id: item.id, joinedAt: item.joinedAt })),
      "",
    );
    if (next) promoteMaster(room, next);
    else destroyRoom(room);
  } else {
    broadcastRoster(room);
  }
}

// --- REST handlers ---------------------------------------------------------

async function handleOpen(ip, json, res) {
  if (!validNickname(json.nickname)) return invalid(res);
  const password = typeof json.password === "string" ? json.password : "";
  if (!validPassword(password)) return invalid(res);
  if (rooms.size >= MAX_ROOMS) return busy(res);

  // The server picks the code; a client-supplied one is ignored (compat).
  let code = null;
  for (let attempt = 0; attempt < 10; attempt += 1) {
    const candidate = randomCode();
    if (!rooms.has(candidate)) {
      code = candidate;
      break;
    }
  }
  if (!code) return busy(res); // 10 collisions: no free code

  const hostToken = randomToken();
  const passwordSalt = randomBytes(SALT_LEN);
  const passwordHash = await scryptAsync(password, passwordSalt);
  const mode = json.mode === "room" ? "room" : "broadcast";
  const masterId = randomViewerId();
  const now = Date.now();
  const room = {
    code,
    mode,
    passwordHash,
    passwordSalt,
    admission: json.admission === true,
    hostNickname: json.nickname,
    hostToken,
    masterId,
    heartbeatAt: now,
    hostWs: null,
    viewers: new Map(),
    members: new Map(),
  };
  if (mode === "room") {
    room.members.set(masterId, {
      id: masterId,
      nickname: json.nickname,
      token: hostToken,
      joinedAt: now,
      heartbeatAt: now,
      ws: null,
      share: false,
      role: "host",
    });
  }
  rooms.set(code, room);
  tokens.set(hostToken, { role: "host", code, memberId: masterId });
  log("info", ip, "open", code, "-", mode);
  ok(res, { ok: true, host_token: hostToken, code, mode, member_id: masterId });
}

function handleHeartbeat(ip, json, res) {
  return handleMemberHeartbeat(ip, json, res);
}

async function handleRotate(ip, json, res) {
  const token = typeof json.host_token === "string" ? json.host_token : "";
  const entry = tokens.get(token);
  if (!entry || entry.role !== "host") return deny(res);
  const room = rooms.get(entry.code);
  if (!room || room.hostToken !== token) return deny(res);

  // The code is server-owned; rotate only changes the mandatory Password.
  const password = typeof json.password === "string" ? json.password : "";
  if (!validPassword(password)) return invalid(res);
  room.passwordSalt = randomBytes(SALT_LEN);
  room.passwordHash = await scryptAsync(password, room.passwordSalt);
  room.heartbeatAt = Date.now();
  log("info", ip, "rotate", room.code);
  ok(res, { ok: true });
}

function handleClose(ip, json, res) {
  const token = typeof json.host_token === "string" ? json.host_token : "";
  const entry = tokens.get(token);
  if (!entry || entry.role !== "host") return deny(res);
  const room = rooms.get(entry.code);
  if (!room || room.hostToken !== token) return deny(res);
  log("info", ip, "close", room.code);
  destroyRoom(room);
  ok(res, { ok: true });
}

async function handleAsk(ip, json, res) {
  if (isTarpitted(ip)) {
    // Tarpit: burn CPU and answer with a fake `pending` so the scanner
    // cannot tell a live room from a dead one. The token is dead on arrival.
    await scryptAsync("", randomBytes(SALT_LEN)); // equalize timing with the hash path
    await delay(50 + Math.random() * 30);
    const viewerToken = randomToken();
    storeFakeToken(viewerToken);
    log("warn", ip, "ask", "-", "-", "tarpit");
    return ok(res, { ok: true, status: "pending", viewer_token: viewerToken });
  }
  const code = normalizeCode(json.code);
  const password = typeof json.password === "string" ? json.password : "";
  const nickname = typeof json.nickname === "string" ? json.nickname : "";
  const room = code ? rooms.get(code) : undefined;

  // Unknown room and wrong Password are indistinguishable: same scrypt cost,
  // same 50-80ms delay, same 404 denied, both count as auth failures.
  if (!room) {
    await scryptAsync("", randomBytes(SALT_LEN)); // equalize timing with the hash path
    await delay(50 + Math.random() * 30);
    noteAuthFailure(ip);
    log("warn", ip, "ask", code, "-", "denied");
    return deny(res);
  }
  if (!validNickname(nickname)) return invalid(res);
  const hash = await scryptAsync(password, room.passwordSalt);
  if (!timingSafeEqual(hash, room.passwordHash)) {
    await delay(50 + Math.random() * 30);
    noteAuthFailure(ip);
    log("warn", ip, "ask", code, "-", "denied");
    return deny(res);
  }

  // Sync section: no await between the full check and the insert.
  let accepted = 0;
  let pending = 0;
  for (const viewer of room.viewers.values()) {
    if (viewer.state === "accepted") accepted += 1;
    else pending += 1;
  }
  if (accepted >= MAX_ACCEPTED || pending >= MAX_PENDING) {
    log("warn", ip, "ask", code, "-", "full");
    return full(res);
  }

  const viewerId = randomViewerId();
  const viewerToken = randomToken();
  const state = room.admission ? "pending" : "accepted";
  const viewer = { id: viewerId, nickname, token: viewerToken, state, ws: null, inbox: null };
  room.viewers.set(viewerId, viewer);
  tokens.set(viewerToken, { role: "viewer", code, viewerId, memberId: viewerId });

  if (room.mode === "room") {
    room.members.set(viewerId, {
      id: viewerId,
      nickname,
      token: viewerToken,
      joinedAt: Date.now(),
      heartbeatAt: Date.now(),
      ws: null,
      share: false,
      role: "member",
    });
  }

  if (state === "accepted") {
    log("info", ip, "ask", code, viewerId, "accepted");
    ok(res, {
      ok: true,
      status: "accepted",
      viewer_token: viewerToken,
      mode: room.mode,
      member_id: viewerId,
      master_id: room.masterId ?? null,
    });
    broadcastRoster(room);
  } else {
    log("info", ip, "ask", code, viewerId, "pending");
    ok(res, { ok: true, status: "pending", viewer_token: viewerToken, mode: room.mode, member_id: viewerId });
    send(room.hostWs, { t: "pending", viewer_id: viewerId, nickname });
    broadcastRoster(room);
  }
}

function tokenRoom(token) {
  const entry = tokens.get(token);
  if (!entry) return null;
  const room = rooms.get(entry.code);
  if (!room) return null;
  return { entry, room };
}

function handleMemberLeave(ip, json, res) {
  const token = typeof json.token === "string" ? json.token : typeof json.host_token === "string" ? json.host_token : typeof json.viewer_token === "string" ? json.viewer_token : "";
  const found = tokenRoom(token);
  if (!found) return deny(res);
  const { entry, room } = found;
  if (room.mode !== "room") {
    if (entry.role === "host") {
      log("info", ip, "close", room.code);
      destroyRoom(room);
      return ok(res, { ok: true });
    }
    return deny(res);
  }
  const memberId = entry.memberId || entry.viewerId || room.masterId;
  log("info", ip, "leave", room.code, memberId);
  dropMember(room, memberId);
  ok(res, { ok: true });
}

function handleMemberHeartbeat(ip, json, res) {
  const token = typeof json.token === "string" ? json.token : typeof json.host_token === "string" ? json.host_token : "";
  const found = tokenRoom(token);
  if (!found) return deny(res);
  const { entry, room } = found;
  const now = Date.now();
  room.heartbeatAt = now;
  if (room.mode === "room") {
    const memberId = entry.memberId || entry.viewerId || room.masterId;
    const member = room.members.get(memberId);
    if (member) member.heartbeatAt = now;
  }
  ok(res, { ok: true, master_id: room.masterId ?? null });
}

function handleMasterKick(ip, json, res) {
  const token = typeof json.host_token === "string" ? json.host_token : typeof json.token === "string" ? json.token : "";
  const found = tokenRoom(token);
  if (!found || found.entry.role !== "host") return deny(res);
  const { room } = found;
  if (room.hostToken !== token) return deny(res);
  const targetId = typeof json.target_id === "string" ? json.target_id : "";
  if (!targetId || targetId === room.masterId) return invalid(res);
  log("info", ip, "kick", room.code, targetId);
  if (room.mode === "room") dropMember(room, targetId);
  else {
    const viewer = room.viewers.get(targetId);
    if (viewer) {
      room.viewers.delete(targetId);
      tokens.delete(viewer.token);
      send(viewer.ws, { t: "kicked" });
      if (viewer.ws) viewer.ws.close(4000, "kick");
      broadcastRoster(room);
    }
  }
  ok(res, { ok: true });
}

function handleDecide(ip, json, res) {
  const token = typeof json.host_token === "string" ? json.host_token : "";
  const entry = tokens.get(token);
  if (!entry || entry.role !== "host") return deny(res);
  const room = rooms.get(entry.code);
  if (!room || room.hostToken !== token) return deny(res);

  const viewerId = typeof json.viewer_id === "string" ? json.viewer_id : "";
  const action = json.action;
  const viewer = room.viewers.get(viewerId);
  if (!viewer) return ok(res, { ok: true }); // idempotent ack

  if (action === "accept") {
    if (viewer.state === "pending") {
      viewer.state = "accepted";
      log("info", ip, "decide", room.code, viewerId, "accept");
      send(viewer.ws, { t: "accepted", viewer_id: viewer.id });
      send(room.hostWs, { t: "roster", entries: rosterEntries(room) });
    }
  } else if (action === "reject" || action === "kick") {
    room.viewers.delete(viewerId);
    tokens.delete(viewer.token);
    log("info", ip, "decide", room.code, viewerId, action);
    send(viewer.ws, { t: action === "reject" ? "rejected" : "kicked" });
    if (viewer.ws) viewer.ws.close(4000, action);
    send(room.hostWs, { t: "roster", entries: rosterEntries(room) });
  } else {
    return invalid(res);
  }
  ok(res, { ok: true });
}

// --- WebSocket -------------------------------------------------------------

function isValidSignal(payload) {
  if (!payload || typeof payload !== "object") return false;
  if (payload.type !== "offer" && payload.type !== "answer") return false;
  if (typeof payload.sdp !== "string") return false;
  if (Buffer.byteLength(payload.sdp, "utf8") > BODY_LIMIT) return false;
  return true;
}

function handleClientMessage(ws, meta, data) {
  let msg;
  try {
    msg = JSON.parse(data.toString());
  } catch {
    return;
  }
  const room = rooms.get(meta.code);
  if (!room) return;
  const fromId = meta.role === "host" ? room.masterId : meta.viewerId;

  if (room.mode === "room" && (msg.t === "share-start" || msg.t === "share-stop")) {
    const member = fromId ? room.members.get(fromId) : null;
    if (!member) return;
    member.share = msg.t === "share-start";
    broadcastRoster(room);
    return;
  }

  if (room.mode === "room" && (msg.t === "watch" || msg.t === "unwatch")) {
    if (typeof msg.to !== "string") return;
    const dest = room.members.get(msg.to);
    if (!dest) return;
    const payload = { t: msg.t, from: fromId, to: msg.to };
    const sock =
      dest.ws && dest.ws.readyState === 1
        ? dest.ws
        : dest.id === room.masterId && room.hostWs && room.hostWs.readyState === 1
          ? room.hostWs
          : null;
    if (sock) send(sock, payload);
    return;
  }

  if (!msg || msg.t !== "signal" || !isValidSignal(msg.payload)) return;

  if (room.mode === "room" && typeof msg.to === "string") {
    const dest = room.members.get(msg.to);
    if (!dest) return;
    send(dest.ws, { t: "signal", from: fromId, to: msg.to, payload: msg.payload });
    if (dest.id === room.masterId) {
      send(room.hostWs, { t: "signal", from: fromId, to: msg.to, payload: msg.payload });
    }
    return;
  }

  if (meta.role === "host") {
    const viewerId = typeof msg.viewer_id === "string" ? msg.viewer_id : "";
    const viewer = room.viewers.get(viewerId);
    if (!viewer || viewer.state !== "accepted") return; // pending never gets SDP
    viewer.inbox = msg.payload; // 1-slot mailbox; a new offer replaces an unread one
    if (viewer.ws && viewer.ws.readyState === 1) {
      // Broadcast has no member ids: stamp the host so the viewer inbox
      // accepts the offer (it drops offers with empty `from`) and the
      // answer has a target. "host" matches the viewer UI convention.
      send(viewer.ws, { t: "signal", from: "host", payload: msg.payload });
      viewer.inbox = null; // delivered, not stored
    }
  } else {
    const viewer = room.viewers.get(meta.viewerId);
    if (!viewer || viewer.state !== "accepted") return;
    send(room.hostWs, { t: "signal", viewer_id: viewer.id, payload: msg.payload });
  }
}

const wss = new WebSocketServer({ noServer: true, maxPayload: BODY_LIMIT });

wss.on("connection", (ws, req, meta) => {
  const ip = ipOf(req);
  if (meta.role === "tarpit") {
    // Tarpit socket: looks alive, carries nothing, dies at 60s.
    ws.isAlive = true;
    ws.on("pong", () => {
      ws.isAlive = true;
    });
    ws.on("error", () => {});
    send(ws, { t: "roster", entries: [] });
    ws.on("message", () => {}); // swallow everything
    setTimeout(() => ws.close(4000, "tarpit"), TARPIT_SOCKET_MS);
    log("warn", ip, "ws", "-", "-", "tarpit");
    return;
  }
  const room = rooms.get(meta.code);
  if (!room) {
    ws.close(4000, "gone");
    return;
  }
  ws.isAlive = true;
  ws.on("pong", () => {
    ws.isAlive = true;
  });
  ws.on("error", () => {});

  if (meta.role === "host") {
    if (room.hostToken !== meta.token) {
      ws.close(4000);
      return;
    }
    room.hostWs = ws; // one socket per role; reconnect replaces it
    if (room.mode === "room" && room.masterId) {
      const master = room.members.get(room.masterId);
      if (master) master.ws = ws;
    }
    send(ws, { t: "roster", entries: rosterEntries(room), master_id: room.masterId ?? null, mode: room.mode });
    log("info", ip, "ws", room.code, "host");
  } else {
    const viewer = room.viewers.get(meta.viewerId);
    if (!viewer || viewer.token !== meta.token) {
      ws.close(4000);
      return;
    }
    viewer.ws = ws;
    if (room.mode === "room") {
      const member = room.members.get(viewer.id);
      if (member) member.ws = ws;
    }
    if (viewer.state === "accepted") send(ws, { t: "accepted", viewer_id: viewer.id });
    if (room.mode === "room") {
      send(ws, { t: "roster", entries: rosterEntries(room), master_id: room.masterId ?? null, mode: room.mode });
    }
    if (viewer.inbox) {
      // Inbox only ever holds a broadcast host offer (room-mode member
      // signals route directly): same `from` stamp as the live path.
      send(ws, { t: "signal", from: "host", payload: viewer.inbox });
      viewer.inbox = null;
    }
    log("info", ip, "ws", room.code, viewer.id);
  }

  ws.on("message", (data) => handleClientMessage(ws, meta, data));
  ws.on("close", () => {
    if (meta.role === "host") {
      if (room.hostWs === ws) room.hostWs = null;
    } else {
      const viewer = room.viewers.get(meta.viewerId);
      if (viewer && viewer.ws === ws) viewer.ws = null;
    }
  });
});

// --- HTTP server -----------------------------------------------------------

function readBody(req) {
  return new Promise((resolve) => {
    let size = 0;
    const chunks = [];
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > BODY_LIMIT) {
        req.pause();
        resolve({ error: "too_large" });
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve({ body: Buffer.concat(chunks).toString("utf8") }));
    req.on("error", () => resolve({ error: "read_error" }));
  });
}

const server = http.createServer(async (req, res) => {
  const ip = ipOf(req);
  const url = new URL(req.url, "http://localhost");
  const path = url.pathname;

  if (req.method === "GET" && path === "/health") {
    return ok(res, { ok: true });
  }
  if (req.method !== "POST" || !path.startsWith("/v1/")) {
    return deny(res);
  }
  if (!ROUTES.has(path)) return deny(res);
  // Tarpitted IPs are not denied on ask: handleAsk answers with a fake
  // `pending` instead, so scanners cannot tell a live room from a dead one.
  if (isIgnored(ip) && !(path === "/v1/viewer/ask" && isTarpitted(ip))) {
    log("warn", ip, "ignored", path.slice(4));
    return deny(res);
  }
  const route = path.slice(4);
  const limit = RATE_LIMITS[route] ?? RATE_LIMITS.rest;
  if (!rateLimit(ip, route, limit)) {
    log("warn", ip, "rate", route, "-", "busy");
    return busy(res);
  }

  const { body, error } = await readBody(req);
  if (error === "too_large") {
    res.writeHead(413, { "Content-Type": "application/json", "Connection": "close" });
    res.end(JSON.stringify({ ok: false, error: "invalid" }), () => req.socket.destroy());
    return;
  }
  if (error === "read_error") return deny(res);

  let json;
  try {
    json = JSON.parse(body);
  } catch {
    return invalid(res);
  }
  if (!json || typeof json !== "object") return invalid(res);

  try {
    switch (path) {
      case "/v1/host/open":
        return await handleOpen(ip, json, res);
      case "/v1/host/heartbeat":
        return handleHeartbeat(ip, json, res);
      case "/v1/host/rotate":
        return await handleRotate(ip, json, res);
      case "/v1/host/close":
        return handleClose(ip, json, res);
      case "/v1/viewer/ask":
        return await handleAsk(ip, json, res);
      case "/v1/member/leave":
        handleMemberLeave(ip, json, res);
        break;
      case "/v1/member/heartbeat":
        handleMemberHeartbeat(ip, json, res);
        break;
      case "/v1/master/kick":
        handleMasterKick(ip, json, res);
        break;
      case "/v1/host/decide":
        return handleDecide(ip, json, res);
      default:
        return deny(res);
    }
  } catch (err) {
    log("warn", ip, "error", route, "-", err.message);
    return deny(res);
  }
});

server.headersTimeout = HEADER_TIMEOUT_MS;
server.requestTimeout = REQUEST_TIMEOUT_MS;
server.on("clientError", (_err, socket) => socket.destroy());

server.on("upgrade", (req, socket, head) => {
  const ip = ipOf(req);
  if (isIgnored(ip) && !isTarpitted(ip)) {
    socket.destroy();
    return;
  }
  if (!rateLimit(ip, "ws", RATE_LIMITS.ws)) {
    socket.write("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  const url = new URL(req.url, "http://localhost");
  const role = url.searchParams.get("role");
  const token = url.searchParams.get("token");
  if (wss.clients.size >= MAX_WS) {
    socket.write("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  if (fakeTokens.has(token)) {
    // Tarpit token: accept the socket, it carries nothing and dies at 60s.
    wss.handleUpgrade(req, socket, head, (ws) => {
      wss.emit("connection", ws, req, { role: "tarpit", token });
    });
    return;
  }
  const entry = tokens.get(token);
  if (!entry || entry.role !== role) {
    socket.write("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    wss.emit("connection", ws, req, { role, token, code: entry.code, viewerId: entry.viewerId });
  });
});

// Keep proxies alive: WS ping/pong every 30s. Dead sockets are terminated.
setInterval(() => {
  for (const ws of wss.clients) {
    if (!ws.isAlive) {
      ws.terminate();
      continue;
    }
    ws.isAlive = false;
    ws.ping();
  }
}, WS_PING_MS);

// GC: rooms whose Host stopped heartbeating die after 5 minutes. Also prunes
// the rate/ignore maps so a long-lived process stays bounded.
setInterval(() => {
  const now = Date.now();
  for (const [code, room] of rooms) {
    if (room.mode === "room") {
      for (const member of [...room.members.values()]) {
        if (now - member.heartbeatAt > HEARTBEAT_TTL_MS) {
          log("warn", "-", "gc-member", code, member.id);
          dropMember(room, member.id);
        }
      }
      continue;
    }
    if (now - room.heartbeatAt > HEARTBEAT_TTL_MS) {
      log("warn", "-", "gc", code);
      destroyRoom(room);
    }
  }
  for (const [ip, byRoute] of rate) {
    for (const [route, hits] of byRoute) {
      while (hits.length && now - hits[0] > RATE_WINDOW_MS) hits.shift();
      if (!hits.length) byRoute.delete(route);
    }
    if (!byRoute.size) rate.delete(ip);
  }
  for (const [ip, entry] of ignore) {
    entry.fails = entry.fails.filter((t) => now - t < IGNORE_WINDOW_MS);
    if (now >= entry.until && !entry.fails.length) ignore.delete(ip);
  }
}, GC_INTERVAL_MS);

// Tarpit tokens are short-lived: 2 min TTL, swept every 30s.
setInterval(() => {
  const now = Date.now();
  for (const [token, createdAt] of fakeTokens) {
    if (now - createdAt > FAKE_TOKEN_TTL_MS) fakeTokens.delete(token);
  }
}, FAKE_TOKEN_GC_MS);

server.listen(PORT, BIND, () => {
  log("info", "-", "listen", `${BIND}:${PORT}`);
});

function shutdown() {
  log("info", "-", "shutdown");
  for (const ws of wss.clients) ws.terminate();
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 1000).unref();
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);