// Answer<->offer-attempt correlation over the Rendezvous wire.
// The server is an opaque pipe for attempt identities: `fence` and
// `offer_attempt` cross unchanged in both directions so Host and Viewer
// can correlate answers with the exact offer attempt. Pending Viewers
// never receive SDP, and kick prunes the roster without eating the cap.
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18794;
const base = `http://127.0.0.1:${PORT}`;

function post(path, body) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).then(async (res) => ({ status: res.status, json: await res.json() }));
}

function connect(role, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${PORT}/v1/ws?role=${role}&token=${token}`);
    const timer = setTimeout(() => reject(new Error(`${role} WS timeout`)), 4000);
    ws.once("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.once("error", reject);
  });
}

function waitForMessage(ws, predicate) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("message timeout")), 4000);
    ws.on("message", function onMessage(data) {
      const message = JSON.parse(String(data));
      if (predicate(message)) {
        clearTimeout(timer);
        ws.off("message", onMessage);
        resolve(message);
      }
    });
  });
}

const child = spawn(process.execPath, ["server.mjs"], {
  cwd: here,
  env: { ...process.env, PORT: String(PORT), BIND: "127.0.0.1" },
  stdio: ["ignore", "pipe", "pipe"],
});

await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("server start timeout")), 4000);
  child.stdout.on("data", (buf) => {
    if (String(buf).includes("listen")) {
      clearTimeout(timer);
      resolve();
    }
  });
  child.on("error", reject);
});

try {
  const open = await post("/v1/host/open", {
    nickname: "Ada",
    password: "secret1",
    mode: "broadcast",
    admission: true,
  });
  assert.equal(open.status, 200);
  const code = open.json.code;
  const hostToken = open.json.host_token;

  // Connect the host first so the live `pending` notice is not raced.
  const hostWs = await connect("host", hostToken);
  const pendingPromise = waitForMessage(hostWs, (msg) => msg.t === "pending");
  const ask = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(ask.json.status, "pending");
  const viewerId = ask.json.member_id;
  const pending = await pendingPromise;
  assert.equal(pending.viewer_id, viewerId);

  // Pending Viewers never get SDP: an offer sent before accept is dropped.
  hostWs.send(
    JSON.stringify({
      t: "signal",
      viewer_id: viewerId,
      payload: { type: "offer", sdp: "early", fence: { epoch: { session: 1, share: 1, link: 1 }, attempt: 1 } },
    }),
  );
  const viewerWs = await connect("viewer", ask.json.viewer_token);
  const early = [];
  viewerWs.on("message", (data) => early.push(JSON.parse(String(data))));
  await new Promise((resolve) => setTimeout(resolve, 500));
  assert.ok(
    !early.some((msg) => msg.t === "signal"),
    "pending viewer must not receive an offer",
  );

  // Attach before the decide POST: the WS `accepted` frame can arrive
  // before the HTTP response resolves.
  const acceptedPromise = waitForMessage(viewerWs, (msg) => msg.t === "accepted");
  const decide = await post("/v1/host/decide", {
    host_token: hostToken,
    viewer_id: viewerId,
    action: "accept",
  });
  assert.equal(decide.status, 200);
  await acceptedPromise;

  // Exact attempt identity crosses both directions unchanged.
  const fence = { epoch: { session: 4, share: 7, link: 9 }, attempt: 12 };
  hostWs.send(
    JSON.stringify({
      t: "signal",
      viewer_id: viewerId,
      payload: { type: "offer", sdp: "v=0\r\n", fence },
    }),
  );
  const offer = await waitForMessage(viewerWs, (msg) => msg.t === "signal" && msg.payload?.type === "offer");
  assert.deepEqual(offer.payload.fence, fence);

  const legacyAttempt = { offer_attempt: 12 };
  viewerWs.send(
    JSON.stringify({
      t: "signal",
      payload: { type: "answer", sdp: "v=0\r\n", fence, ...legacyAttempt },
    }),
  );
  const answer = await waitForMessage(hostWs, (msg) => msg.t === "signal" && msg.payload?.type === "answer");
  assert.deepEqual(answer.payload.fence, fence, "answer fence must cross unchanged");
  assert.equal(answer.payload.offer_attempt, 12, "opaque attempt must cross unchanged");
  assert.equal(answer.viewer_id, viewerId);

  // Kick prunes the roster entry; a fresh ask is admitted (cap not eaten).
  // Attach before the kick POST: `kicked` can arrive before the response.
  const kickedPromise = waitForMessage(viewerWs, (msg) => msg.t === "kicked");
  const kick = await post("/v1/host/decide", {
    host_token: hostToken,
    viewer_id: viewerId,
    action: "kick",
  });
  assert.equal(kick.status, 200);
  const kicked = await kickedPromise;
  assert.ok(kicked);
  const reask = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(reask.json.status, "pending", "kicked viewer must not eat the roster");

  hostWs.close();
  viewerWs.close();
  console.log("signaling correlation ok");
} finally {
  child.kill("SIGTERM");
}
