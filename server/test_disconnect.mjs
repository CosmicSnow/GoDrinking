// Socket close without REST leave must drop the member from the roster
// after the disconnect grace (not five minutes later).
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  collect,
  waitFor,
} from "./test_helpers.mjs";

const srv = await spawnServer({ DISCONNECT_GRACE_MS: "80", GC_INTERVAL_MS: "50" });
try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.status, 200);

  const wsAda = await connectOk(base, created.json.token);
  const wsBob = await connectOk(base, joined.json.token);
  const adaSeen = collect(wsAda);
  wsBob.close();

  const shrunk = await waitFor(
    "ghost gone from roster",
    () => adaSeen.find((m) => m.t === "roster" && m.entries.length === 1),
    2000,
  );
  assert.equal(shrunk.entries[0].nickname, "Ada");
  assert.ok(!shrunk.entries.some((e) => e.id === joined.json.memberId));

  console.log("disconnect ok");
} finally {
  await srv.kill();
}
