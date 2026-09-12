// Heartbeat expiry: silent members are pruned and empty rooms destroyed;
// heartbeating members survive.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  connectFails,
  collect,
  waitFor,
  sleep,
} from "./test_helpers.mjs";

const srv = await spawnServer({ HEARTBEAT_TTL_MS: "800", GC_INTERVAL_MS: "100" });
try {
  const { base } = srv;

  // A member that never heartbeats expires; its room dies with it.
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  const idleToken = created.json.token;
  const { code } = created.json;
  await sleep(1600);
  await connectFails(base, idleToken);
  const gone = await post(base, `/v1/rooms/${code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(gone.status, 404, "empty expired room must be destroyed");

  // A heartbeating member survives past the TTL, then expires when quiet.
  const created2 = await post(base, "/v1/rooms", { nickname: "Al", password: "secret1" });
  const ws = await connectOk(base, created2.json.token);
  const seen = collect(ws);
  for (let i = 0; i < 6; i += 1) {
    ws.send(JSON.stringify({ t: "heartbeat" }));
    await sleep(300);
  }
  const ack = await waitFor("heartbeat ack", () =>
    seen.find((m) => m.t === "roster" && m.master_id === created2.json.memberId),
  );
  assert.ok(ack.entries.length === 1, "heartbeating member must survive");
  seen.length = 0;
  const closed = new Promise((resolve) => {
    ws.once("close", resolve);
    ws.once("error", resolve);
  });
  await closed;
  await connectFails(base, created2.json.token);

  console.log("heartbeat ok");
} finally {
  await srv.kill();
}
