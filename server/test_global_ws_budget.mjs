// Independent WebSocket byte bursts must still share a finite process-wide budget.
import assert from "node:assert/strict";
import WebSocket from "ws";
import { spawnServer, waitFor } from "./test_helpers.mjs";

const ROOM_COUNT = 6;
const OFFERS_PER_SENDER = 120;
const FRAME_BYTES = 60 * 1024;
const REQUEST_TIMEOUT_MS = 15_000;
const CLOSE_TIMEOUT_MS = 8_000;

const srv = await spawnServer();
const sockets = [];

async function request(base, path, body) {
  const response = await fetch(`${base}${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  let json = null;
  try {
    json = await response.json();
  } catch {
    // The status assertion below is sufficient for non-JSON failures.
  }
  return { status: response.status, json };
}

try {
  const senders = [];
  // Create and join sequentially: room/password routes use bounded KDF capacity.
  for (let i = 0; i < ROOM_COUNT; i += 1) {
    const created = await request(srv.base, "/v1/rooms", {
      nickname: `Sender${i + 1}`,
      password: "secret1",
    });
    assert.equal(created.status, 200, `room ${i + 1} creation`);

    const recipient = await request(srv.base, `/v1/rooms/${created.json.code}/join`, {
      nickname: `Peer${i + 1}`,
      password: "secret1",
    });
    assert.equal(recipient.status, 200, `room ${i + 1} accepted peer join`);
    assert.equal(recipient.json.status, "accepted");

    const ws = await new Promise((resolve, reject) => {
      const socket = new WebSocket(`${srv.base.replace("http", "ws")}/ws?token=${created.json.token}`);
      const timer = setTimeout(() => {
        socket.terminate();
        reject(new Error(`sender ${i + 1} WebSocket open timeout`));
      }, 4_000);
      socket.once("open", () => {
        clearTimeout(timer);
        resolve(socket);
      });
      socket.once("error", (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
    // Keep no received signaling payloads: recipient sockets intentionally stay disconnected.
    ws.on("error", () => {});
    sockets.push(ws);
    senders.push({ ws, recipientId: recipient.json.memberId, index: i, closed: false });
    ws.once("close", () => {
      senders[i].closed = true;
    });
  }

  let sent = 0;
  // Round-robin 60 KiB valid offers: each socket stays below its 8 MiB byte
  // burst, while the aggregate is about 43 MiB. Unique links remain below the
  // per-room signaling-state limit. Do not retain generated messages.
  for (let n = 0; n < OFFERS_PER_SENDER; n += 1) {
    for (const sender of senders) {
      if (sender.ws.readyState !== WebSocket.OPEN) continue;
      const message = {
        t: "offer",
        to: sender.recipientId,
        payload: {
          type: "offer",
          session: `global-budget-${sender.index}`,
          share: "global-budget",
          link: `offer-${n}`,
          attempt: 1,
          sdp: "",
        },
      };
      const empty = JSON.stringify(message);
      message.payload.sdp = "v=0" + "x".repeat(FRAME_BYTES - Buffer.byteLength(empty) - 3);
      const encoded = JSON.stringify(message);
      assert.equal(Buffer.byteLength(encoded), FRAME_BYTES, "each offer frame is exactly 60 KiB");
      sender.ws.send(encoded);
      sent += 1;
    }
  }

  assert.ok(sent > 0 && sent <= ROOM_COUNT * OFFERS_PER_SENDER, "bounded aggregate frame count");
  assert.ok(sent * FRAME_BYTES <= 45 * 1024 * 1024, "payload stays within memory budget");

  await waitFor(
    "at least one sender terminated by the process-wide byte budget",
    () => senders.some((sender) => sender.closed),
    CLOSE_TIMEOUT_MS,
  );

  const health = await fetch(`${srv.base}/health`, { signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) });
  assert.equal(health.status, 200, "health endpoint remains available after aggregate WS flood");
  assert.equal(srv.child.exitCode, null, "server remains running after aggregate WS flood");
  console.log(`global WebSocket byte budget ok (${sent} frames)`);
} finally {
  for (const ws of sockets) {
    if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) ws.terminate();
  }
  await srv.kill();
}
