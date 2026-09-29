// Sala signaling server — the whole server in one file.
//
// RAM only. node:http + ws + node:crypto. No DB, no media, no TURN.
// Protocol: server/PROTOCOL.md.
//
//   PORT=18790 BIND=127.0.0.1 node server.mjs   (PORT=0 picks an ephemeral port)
//   Docker (behind a reverse proxy): BIND=0.0.0.0 PORT=8787 node server.mjs
//
// Signaling-only: this process routes JSON envelopes. It never opens UDP
// sockets, never terminates media, never logs passwords, tokens, SDP or
// ICE candidates.

import http from "node:http";
import { isIP } from "node:net";
import { randomBytes, scrypt, timingSafeEqual } from "node:crypto";
import { WebSocketServer } from "ws";
import { createDiagnostics } from "./diagnostics.mjs";

const PORT = Number(process.env.PORT || 18790);
const BIND = process.env.BIND || "127.0.0.1";

function normalizeIp(value) {
  if (typeof value !== "string" || value.length === 0 || value.includes("%") || value.trim() !== value) return null;
  const version = isIP(value);
  if (version === 4) return value;
  if (version !== 6) return null;

  let address = value.toLowerCase();
  const embeddedV4 = address.match(/(?:^|:)(\d+\.\d+\.\d+\.\d+)$/);
  if (embeddedV4) {
    const octets = embeddedV4[1].split(".").map(Number);
    address = address.slice(0, -embeddedV4[1].length) + `${((octets[0] << 8) | octets[1]).toString(16)}:${((octets[2] << 8) | octets[3]).toString(16)}`;
  }
  const halves = address.split("::");
  const left = halves[0] ? halves[0].split(":") : [];
  const right = halves.length > 1 && halves[1] ? halves[1].split(":") : [];
  const groups = halves.length === 1
    ? left
    : [...left, ...Array(8 - left.length - right.length).fill("0"), ...right];
  if (groups.length !== 8) return null;
  const bytes = groups.flatMap((group) => {
    const n = Number.parseInt(group, 16);
    return [(n >> 8) & 0xff, n & 0xff];
  });
  if (bytes.slice(0, 10).every((byte) => byte === 0) && bytes[10] === 255 && bytes[11] === 255) {
    return bytes.slice(12).join(".");
  }
  return bytes.map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

const trustedProxyEnv = process.env.TRUSTED_PROXY_PEER;
const trustedProxyPeer = trustedProxyEnv === undefined ? null : normalizeIp(trustedProxyEnv);
if (trustedProxyEnv !== undefined && !trustedProxyPeer) {
  console.error("invalid TRUSTED_PROXY_PEER");
  process.exit(1);
}

// Default is loopback. Docker sets BIND=0.0.0.0 (or ::) behind a reverse proxy.
// Binding a specific public address is still refused.
if (!/^(127\.\d+\.\d+\.\d+|::1|::ffff:127\.\d+\.\d+\.\d+|localhost|0\.0\.0\.0|::)$/.test(BIND)) {
  console.error("refusing non-local bind");
  process.exit(1);
}

const MAX_ROOMS = 256;
const MAX_ACTIVE_SCRYPT = 4;
const MAX_ACCEPTED = 8;
const MAX_PENDING = 8;
const MAX_WS = 512;
const HEARTBEAT_TTL_MS = Number(process.env.HEARTBEAT_TTL_MS || 5 * 60 * 1000);
const GC_INTERVAL_MS = Number(process.env.GC_INTERVAL_MS || 15 * 1000);
const DISCONNECT_GRACE_MS = Number(process.env.DISCONNECT_GRACE_MS || 8 * 1000);
const BODY_LIMIT = 64 * 1024;
const MAX_CANDIDATE_BYTES = 8 * 1024;
const MAX_CANDIDATES_PER_ATTEMPT = 64;
const MAX_SIGNAL_LINKS_PER_ROOM = 256;
const MAX_SIGNAL_LINKS_GLOBAL = 4096;
const MAX_CANDIDATE_KEYS_PER_ROOM = 256;
const MAX_CANDIDATE_KEYS_GLOBAL = 8192;
const WS_MESSAGE_BURST = 1024;
const WS_MESSAGE_RATE = 256;
const WS_BYTE_BURST = 8 * 1024 * 1024;
const WS_BYTE_RATE = 1024 * 1024;
const WS_CONTROL_BURST = 32;
const WS_CONTROL_RATE = 8;
const WS_OUTBOUND_MAX_BUFFERED = 2 * 1024 * 1024;
const WS_OUTBOUND_GLOBAL_MAX_BUFFERED = 32 * 1024 * 1024;
const WS_GLOBAL_MESSAGE_BURST = 8192;
const WS_GLOBAL_MESSAGE_RATE = 2048;
const WS_GLOBAL_BYTE_BURST = 32 * 1024 * 1024;
const WS_GLOBAL_BYTE_RATE = 4 * 1024 * 1024;
const HEARTBEAT_ROSTER_INTERVAL_MS = 250;
const HEADER_TIMEOUT_MS = 5 * 1000;
const REQUEST_TIMEOUT_MS = 15 * 1000;
const WS_PING_MS = 30 * 1000;
const RATE_WINDOW_MS = 60 * 1000;
const RATE_LIMITS = { create: 10, join: 20, leave: 30, ws: 30, rest: 60 };
const IGNORE_WINDOW_MS = 10 * 60 * 1000;
const IGNORE_AFTER_FAILS = 5;
const IGNORE_FOR_MS = 5 * 60 * 1000;
const NICK_RE = /^[A-Za-z0-9 _\-.]+$/;
const SIGNAL_TYPES = new Set(["offer", "answer", "candidate", "ice-complete"]);

// --- State (RAM) -----------------------------------------------------------

/** code -> { code, passwordHash, passwordSalt, admission, masterId, members: Map } */
const rooms = new Map();
/** token -> { code, memberId } */
const tokens = new Map();
/** ip -> Map(route -> timestamps) */
const rate = new Map();
/** ip -> { fails: number[], until: number } */
const ignore = new Map();
/** linkKey -> { attempt, code, fromId, toId } */
const attempts = new Map();
/** linkKey+attempt -> { count, code, fromId, toId, attempt } */
const candidateCounts = new Map();
const signalLinksByRoom = new Map();
const candidateKeysByRoom = new Map();
let activeScrypt = 0;
let reservedRoomCreates = 0;
const reservedRoomCodes = new Set();

// Member = { id, nickname, token, state: "pending"|"accepted",
//            joinedAt, heartbeatAt, ws, share }

// --- Helpers ---------------------------------------------------------------

function ipOf(req) {
  const ip = req.socket.remoteAddress || "-";
  return normalizeIp(ip) || ip;
}

function requestIp(req) {
  const peer = ipOf(req);
  if (!trustedProxyPeer || peer !== trustedProxyPeer) return { ip: peer, invalid: false };

  const values = [];
  for (let i = 0; i < req.rawHeaders.length; i += 2) {
    if (req.rawHeaders[i].toLowerCase() === "x-real-ip") values.push(req.rawHeaders[i + 1]);
  }
  if (values.length !== 1) return { ip: null, invalid: true };
  const forwardedIp = normalizeIp(values[0]);
  return forwardedIp ? { ip: forwardedIp, invalid: false } : { ip: null, invalid: true };
}

// Legacy call sites may pass identities; never persist or print those arguments.
const diagnostics = createDiagnostics(process.env.DIAGNOSTIC_LOG_PATH || "");
function log(_level, _ip, event, _code, _member, result) {
  if (event === "ws") diagnostics.record("websocket", "connected");
  if (event === "gc-room") diagnostics.record("room", "removed");
  if (event === "rate") diagnostics.record("pressure", "request_limit");
  if (event === "ignored") diagnostics.record("auth", "ignored");
  if (event === "join" && (result === "denied" || result === "full")) diagnostics.record("auth", result);
}

function randomToken() {
  return randomBytes(32).toString("hex");
}

function randomMemberId() {
  return randomBytes(4).toString("hex");
}

function randomCode() {
  return (randomBytes(4).readUInt32BE(0) & 0x7fffffff).toString(36).toUpperCase().padStart(6, "0");
}

function scryptAsync(password, salt) {
  return new Promise((resolve, reject) => {
    scrypt(password, salt, 32, { N: 16384, r: 8, p: 1 }, (err, key) => {
      if (err) reject(err);
      else resolve(key);
    });
  });
}

function acquireScrypt() {
  if (activeScrypt >= MAX_ACTIVE_SCRYPT) return false;
  activeScrypt += 1;
  return true;
}

function releaseScrypt() {
  activeScrypt -= 1;
}

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const normalizeCode = (code) => (typeof code === "string" ? code.trim().toUpperCase() : "");

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

function isSignalId(value) {
  if (typeof value === "string") return value.length >= 1 && value.length <= 256;
  if (typeof value === "number") return Number.isInteger(value) && value >= 0;
  return false;
}

function hasExactKeys(obj, keys) {
  const actual = Object.keys(obj);
  return actual.length === keys.length && keys.every((k) => Object.hasOwn(obj, k));
}

function sendJson(res, status, obj) {
  res.writeHead(status, { "Content-Type": "application/json", Connection: "close" });
  res.end(JSON.stringify(obj));
}
const ok = (res, obj) => sendJson(res, 200, obj);
const deny = (res) => sendJson(res, 404, { ok: false, error: "denied" });
const invalid = (res) => sendJson(res, 400, { ok: false, error: "invalid" });
const busy = (res) => sendJson(res, 429, { ok: false, error: "busy" });
const full = (res) => sendJson(res, 429, { ok: false, error: "full" });

function send(ws, obj) {
  if (!ws || ws.readyState !== 1) return;
  const data = JSON.stringify(obj);
  const bytes = Buffer.byteLength(data);
  let globallyBuffered = 0;
  for (const client of wss.clients) globallyBuffered += client.bufferedAmount;
  if (ws.bufferedAmount + bytes > WS_OUTBOUND_MAX_BUFFERED ||
      globallyBuffered + bytes > WS_OUTBOUND_GLOBAL_MAX_BUFFERED) {
    diagnostics.record("pressure", ws.bufferedAmount + bytes > WS_OUTBOUND_MAX_BUFFERED ? "outbound_limit" : "global_limit");
    ws.terminate();
    return;
  }
  ws.send(data, (error) => {
    if (error && ws.readyState === 1) {
      diagnostics.record("websocket", "send_error");
      ws.terminate();
    }
  });
}

// --- Rate limit + ignore list ----------------------------------------------

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
  if (!entry.fails.length) ignore.delete(ip);
  return false;
}

function noteAuthFailure(ip) {
  const now = Date.now();
  let entry = ignore.get(ip);
  if (!entry) {
    entry = { fails: [], until: 0 };
    ignore.set(ip, entry);
  }
  entry.fails = entry.fails.filter((t) => now - t < IGNORE_WINDOW_MS);
  entry.fails.push(now);
  if (entry.fails.length >= IGNORE_AFTER_FAILS) {
    entry.until = now + IGNORE_FOR_MS;
    entry.fails = [];
  }
}

// --- Room helpers ----------------------------------------------------------

function rosterEntries(room) {
  return [...room.members.values()]
    .filter((m) => m.state === "accepted")
    .map((m) => ({
      id: m.id,
      nickname: m.nickname,
      master: m.id === room.masterId,
      share: m.share === true,
    }));
}

function broadcastRoster(room) {
  const msg = { t: "roster", entries: rosterEntries(room), master_id: room.masterId };
  for (const member of room.members.values()) {
    if (member.state === "accepted") send(member.ws, msg);
  }
}

/** Oldest remaining joinedAt wins; lexical member id breaks ties. */
function nextMaster(room, leavingId) {
  const rest = [...room.members.values()]
    .filter((m) => m.state === "accepted" && m.id !== leavingId)
    .sort((a, b) => (a.joinedAt !== b.joinedAt ? a.joinedAt - b.joinedAt : a.id < b.id ? -1 : 1));
  return rest[0] ? rest[0].id : null;
}

function purgeSignalState(code) {
  for (const [key, entry] of attempts) {
    if (entry.code === code) attempts.delete(key);
  }
  for (const [key, entry] of candidateCounts) {
    if (entry.code === code) candidateCounts.delete(key);
  }
  signalLinksByRoom.delete(code);
  candidateKeysByRoom.delete(code);
}

function purgeMemberSignalState(code, memberId) {
  for (const [key, entry] of attempts) {
    if (entry.code === code && (entry.fromId === memberId || entry.toId === memberId)) {
      attempts.delete(key);
      signalLinksByRoom.set(code, Math.max(0, (signalLinksByRoom.get(code) ?? 1) - 1));
    }
  }
  for (const [key, entry] of candidateCounts) {
    if (entry.code === code && (entry.fromId === memberId || entry.toId === memberId)) {
      candidateCounts.delete(key);
      candidateKeysByRoom.set(code, Math.max(0, (candidateKeysByRoom.get(code) ?? 1) - 1));
    }
  }
  if (signalLinksByRoom.get(code) === 0) signalLinksByRoom.delete(code);
  if (candidateKeysByRoom.get(code) === 0) candidateKeysByRoom.delete(code);
}

/** Removes a member, invalidates its token, closes its socket. */
function removeMember(room, memberId, reason) {
  const member = room.members.get(memberId);
  if (!member) return;
  clearTimeout(member.disconnectTimer);
  member.disconnectTimer = null;
  const wasMaster = room.masterId === memberId;
  room.members.delete(memberId);
  purgeMemberSignalState(room.code, memberId);
  tokens.delete(member.token);
  send(member.ws, { t: reason });
  if (member.ws) member.ws.close(4000, reason);
  if (room.members.size === 0 || ![...room.members.values()].some((m) => m.state === "accepted")) {
    // Nobody left to hold the room (pending-only rooms never existed).
    if ([...room.members.values()].length === 0) {
      rooms.delete(room.code);
      purgeSignalState(room.code);
      log("info", "-", "gc-room", room.code);
      return;
    }
  }
  if (wasMaster) {
    const next = nextMaster(room, memberId);
    if (next) {
      room.masterId = next;
      log("info", "-", "succession", room.code, next);
    } else {
      // Only pending members remain: the room never activated; drop it.
      for (const m of room.members.values()) {
        tokens.delete(m.token);
        send(m.ws, { t: "gone" });
        if (m.ws) m.ws.close(4000, "gone");
      }
      rooms.delete(room.code);
      purgeSignalState(room.code);
      log("info", "-", "gc-room", room.code);
      return;
    }
  }
  broadcastRoster(room);
}

// --- Signal envelope validation (never parses SDP/candidate semantics) -----

function validOfferAnswer(payload) {
  if (!hasExactKeys(payload, ["type", "session", "share", "link", "attempt", "sdp"])) return false;
  if (!isSignalId(payload.session) || !isSignalId(payload.share)) return false;
  if (!isSignalId(payload.link) || !isSignalId(payload.attempt)) return false;
  if (typeof payload.sdp !== "string") return false;
  return Buffer.byteLength(payload.sdp, "utf8") <= BODY_LIMIT;
}

function validCandidate(payload) {
  if (!hasExactKeys(payload, ["type", "session", "share", "link", "attempt", "candidate"])) {
    return false;
  }
  if (!isSignalId(payload.session) || !isSignalId(payload.share)) return false;
  if (!isSignalId(payload.link) || !isSignalId(payload.attempt)) return false;
  if (typeof payload.candidate !== "string") return false;
  const bytes = Buffer.byteLength(payload.candidate, "utf8");
  if (bytes < 1 || bytes > MAX_CANDIDATE_BYTES) return false;
  // TURN is outside the product: reject relay candidates. Envelope policy
  // filter, not semantic parsing.
  if (payload.candidate.toLowerCase().includes("typ relay")) return false;
  return true;
}

function validIceComplete(payload) {
  if (!hasExactKeys(payload, ["type", "session", "share", "link", "attempt"])) return false;
  if (!isSignalId(payload.session) || !isSignalId(payload.share)) return false;
  if (!isSignalId(payload.link) || !isSignalId(payload.attempt)) return false;
  return true;
}

function validSignalEnvelope(payload) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) return false;
  if (payload.type === "offer" || payload.type === "answer") return validOfferAnswer(payload);
  if (payload.type === "candidate") return validCandidate(payload);
  if (payload.type === "ice-complete") return validIceComplete(payload);
  return false;
}

function linkKey(code, fromId, toId, payload) {
  const pair = [String(fromId), String(toId)].sort();
  return JSON.stringify([code, pair[0], pair[1], payload.session, payload.share, payload.link]);
}

function incrementRoomCount(map, code) {
  map.set(code, (map.get(code) ?? 0) + 1);
}

function decrementRoomCount(map, code) {
  const next = (map.get(code) ?? 0) - 1;
  if (next > 0) map.set(code, next);
  else map.delete(code);
}

// Offers establish the current attempt; candidate-before-offer remains valid.
function attemptAllowed(code, fromId, toId, key, payload) {
  const attempt = String(payload.attempt);
  if (payload.type === "offer") {
    const current = attempts.get(key);
    if (current && attempt !== current.attempt) {
      const incoming = Number(attempt);
      const known = Number(current.attempt);
      if (Number.isFinite(incoming) && Number.isFinite(known)) {
        if (incoming < known) return false;
      }
    }
    if (!current) {
      if ((signalLinksByRoom.get(code) ?? 0) >= MAX_SIGNAL_LINKS_PER_ROOM ||
          attempts.size >= MAX_SIGNAL_LINKS_GLOBAL) return "full";
      incrementRoomCount(signalLinksByRoom, code);
    }
    attempts.set(key, { attempt, code, fromId, toId });
    return true;
  }
  const current = attempts.get(key);
  if (current && attempt !== current.attempt) return false;
  if (payload.type === "candidate") {
    const countKey = JSON.stringify([key, attempt]);
    const entry = candidateCounts.get(countKey);
    if ((entry?.count ?? 0) >= MAX_CANDIDATES_PER_ATTEMPT) return false;
    if (!entry) {
      if ((candidateKeysByRoom.get(code) ?? 0) >= MAX_CANDIDATE_KEYS_PER_ROOM ||
          candidateCounts.size >= MAX_CANDIDATE_KEYS_GLOBAL) return "full";
      incrementRoomCount(candidateKeysByRoom, code);
      candidateCounts.set(countKey, { count: 1, code, fromId, toId, attempt, linkKey: key });
    } else {
      entry.count += 1;
    }
  }
  return true;
}

// --- REST ------------------------------------------------------------------

async function handleCreate(ip, json, res) {
  if (!validNickname(json.nickname)) return invalid(res);
  if (!validPassword(json.password)) return invalid(res);
  if (!acquireScrypt()) return busy(res);
  let reserved = false;
  let code = null;
  try {
    if (rooms.size + reservedRoomCreates >= MAX_ROOMS) return busy(res);
    reservedRoomCreates += 1;
    reserved = true;
    for (let i = 0; i < 10; i += 1) {
      const c = randomCode();
      if (!rooms.has(c) && !reservedRoomCodes.has(c)) {
        code = c;
        reservedRoomCodes.add(c);
        break;
      }
    }
    if (!code) return busy(res);
    const salt = randomBytes(16);
    const hash = await scryptAsync(json.password, salt);
    const memberId = randomMemberId();
    const token = randomToken();
    const now = Date.now();
    const room = {
      code,
      passwordHash: hash,
      passwordSalt: salt,
      admission: json.admission === true,
      masterId: memberId,
      createdAt: now,
      members: new Map(),
    };
    room.members.set(memberId, {
      id: memberId,
      nickname: json.nickname.trim(),
      token,
      state: "accepted",
      joinedAt: now,
      heartbeatAt: now,
      ws: null,
      share: false,
    });
    // A reserved code cannot normally be taken; guard the commit regardless.
    if (rooms.has(code)) return busy(res);
    rooms.set(code, room);
    tokens.set(token, { code, memberId });
    log("info", ip, "create", code, memberId);
    ok(res, { ok: true, code, memberId, token });
  } finally {
    if (reserved) reservedRoomCreates -= 1;
    if (code) reservedRoomCodes.delete(code);
    releaseScrypt();
  }
}

async function handleJoin(ip, code, json, res) {
  const room = rooms.get(code);
  const password = typeof json.password === "string" ? json.password : "";
  if (!acquireScrypt()) return busy(res);
  try {
  if (!room) {
    // Indistinguishable from wrong password: same cost, same delay, same deny.
    await scryptAsync("", randomBytes(16));
    await delay(50 + Math.random() * 30);
    noteAuthFailure(ip);
    log("warn", ip, "join", code, "-", "denied");
    return deny(res);
  }
  if (!validNickname(json.nickname)) return invalid(res);
  const hash = await scryptAsync(password, room.passwordSalt);
  if (!timingSafeEqual(hash, room.passwordHash)) {
    await delay(50 + Math.random() * 30);
    noteAuthFailure(ip);
    log("warn", ip, "join", code, "-", "denied");
    return deny(res);
  }
  // The room may have been removed while password verification was pending.
  if (rooms.get(code) !== room) return deny(res);
  let accepted = 0;
  let pending = 0;
  for (const m of room.members.values()) {
    if (m.state === "accepted") accepted += 1;
    else pending += 1;
  }
  if (accepted >= MAX_ACCEPTED || pending >= MAX_PENDING) {
    log("warn", ip, "join", code, "-", "full");
    return full(res);
  }
  const memberId = randomMemberId();
  const token = randomToken();
  const now = Date.now();
  const state = room.admission ? "pending" : "accepted";
  room.members.set(memberId, {
    id: memberId,
    nickname: json.nickname.trim(),
    token,
    state,
    joinedAt: now,
    heartbeatAt: now,
    ws: null,
    share: false,
  });
  tokens.set(token, { code, memberId });
  log("info", ip, "join", code, memberId);
  if (state === "pending") {
    const master = room.members.get(room.masterId);
    send(master?.ws, { t: "pending", member_id: memberId, nickname: json.nickname.trim() });
  }
  broadcastRoster(room);
  ok(res, { ok: true, status: state, memberId, token });
  } finally {
    releaseScrypt();
  }
}

function handleLeave(ip, code, json, res) {
  const token = typeof json.token === "string" ? json.token : "";
  const entry = tokens.get(token);
  if (!entry || entry.code !== code) return deny(res);
  const room = rooms.get(code);
  if (!room) return deny(res);
  removeMember(room, entry.memberId, "gone");
  ok(res, { ok: true });
}

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
  try {
  const { ip, invalid: invalidIdentity } = requestIp(req);
  if (invalidIdentity) return invalid(res);
  let url;
  try {
    url = new URL(req.url, "http://localhost");
  } catch {
    return invalid(res);
  }
  const path = url.pathname;

  if (req.method === "GET" && path === "/health") {
    return ok(res, { ok: true });
  }
  if (req.method !== "POST") return deny(res);
  if (isIgnored(ip)) {
    log("warn", ip, "ignored");
    return deny(res);
  }

  let route = "rest";
  let m;
  if (path === "/v1/rooms") route = "create";
  else if ((m = path.match(/^\/v1\/rooms\/([A-Za-z0-9]+)\/(join|leave)$/))) route = m[2];
  else return deny(res);

  if (!rateLimit(ip, route, RATE_LIMITS[route] ?? RATE_LIMITS.rest)) {
    log("warn", ip, "rate", route);
    return busy(res);
  }

  const { body, error } = await readBody(req);
  if (error === "too_large") {
    res.writeHead(413, { "Content-Type": "application/json", Connection: "close" });
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

  if (route === "create") return await handleCreate(ip, json, res);
  const code = normalizeCode(m[1]);
  if (route === "join") return await handleJoin(ip, code, json, res);
  return handleLeave(ip, code, json, res);
  } catch {
    // Async request handlers must never reject into Node's unhandled-rejection path.
    if (!res.headersSent && !res.destroyed) deny(res);
    else if (!res.destroyed) res.destroy();
  }
});

server.headersTimeout = HEADER_TIMEOUT_MS;
server.requestTimeout = REQUEST_TIMEOUT_MS;
server.on("clientError", (_err, socket) => socket.destroy());

// --- WebSocket -------------------------------------------------------------

const wss = new WebSocketServer({
  noServer: true,
  maxPayload: BODY_LIMIT,
  maxFragments: 32,
  maxBufferedChunks: 1024,
  perMessageDeflate: false,
  autoPong: false,
});

function makeTokenBucket(capacity, rate, now = Date.now()) {
  return { tokens: capacity, capacity, rate, updatedAt: now };
}

function takeToken(bucket, amount = 1, now = Date.now()) {
  const elapsed = Math.max(0, now - bucket.updatedAt) / 1000;
  bucket.tokens = Math.min(bucket.capacity, bucket.tokens + elapsed * bucket.rate);
  bucket.updatedAt = now;
  if (bucket.tokens < amount) return false;
  bucket.tokens -= amount;
  return true;
}

// Process-wide ingress budget, shared by every socket. Charge before parsing;
// a single abusive connection cannot multiply the per-socket allowance.
const globalWsMessageBucket = makeTokenBucket(WS_GLOBAL_MESSAGE_BURST, WS_GLOBAL_MESSAGE_RATE);
const globalWsByteBucket = makeTokenBucket(WS_GLOBAL_BYTE_BURST, WS_GLOBAL_BYTE_RATE);

function takeGlobalWsTokens(bytes, now = Date.now()) {
  return takeToken(globalWsMessageBucket, 1, now) && takeToken(globalWsByteBucket, bytes, now);
}

function wsDataBytes(data) {
  return Buffer.isBuffer(data) ? data.length : Buffer.byteLength(String(data));
}

function handleSignal(room, member, msg) {
  const toId = typeof msg.to === "string" ? msg.to : "";
  const dest = room.members.get(toId);
  if (!dest || dest.state !== "accepted" || dest.id === member.id) {
    diagnostics.record("signal", "rejected");
    send(member.ws, { t: "error", error: "forbidden" });
    return;
  }
  if (!validSignalEnvelope(msg.payload)) {
    diagnostics.record("signal", "rejected");
    send(member.ws, { t: "error", error: "invalid" });
    return;
  }
  const key = linkKey(room.code, member.id, dest.id, msg.payload);
  const allowed = attemptAllowed(room.code, member.id, dest.id, key, msg.payload);
  if (allowed === "full") {
    diagnostics.record("signal", "rejected");
    send(member.ws, { t: "error", error: "full" });
    return;
  }
  if (!allowed) {
    diagnostics.record("signal", "rejected");
    send(member.ws, { t: "error", error: "stale" });
    return;
  }
  if (!dest.ws || dest.ws.readyState !== 1) {
    diagnostics.record("signal", "target_offline");
    return;
  }
  send(dest.ws, { t: "signal", from: member.id, to: dest.id, payload: msg.payload });
  diagnostics.record("signal", msg.payload.type === "offer" ? "offer" : msg.payload.type === "answer" ? "answer" : "forwarded");
}

function handleWsMessage(room, member, ws, raw) {
  if (member.ws !== ws || room.members.get(member.id) !== member) return;
  let msg;
  try {
    msg = JSON.parse(raw.toString());
  } catch {
    send(member.ws, { t: "error", error: "invalid" });
    return;
  }
  if (!msg || typeof msg.t !== "string") {
    send(member.ws, { t: "error", error: "invalid" });
    return;
  }
  member.heartbeatAt = Date.now();

  if (msg.t === "heartbeat") {
    const now = Date.now();
    if (now - (ws.lastHeartbeatRosterAt ?? 0) >= HEARTBEAT_ROSTER_INTERVAL_MS) {
      ws.lastHeartbeatRosterAt = now;
      send(ws, { t: "roster", entries: rosterEntries(room), master_id: room.masterId });
    }
    return;
  }
  if (member.state !== "accepted") {
    send(member.ws, { t: "error", error: "forbidden" });
    return;
  }
  if (msg.t === "announce-share" || msg.t === "stop-share") {
    const share = msg.t === "announce-share";
    if (member.share === share) return;
    member.share = share;
    diagnostics.record("share", share ? "announced" : "stopped");
    log("info", "-", "share", room.code, member.id);
    broadcastRoster(room);
    return;
  }
  if (msg.t === "watch" || msg.t === "unwatch") {
    const dest = typeof msg.to === "string" ? room.members.get(msg.to) : null;
    if (!dest || dest.state !== "accepted" || dest.id === member.id) {
      diagnostics.record("watch", "rejected");
      send(member.ws, { t: "error", error: "forbidden" });
      return;
    }
    if (!dest.ws || dest.ws.readyState !== 1) {
      diagnostics.record("watch", "target_offline");
      return;
    }
    send(dest.ws, { t: msg.t, from: member.id, to: dest.id });
    if (msg.t === "watch") diagnostics.record("watch", "forwarded");
    return;
  }
  if (SIGNAL_TYPES.has(msg.t)) {
    handleSignal(room, member, msg);
    return;
  }
  if (msg.t === "kick") {
    if (member.id !== room.masterId) {
      send(member.ws, { t: "error", error: "forbidden" });
      return;
    }
    const target = typeof msg.target === "string" ? room.members.get(msg.target) : null;
    if (!target || target.id === room.masterId) {
      send(member.ws, { t: "error", error: "invalid" });
      return;
    }
    removeMember(room, target.id, "kicked");
    send(member.ws, { t: "ok" });
    return;
  }
  if (msg.t === "admit") {
    if (member.id !== room.masterId) {
      send(member.ws, { t: "error", error: "forbidden" });
      return;
    }
    const target = typeof msg.member === "string" ? room.members.get(msg.member) : null;
    if (!target || target.state !== "pending") {
      send(member.ws, { t: "error", error: "invalid" });
      return;
    }
    if (msg.accept === true) {
      const accepted = [...room.members.values()].filter((m) => m.state === "accepted").length;
      if (accepted >= MAX_ACCEPTED) {
        send(member.ws, { t: "error", error: "full" });
        return;
      }
      target.state = "accepted";
      target.heartbeatAt = Date.now();
      log("info", "-", "admit", room.code, target.id);
      send(target.ws, { t: "admitted", member_id: target.id });
      broadcastRoster(room);
    } else {
      removeMember(room, target.id, "gone");
    }
    send(member.ws, { t: "ok" });
    return;
  }
  send(member.ws, { t: "error", error: "invalid" });
}

wss.on("connection", (ws, req, meta) => {
  const ip = ipOf(req);
  const room = rooms.get(meta.code);
  const member = room ? room.members.get(meta.memberId) : null;
  if (!room || !member || member.token !== meta.token) {
    ws.close(4000, "gone");
    return;
  }
  if (member.ws && member.ws !== ws) {
    diagnostics.record("websocket", "replaced");
    try {
      member.ws.close(4000, "replaced");
    } catch {
      // ignore close errors on the stale socket
    }
  }
  member.ws = ws;
  clearTimeout(member.disconnectTimer);
  member.disconnectTimer = null;
  member.connectionGeneration = (member.connectionGeneration ?? 0) + 1;
  member.heartbeatAt = Date.now();
  ws.isAlive = true;
  ws.on("pong", (data) => {
    const now = Date.now();
    if (!takeToken(ws.pongBucket, 1, now) || !takeGlobalWsTokens(data.length, now)) {
      diagnostics.record("pressure", "control_limit");
      ws.terminate();
      return;
    }
    if (ws.isAlive === false) ws.isAlive = true;
  });
  ws.on("ping", (data) => {
    const now = Date.now();
    if (!takeToken(ws.pingBucket, 1, now) || !takeGlobalWsTokens(data.length, now) ||
        ws.bufferedAmount + data.length + 2 > WS_OUTBOUND_MAX_BUFFERED) {
      diagnostics.record("pressure", "inbound_limit");
      ws.terminate();
      return;
    }
    ws.pong(data, undefined, (error) => {
      if (error && ws.readyState === 1) {
        diagnostics.record("websocket", "send_error");
        ws.terminate();
      }
    });
  });
  ws.on("error", () => {});
  if (member.state === "pending") {
    send(ws, { t: "pending", member_id: member.id });
  } else {
    send(ws, { t: "admitted", member_id: member.id });
    send(ws, { t: "roster", entries: rosterEntries(room), master_id: room.masterId });
  }
  log("info", ip, "ws", room.code, member.id);
  const bucketNow = Date.now();
  ws.messageBucket = makeTokenBucket(WS_MESSAGE_BURST, WS_MESSAGE_RATE, bucketNow);
  ws.byteBucket = makeTokenBucket(WS_BYTE_BURST, WS_BYTE_RATE, bucketNow);
  ws.pingBucket = makeTokenBucket(WS_CONTROL_BURST, WS_CONTROL_RATE, bucketNow);
  ws.pongBucket = makeTokenBucket(WS_CONTROL_BURST, WS_CONTROL_RATE, bucketNow);
  ws.on("message", (data) => {
    if (member.ws !== ws || room.members.get(member.id) !== member) return;
    const now = Date.now();
    const bytes = wsDataBytes(data);
    if (!takeToken(ws.messageBucket, 1, now) || !takeToken(ws.byteBucket, bytes, now) ||
        !takeGlobalWsTokens(bytes, now)) {
      diagnostics.record("pressure", "inbound_limit");
      ws.terminate();
      return;
    }
    handleWsMessage(room, member, ws, data);
  });
  ws.on("close", () => {
    if (member.ws !== ws) return;
    diagnostics.record("websocket", "closed");
    member.ws = null;
    const generation = member.connectionGeneration;
    const memberId = member.id;
    const code = room.code;
    clearTimeout(member.disconnectTimer);
    member.disconnectTimer = setTimeout(() => {
      const currentRoom = rooms.get(code);
      if (!currentRoom) return;
      const current = currentRoom.members.get(memberId);
      if (current && current.connectionGeneration === generation && !current.ws) {
        current.disconnectTimer = null;
        diagnostics.record("grace_expiry", "expired");
        removeMember(currentRoom, memberId, "gone");
      }
    }, DISCONNECT_GRACE_MS);
    member.disconnectTimer.unref?.();
  });
});

server.on("upgrade", (req, socket, head) => {
  const { ip, invalid: invalidIdentity } = requestIp(req);
  if (invalidIdentity) {
    socket.write("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  let url;
  try {
    url = new URL(req.url, "http://localhost");
  } catch {
    socket.write("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  if (isIgnored(ip)) {
    socket.destroy();
    return;
  }
  if (!rateLimit(ip, "ws", RATE_LIMITS.ws)) {
    diagnostics.record("pressure", "upgrade_limit");
    socket.write("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  if (wss.clients.size >= MAX_WS) {
    diagnostics.record("pressure", "upgrade_limit");
    socket.write("HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  const token = url.searchParams.get("token") || "";
  const entry = tokens.get(token);
  if (!entry) {
    socket.write("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    wss.emit("connection", ws, req, { token, code: entry.code, memberId: entry.memberId });
  });
});

setInterval(() => {
  for (const ws of wss.clients) {
    if (!ws.isAlive) {
      diagnostics.record("websocket", "ping_timeout");
      ws.terminate();
      continue;
    }
    ws.isAlive = false;
    ws.ping(undefined, (error) => {
      if (error && ws.readyState === 1) {
        diagnostics.record("websocket", "send_error");
        ws.terminate();
      }
    });
  }
}, WS_PING_MS);

// GC: stale members are pruned (master departure triggers succession);
// empty rooms are destroyed; rate/ignore maps stay bounded.
setInterval(() => {
  const now = Date.now();
  for (const room of [...rooms.values()]) {
    for (const member of [...room.members.values()]) {
      if (now - member.heartbeatAt > HEARTBEAT_TTL_MS) {
        diagnostics.record("heartbeat_expiry", "expired");
        removeMember(room, member.id, "gone");
        if (!rooms.has(room.code)) break;
      }
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

server.listen(PORT, BIND, () => {
  const addr = server.address();
  const port = typeof addr === "object" && addr ? addr.port : PORT;
  diagnostics.record("startup", "started");
  diagnostics.flush();
  // BIND has been validated above; retain the address for IPv4/IPv6 test harnesses.
  console.log(`listen ${BIND}:${port}`);
});

function shutdown() {
  diagnostics.record("shutdown", "stopped");
  diagnostics.close();
  for (const ws of wss.clients) ws.terminate();
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 1000).unref();
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
