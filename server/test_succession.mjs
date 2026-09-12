// Succession: oldest joinedAt wins, lexical id breaks ties; the last
// member out destroys the room.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  collect,
  waitFor,
  sleep,
} from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  const { code } = created.json;
  await sleep(10);
  const joinA = await post(base, `/v1/rooms/${code}/join`, {
    nickname: "Amy",
    password: "secret1",
  });
  await sleep(10);
  const joinB = await post(base, `/v1/rooms/${code}/join`, {
    nickname: "Bea",
    password: "secret1",
  });

  const masterWs = await connectOk(base, created.json.token);
  const masterSeen = collect(masterWs);
  const aWs = await connectOk(base, joinA.json.token);
  const bWs = await connectOk(base, joinB.json.token);
  void aWs;
  void bWs;
  const bSeen = collect(bWs);

  // Master leaves: Amy (oldest) succeeds.
  const left = await post(base, `/v1/rooms/${code}/leave`, { token: created.json.token });
  assert.equal(left.status, 200);
  const r1 = await waitFor("succession roster", () =>
    bSeen.find((m) => m.t === "roster" && m.master_id === joinA.json.memberId),
  );
  assert.ok(r1.entries.some((e) => e.id === joinA.json.memberId && e.master));
  assert.ok(!r1.entries.some((e) => e.id === created.json.memberId));
  void masterSeen;

  // Amy leaves: Bea succeeds.
  await post(base, `/v1/rooms/${code}/leave`, { token: joinA.json.token });
  const r2 = await waitFor("second succession", () =>
    bSeen.find((m) => m.t === "roster" && m.master_id === joinB.json.memberId),
  );
  assert.equal(r2.entries.length, 1);

  // Bea leaves: the room is destroyed, the code is dead.
  await post(base, `/v1/rooms/${code}/leave`, { token: joinB.json.token });
  const after = await post(base, `/v1/rooms/${code}/join`, {
    nickname: "Zed",
    password: "secret1",
  });
  assert.equal(after.status, 404);

  masterWs.close();
  console.log("succession ok");
} finally {
  await srv.kill();
}
