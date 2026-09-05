// Prepare/commit/abort regression coverage. A prepared Stunar bundle owns a
// code and security material, but is not a discoverable Room until commit.
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18793;
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
  env: {
    ...process.env,
    PORT: String(PORT),
    BIND: "127.0.0.1",
    PREPARED_TTL_MS: "1000",
    GC_INTERVAL_MS: "100",
  },
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
  const aborted = await post("/v1/host/prepare", {
    nickname: "Ada",
    password: "secret1",
    mode: "broadcast",
  });
  assert.equal(aborted.status, 200);
  assert.equal(aborted.json.status, "prepared");
  const abortedCode = aborted.json.code;
  assert.ok(aborted.json.prepare_token);
  // Prepared records are not in the viewer lookup table.
  const beforeAbort = await post("/v1/viewer/ask", {
    code: abortedCode,
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(beforeAbort.status, 404);
  const abort = await post("/v1/host/abort", { prepare_token: aborted.json.prepare_token });
  assert.equal(abort.status, 200);
  // Abort is terminal for the lease: a repeated abort finds nothing to
  // revoke and is denied, not a server error.
  const abortAgain = await post("/v1/host/abort", { prepare_token: aborted.json.prepare_token });
  assert.equal(abortAgain.status, 404);
  const afterAbort = await post("/v1/viewer/ask", {
    code: abortedCode,
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(afterAbort.status, 404);

  const committed = await post("/v1/host/prepare", {
    nickname: "Ada",
    password: "secret1",
    mode: "broadcast",
  });
  const hostToken = committed.json.prepare_token;
  const code = committed.json.code;
  const notYet = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(notYet.status, 404);
  const commit = await post("/v1/host/commit", { prepare_token: hostToken });
  assert.equal(commit.status, 200);
  assert.equal(commit.json.status, "active");

  // Retrying the same operation after a lost response is safe: the original
  // Room and Host token are returned, rather than creating a second Room or
  // denying the retry.
  const retry = await post("/v1/host/commit", { host_token: hostToken });
  assert.equal(retry.status, 200);
  assert.deepEqual(retry.json, commit.json);

  // A client-side response loss still permits the same retry. This uses a
  // fresh lease so the retry has to recover the commit, rather than relying
  // on the idempotence check above.
  const responseLossPrepare = await post("/v1/host/prepare", {
    nickname: "Ada",
    password: "secret1",
    mode: "broadcast",
  });
  const responseLossToken = responseLossPrepare.json.prepare_token;
  const responseLoss = new AbortController();
  const lost = fetch(base + "/v1/host/commit", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ prepare_token: responseLossToken }),
    signal: responseLoss.signal,
  }).catch(() => undefined);
  await new Promise((resolve) => setImmediate(resolve));
  responseLoss.abort();
  await lost;
  const responseLossRetry = await post("/v1/host/commit", { prepare_token: responseLossToken });
  assert.equal(responseLossRetry.status, 200);
  assert.equal(responseLossRetry.json.status, "active");
  assert.equal(responseLossRetry.json.code, responseLossPrepare.json.code);

  // Abort-after-commit is denied and keeps the Room alive: abort only
  // revokes uncommitted prepare leases, never a live Room.
  const abortCommitted = await post("/v1/host/abort", { prepare_token: hostToken });
  assert.equal(abortCommitted.status, 404);
  const responseLossCode = responseLossPrepare.json.code;
  const stillThere = await post("/v1/viewer/ask", {
    code: responseLossCode,
    nickname: "Zed",
    password: "secret1",
  });
  assert.equal(stillThere.json.status, "accepted", "abort must not kill a committed Room");

  // Unread offers are a bounded 1-slot mailbox: a rapid re-send replaces
  // the pending offer instead of queueing, so a late Viewer gets exactly
  // the latest attempt.
  const mb = await post("/v1/host/open", { nickname: "Ada", password: "secret1", mode: "broadcast" });
  assert.equal(mb.status, 200);
  const mbAsk = await post("/v1/viewer/ask", {
    code: mb.json.code,
    nickname: "Mia",
    password: "secret1",
  });
  assert.equal(mbAsk.json.status, "accepted");
  const mbHostWs = await connect("host", mb.json.host_token);
  const mbFence = { epoch: { session: 1, share: 1, link: 1 }, attempt: 1 };
  mbHostWs.send(
    JSON.stringify({
      t: "signal",
      viewer_id: mbAsk.json.member_id,
      payload: { type: "offer", sdp: "first", fence: mbFence },
    }),
  );
  mbHostWs.send(
    JSON.stringify({
      t: "signal",
      viewer_id: mbAsk.json.member_id,
      payload: { type: "offer", sdp: "second", fence: mbFence },
    }),
  );
  await new Promise((resolve) => setTimeout(resolve, 200));
  const mbViewerWs = await connect("viewer", mbAsk.json.viewer_token);
  const mbSeen = await new Promise((resolve) => {
    const seen = [];
    mbViewerWs.on("message", (data) => seen.push(JSON.parse(String(data))));
    setTimeout(() => resolve(seen), 600);
  });
  const mbSignals = mbSeen.filter((msg) => msg.t === "signal");
  assert.equal(mbSignals.length, 1, "mailbox must hold only the latest offer");
  assert.equal(mbSignals[0].payload.sdp, "second");
  assert.deepEqual(mbSignals[0].payload.fence, mbFence, "offer attempt must cross unchanged");
  mbHostWs.close();
  mbViewerWs.close();
  await post("/v1/host/close", { host_token: mb.json.host_token });

  const hostWs = await connect("host", hostToken);
  const rosterPromise = waitForMessage(hostWs, (msg) => msg.t === "roster" && msg.entries.length === 1);
  const viewer = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(viewer.json.status, "accepted");
  const viewerWs = await connect("viewer", viewer.json.viewer_token);
  const roster = await rosterPromise;
  assert.equal(roster.entries[0].nickname, "Bob");

  const fence = { epoch: { session: 4, share: 7, link: 9 }, attempt: 12 };
  const offerPayload = { type: "offer", sdp: "v=0\r\n", fence };
  hostWs.send(JSON.stringify({ t: "signal", viewer_id: viewer.json.member_id, payload: offerPayload }));
  const delivered = await waitForMessage(viewerWs, (msg) => msg.t === "signal" && msg.payload?.type === "offer");
  assert.deepEqual(delivered.payload.fence, fence, "offer attempt must cross Rendezvous unchanged");

  // Answers carry the exact opaque attempt back: the Rendezvous forwards
  // the fence unchanged so the Host can correlate it with its offer.
  const answerPayload = { type: "answer", sdp: "v=0\r\n", fence };
  viewerWs.send(JSON.stringify({ t: "signal", payload: answerPayload }));
  const answered = await waitForMessage(hostWs, (msg) => msg.t === "signal" && msg.payload?.type === "answer");
  assert.deepEqual(answered.payload.fence, fence, "answer attempt must cross Rendezvous unchanged");
  hostWs.close();
  viewerWs.close();

  const expired = await post("/v1/host/prepare", { nickname: "Cid", password: "secret1" });
  await new Promise((resolve) => setTimeout(resolve, 1400));
  const expiredCommit = await post("/v1/host/commit", { prepare_token: expired.json.prepare_token });
  assert.equal(expiredCommit.status, 404, "expired prepare must not activate");
  const expiredAsk = await post("/v1/viewer/ask", { code: expired.json.code, nickname: "Dan", password: "secret1" });
  assert.equal(expiredAsk.status, 404, "expired prepare must remain undiscoverable");
  console.log("prepare/commit/abort ok");
} finally {
  child.kill("SIGTERM");
}
