// Kick: master-only, invalidates the victim token everywhere, frees the cap.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  connectFails,
  collect,
  waitFor,
} from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  const victimToken = joined.json.token;

  const masterWs = await connectOk(base, created.json.token);
  const masterSeen = collect(masterWs);
  const victimWs = await connectOk(base, victimToken);
  const victimSeen = collect(victimWs);

  // Non-master kick is forbidden.
  victimWs.send(JSON.stringify({ t: "kick", target: created.json.memberId }));
  await waitFor("kick forbidden", () =>
    victimSeen.find((m) => m.t === "error" && m.error === "forbidden"),
  );

  // The master cannot kick itself.
  masterWs.send(JSON.stringify({ t: "kick", target: created.json.memberId }));
  await waitFor("self-kick invalid", () =>
    masterSeen.find((m) => m.t === "error" && m.error === "invalid"),
  );

  // Real kick: victim is told, its socket dies, roster shrinks.
  const victimClosed = new Promise((resolve) => {
    victimWs.once("close", resolve);
    victimWs.once("error", resolve);
  });
  masterWs.send(JSON.stringify({ t: "kick", target: joined.json.memberId }));
  await waitFor("kicked frame", () => victimSeen.find((m) => m.t === "kicked"));
  await victimClosed;
  await waitFor("kick ok", () => masterSeen.find((m) => m.t === "ok"));
  const roster = await waitFor("shrunk roster", () =>
    masterSeen.find((m) => m.t === "roster" && !m.entries.some((e) => e.id === joined.json.memberId)),
  );
  assert.equal(roster.entries.length, 1);

  // The token is dead on every surface.
  await connectFails(base, victimToken);
  const leave = await post(base, `/v1/rooms/${created.json.code}/leave`, { token: victimToken });
  assert.equal(leave.status, 404);

  // The cap is freed: a fresh join is accepted.
  const rejoin = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(rejoin.json.status, "accepted");

  masterWs.close();
  console.log("kick ok");
} finally {
  await srv.kill();
}
