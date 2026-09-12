// Room lifecycle: create / join / leave, capacities, input validation.
import assert from "node:assert/strict";
import { spawnServer, post, sleep } from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);
  assert.match(created.json.code, /^[A-Z0-9]{6}$/);
  assert.ok(created.json.memberId);
  assert.ok(created.json.token);
  const { code } = created.json;

  const joined = await post(base, `/v1/rooms/${code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.status, 200);
  assert.equal(joined.json.status, "accepted");

  const left = await post(base, `/v1/rooms/${code}/leave`, { token: joined.json.token });
  assert.equal(left.status, 200);
  assert.equal(left.json.ok, true);

  // A used token is dead: leaving twice is denied.
  const leftAgain = await post(base, `/v1/rooms/${code}/leave`, { token: joined.json.token });
  assert.equal(leftAgain.status, 404);

  // Password is mandatory; nicknames are validated.
  const noPassword = await post(base, "/v1/rooms", { nickname: "Eve" });
  assert.equal(noPassword.status, 400);
  const badNick = await post(base, "/v1/rooms", { nickname: "x", password: "secret1" });
  assert.equal(badNick.status, 400);

  // Capacity: master + 7 joins = 8 accepted; the 9th is full.
  const room2 = await post(base, "/v1/rooms", { nickname: "Max", password: "secret1" });
  assert.equal(room2.status, 200);
  for (let i = 0; i < 7; i += 1) {
    const j = await post(base, `/v1/rooms/${room2.json.code}/join`, {
      nickname: `M${i}`,
      password: "secret1",
    });
    assert.equal(j.json.status, "accepted");
  }
  const overflow = await post(base, `/v1/rooms/${room2.json.code}/join`, {
    nickname: "Extra",
    password: "secret1",
  });
  assert.equal(overflow.status, 429);
  assert.equal(overflow.json.error, "full");

  // Codes are crypto-random: two rooms differ.
  assert.notEqual(room2.json.code, code);

  await sleep(50);
  console.log("rooms ok");
} finally {
  await srv.kill();
}
