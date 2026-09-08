// Admission: pending members see nothing until the master admits them;
// rejection removes them; the pending cap holds.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  collect,
  waitFor,
  expectNothing,
} from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", {
    nickname: "Ada",
    password: "secret1",
    admission: true,
  });
  assert.equal(created.status, 200);
  const masterWs = await connectOk(base, created.json.token);
  const masterSeen = collect(masterWs);
  const masterId = created.json.memberId;

  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.json.status, "pending");
  const notice = await waitFor("pending notice", () =>
    masterSeen.find((m) => m.t === "pending" && m.member_id === joined.json.memberId),
  );
  assert.equal(notice.nickname, "Bob");

  const pendingWs = await connectOk(base, joined.json.token);
  const pendingSeen = collect(pendingWs);
  await expectNothing("pending roster/signal", () =>
    pendingSeen.find((m) => m.t === "roster" || m.t === "signal" || m.t === "admitted"),
  );

  // Pending members cannot signal: forbidden, nothing routed.
  pendingWs.send(
    JSON.stringify({
      t: "offer",
      to: masterId,
      payload: { type: "offer", session: "s", share: "sh", link: "l", attempt: 1, sdp: "v=0" },
    }),
  );
  await waitFor("pending forbidden", () =>
    pendingSeen.find((m) => m.t === "error" && m.error === "forbidden"),
  );
  await expectNothing("pending signal routed", () =>
    masterSeen.find((m) => m.t === "signal"),
  );

  // Master admits: admitted frame + roster, then signaling flows.
  masterWs.send(JSON.stringify({ t: "admit", member: joined.json.memberId, accept: true }));
  await waitFor("admitted", () =>
    pendingSeen.find((m) => m.t === "admitted" && m.member_id === joined.json.memberId),
  );
  pendingWs.send(
    JSON.stringify({
      t: "answer",
      to: masterId,
      payload: { type: "answer", session: "s", share: "sh", link: "l", attempt: 1, sdp: "v=0-a" },
    }),
  );
  const routed = await waitFor("post-admit signal", () =>
    masterSeen.find((m) => m.t === "signal" && m.payload?.type === "answer"),
  );
  assert.equal(routed.from, joined.json.memberId);

  // Rejection removes the pending member.
  const joined2 = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Cid",
    password: "secret1",
  });
  assert.equal(joined2.json.status, "pending");
  masterWs.send(JSON.stringify({ t: "admit", member: joined2.json.memberId, accept: false }));
  await waitFor("reject ok", () => masterSeen.find((m) => m.t === "ok"));
  const gone = await post(base, `/v1/rooms/${created.json.code}/leave`, {
    token: joined2.json.token,
  });
  assert.equal(gone.status, 404, "rejected token must be dead");

  // Pending cap: 8 pending max (1 slot used above by Bob? No — Bob was
  // admitted. Room currently has 2 accepted, 0 pending).
  for (let i = 0; i < 8; i += 1) {
    const j = await post(base, `/v1/rooms/${created.json.code}/join`, {
      nickname: `P${i}`,
      password: "secret1",
    });
    assert.equal(j.json.status, "pending");
  }
  const pFull = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Pover",
    password: "secret1",
  });
  assert.equal(pFull.status, 429);

  masterWs.close();
  pendingWs.close();
  console.log("admit ok");
} finally {
  await srv.kill();
}
