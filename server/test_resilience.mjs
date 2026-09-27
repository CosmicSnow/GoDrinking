// A valid member's WebSocket inbound messages should have a per-connection
// budget; a heartbeat burst must not monopolize the server or flood replies.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  rawRequest,
  connectOk,
  collect,
  waitFor,
  sleep,
} from "./test_helpers.mjs";

const srv = await spawnServer();
let ws;
try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);

  ws = await connectOk(base, created.json.token);
  const seen = collect(ws);

  // Verify normal liveness independently of the flood assertion.
  ws.send(JSON.stringify({ t: "heartbeat" }));
  await waitFor("normal heartbeat roster response", () =>
    seen.find((message) => message.t === "roster" && message.master_id === created.json.memberId),
  );
  seen.length = 0;

  // Send one burst on the same valid member socket. Count roster replies over
  // a fixed, bounded observation window instead of relying on a long timeout.
  for (let i = 0; i < 100; i += 1) {
    ws.send(JSON.stringify({ t: "heartbeat" }));
  }
  await sleep(500);
  const burstResponses = seen.filter(
    (message) => message.t === "roster" && message.master_id === created.json.memberId,
  ).length;

  assert.ok(
    burstResponses <= 40,
    `100 quick heartbeats should receive at most 40 roster responses in 500ms; got ${burstResponses}`,
  );

  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200, "health endpoint should respond after the WebSocket burst");
  assert.equal(srv.child.exitCode, null, "server should remain running after the WebSocket burst");

  console.log(`resilience ok (${burstResponses} roster responses)`);
} finally {
  if (ws && ws.readyState !== ws.CLOSED) ws.terminate();
  await srv.kill();
}
