// A room may retain at most 256 distinct signaling attempts.
import assert from "node:assert/strict";
import { post, rawRequest, spawnServer, connectOk, sleep } from "./test_helpers.mjs";

const srv = await spawnServer();
let hostWs;
let peerWs;
const hardTimeout = setTimeout(() => {
  console.error("signal state capacity timed out");
  process.exit(1);
}, 30_000);

try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.status, 200);

  hostWs = await connectOk(base, created.json.token);
  peerWs = await connectOk(base, joined.json.token);

  const errors = [];
  hostWs.on("message", (data) => {
    try {
      const message = JSON.parse(String(data));
      if (message.t === "error") errors.push(message);
    } catch {
      // Ignore non-JSON messages; only retain errors, not the offer stream.
    }
  });

  for (let i = 0; i < 270; i += 1) {
    hostWs.send(JSON.stringify({
      t: "offer",
      to: joined.json.memberId,
      payload: {
        type: "offer",
        session: "capacity-test",
        share: "capacity-test",
        link: `attempt-${i}`,
        attempt: 1,
        sdp: "v=0",
      },
    }));
    await sleep(25);
  }

  assert.ok(
    errors.some((message) => message.error === "full"),
    "a finite stream of 270 distinct valid offers should hit the 256-attempt room cap",
  );

  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200, "server should remain healthy after reaching signaling state capacity");
  console.log("signal state capacity ok");
} finally {
  clearTimeout(hardTimeout);
  hostWs?.close();
  peerWs?.close();
  await srv.kill();
}
