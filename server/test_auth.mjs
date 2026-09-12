// Auth: unknown code vs wrong password are indistinguishable; failures
// carry no token and leak nothing.
import assert from "node:assert/strict";
import { spawnServer, post } from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);

  const wrongPassword = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "wrong-password",
  });
  const unknownCode = await post(base, "/v1/rooms/ZZZZZZ/join", {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(wrongPassword.status, 404);
  assert.equal(unknownCode.status, 404);
  assert.deepEqual(wrongPassword.json, unknownCode.json);
  assert.deepEqual(wrongPassword.json, { ok: false, error: "denied" });
  assert.ok(!wrongPassword.json.token);

  // Bogus leave tokens are denied the same way.
  const bogusLeave = await post(base, `/v1/rooms/${created.json.code}/leave`, {
    token: "nope",
  });
  assert.equal(bogusLeave.status, 404);
  assert.deepEqual(bogusLeave.json, { ok: false, error: "denied" });

  // A correct password still works afterwards (no self-tarpit from 2 fails).
  const good = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(good.status, 200);
  assert.ok(good.json.token);

  console.log("auth ok");
} finally {
  await srv.kill();
}
