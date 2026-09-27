// A valid member may use WebSocket ping for liveness, but a control-frame
// burst must be bounded per connection without taking down the server.
import assert from "node:assert/strict";
import { spawnServer, post, rawRequest, connectOk, waitFor } from "./test_helpers.mjs";

const srv = await spawnServer();
let ws;
try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);

  ws = await connectOk(base, created.json.token);
  // ws emits pong when the peer answers our ping; install the listener before
  // sending so a fast local response cannot race the assertion.
  let gotPong = false;
  const normalPong = waitFor("normal WebSocket pong", () => gotPong, 1000);
  ws.once("pong", () => {
    gotPong = true;
  });
  ws.ping(Buffer.from([0x01]));
  await normalPong;

  let closeObserved = false;
  const closed = new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), 2000);
    ws.once("close", () => {
      closeObserved = true;
      clearTimeout(timer);
      resolve(true);
    });
  });
  ws.on("error", () => {});

  // Keep the burst deliberately small and bounded: each control frame has a
  // one-byte payload, and no further frames are sent after these 300.
  for (let i = 0; i < 300; i += 1) {
    ws.ping(Buffer.from([i & 0xff]));
  }

  const terminatedInTime = await closed;
  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200, "health endpoint should remain available after the ping burst");
  assert.equal(srv.child.exitCode, null, "server should remain running after the ping burst");
  assert.ok(
    terminatedInTime && closeObserved,
    "server should terminate a socket sending 300 control-frame pings within 2 seconds",
  );

  console.log("control frames ok");
} finally {
  if (ws && ws.readyState !== ws.CLOSED) ws.terminate();
  await srv.kill();
}
