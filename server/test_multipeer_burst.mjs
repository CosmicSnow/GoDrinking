// A multi-peer signaling burst must not trip the per-socket message cap.
import assert from "node:assert/strict";
import WebSocket from "ws";
import {
  spawnServer,
  post,
  connectOk,
  waitFor,
} from "./test_helpers.mjs";

const VIEWER_COUNT = 4;
const CANDIDATES_PER_VIEWER = 64;
const BURST_TIMEOUT_MS = 8000;
const srv = await spawnServer();
const sockets = [];

try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", {
    nickname: "Host",
    password: "secret1",
    admission: true,
  });
  assert.equal(created.status, 200);
  const hostId = created.json.memberId;
  const hostWs = await connectOk(base, created.json.token);
  sockets.push(hostWs);
  const hostSeen = [];
  hostWs.on("message", (data) => {
    try {
      const message = JSON.parse(String(data));
      if (message.t === "error") hostSeen.push(message.error);
    } catch {
      // Ignore non-JSON frames.
    }
  });

  const viewers = [];
  for (let i = 0; i < VIEWER_COUNT; i += 1) {
    const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
      nickname: `Viewer${i + 1}`,
      password: "secret1",
    });
    assert.equal(joined.status, 200);
    assert.equal(joined.json.status, "pending");

    const ws = await connectOk(base, joined.json.token);
    sockets.push(ws);
    const viewer = { id: joined.json.memberId, ws, received: { offers: 0, candidates: 0 }, admitted: false };
    viewers.push(viewer);
    ws.on("message", (data) => {
      try {
        const message = JSON.parse(String(data));
        if (message.t === "admitted" && message.member_id === viewer.id) {
          viewer.admitted = true;
          return;
        }
        if (message.t !== "signal" || message.from !== hostId || message.to !== viewer.id) return;
        const payload = message.payload;
        if (payload?.type === "offer" && payload.link === `burst-link-${i}`) viewer.received.offers += 1;
        if (payload?.type === "candidate" && payload.link === `burst-link-${i}`) viewer.received.candidates += 1;
      } catch {
        // Ignore non-JSON frames; retain only counters, never full payloads.
      }
    });

    hostWs.send(JSON.stringify({ t: "admit", member: joined.json.memberId, accept: true }));
    await waitFor(`viewer ${i + 1} admitted`, () =>
      viewer.admitted ? true : null,
    );
  }

  // Per-viewer negotiation identity keeps candidate budgets independent.
  for (let i = 0; i < VIEWER_COUNT; i += 1) {
    const viewer = viewers[i];
    const ids = {
      session: `burst-session-${i}`,
      share: `burst-share-${i}`,
      link: `burst-link-${i}`,
      attempt: 1,
    };
    hostWs.send(JSON.stringify({
      t: "offer",
      to: viewer.id,
      payload: { type: "offer", ...ids, sdp: "v=0" },
    }));
    for (let n = 0; n < CANDIDATES_PER_VIEWER; n += 1) {
      const candidate = `candidate:${n + 1} 1 udp 1 192.0.2.1 5000 typ host`;
      assert.ok(Buffer.byteLength(candidate, "utf8") <= 8 * 1024);
      hostWs.send(JSON.stringify({
        t: "candidate",
        to: viewer.id,
        payload: { type: "candidate", ...ids, candidate },
      }));
    }
  }

  await waitFor("all multi-peer burst signals", () =>
    viewers.every(({ received }) =>
      received.offers === 1 && received.candidates === CANDIDATES_PER_VIEWER,
    ) ? true : null,
  BURST_TIMEOUT_MS);
  assert.equal(hostWs.readyState, WebSocket.OPEN, "host socket must remain connected after 260-message burst");
  assert.deepEqual(hostSeen, [], "host must not receive signaling errors");

  const health = await fetch(`${base}/health`);
  assert.equal(health.status, 200, "health endpoint remains available after burst");
  console.log("multi-peer burst ok");
} finally {
  for (const ws of sockets) {
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) ws.close();
  }
  await srv.kill();
}
