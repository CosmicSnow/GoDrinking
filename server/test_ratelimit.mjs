// Rate limit + tarpit: repeated auth failures ignore the IP; the server
// itself stays up (/health answers).
import assert from "node:assert/strict";
import { spawnServer, post, rawRequest } from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);

  for (let i = 0; i < 6; i += 1) {
    const bad = await post(base, `/v1/rooms/${created.json.code}/join`, {
      nickname: "Bob",
      password: "wrong",
    });
    assert.equal(bad.status, 404);
  }

  // The IP is now ignored: even the right password is denied.
  const good = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(good.status, 404);

  // …but the process is alive and healthy (health bypasses the ignore list).
  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200);

  console.log("ratelimit ok");
} finally {
  await srv.kill();
}
