// A fragmented WebSocket message must not let a peer consume an unbounded
// number of frames; exceeding the fragment cap closes only that connection.
import assert from "node:assert/strict";
import { spawnServer, post, connectOk, rawRequest, waitFor } from "./test_helpers.mjs";

const srv = await spawnServer();
let ws;
try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);

  ws = await connectOk(base, created.json.token);
  ws.on("error", () => {});

  let rosterReceived = false;
  // Install before sending to avoid racing a fast local response.
  ws.on("message", (data) => {
    try {
      if (JSON.parse(String(data)).t === "roster") rosterReceived = true;
    } catch {
      // Ignore unrelated/non-JSON messages.
    }
  });

  // Prove the authenticated socket is healthy before exercising fragmentation.
  ws.send(JSON.stringify({ t: "heartbeat" }));
  await waitFor(
    "normal heartbeat roster",
    () => rosterReceived,
    1500,
  );

  // Send the heartbeat JSON as the initial text fragment, followed by 40
  // continuation frames. Use ws's sender to preserve real client masking and
  // continuation opcodes while controlling FIN on every fragment.
  const sender = ws._sender;
  sender.send(JSON.stringify({ t: "heartbeat" }), { binary: false, fin: false, mask: true });
  for (let i = 0; i < 40; i += 1) {
    sender.send(Buffer.alloc(0), { binary: false, fin: false, mask: true });
  }

  let closeTimer;
  const closed = new Promise((resolve) => {
    closeTimer = setTimeout(() => resolve(false), 1500);
    ws.once("close", () => {
      clearTimeout(closeTimer);
      resolve(true);
    });
  });
  sender.send(Buffer.alloc(0), { binary: false, fin: true, mask: true });

  assert.equal(await closed, true, "socket sending more than 32 fragments should be terminated");

  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200, "health endpoint should survive a fragmented-message violation");
  assert.equal(srv.child.exitCode, null, "server should remain running after terminating the socket");

  console.log("websocket fragments ok");
} finally {
  if (ws && ws.readyState !== ws.CLOSED) ws.terminate();
  await srv.kill();
}
