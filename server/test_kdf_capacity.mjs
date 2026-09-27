// A burst of ten valid creates must be bounded by the active KDF capacity,
// reject excess work immediately, and leave the server healthy.
import assert from "node:assert/strict";
import { spawnServer, post, rawRequest } from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;
  const requests = Array.from({ length: 10 }, () =>
    post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" }),
  );
  const results = await Promise.all(requests);

  assert.equal(results.length, 10, "all ten concurrent create requests should complete");
  const rejected = results.filter((result) => result.status === 429);
  const succeeded = results.filter((result) => result.status === 200);
  assert.ok(rejected.length > 0, "burst should reject excess KDF work with 429");
  assert.ok(succeeded.length <= 4, `at most four creates may succeed, got ${succeeded.length}`);
  for (const result of rejected) {
    assert.deepEqual(result.json, { ok: false, error: "busy" });
  }
  for (const result of results) {
    assert.ok(
      result.status === 200 || result.status === 429,
      `unexpected create status ${result.status}`,
    );
  }

  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200);

  console.log("kdf capacity ok");
} finally {
  await srv.kill();
}
