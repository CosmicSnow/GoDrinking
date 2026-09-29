import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createDiagnostics } from "./diagnostics.mjs";
import { spawnServer, post, connectOk, collect, signals, waitFor } from "./test_helpers.mjs";

const directory = await mkdtemp(join(tmpdir(), "golive-diag-"));
const file = join(directory, "events.jsonl");
const marker = `PRIVATE${Math.random().toString(36).slice(2, 10)}`;
const srv = await spawnServer({ DIAGNOSTIC_LOG_PATH: file, DISCONNECT_GRACE_MS: "75" });
try {
  const password = `pass-${marker}`;
  const created = await post(srv.base, "/v1/rooms", { nickname: `A${marker}`, password });
  assert.equal(created.status, 200);
  const joined = await post(srv.base, `/v1/rooms/${created.json.code}/join`, {
    nickname: `B${marker}`, password,
  });
  assert.equal(joined.status, 200);
  const a = await connectOk(srv.base, created.json.token);
  const b = await connectOk(srv.base, joined.json.token);
  const seen = collect(b);
  for (let i = 0; i < 7; i += 1) a.send(JSON.stringify({ t: "watch", to: joined.json.memberId }));
  a.send(JSON.stringify({
    t: "offer", to: joined.json.memberId,
    payload: { type: "offer", session: "s", share: "sh", link: "l", attempt: 1, sdp: `SDP-${marker}` },
  }));
  await waitFor("offer forwarded", () => signals(seen).find((m) => m.payload?.type === "offer"));
  a.send(JSON.stringify({ t: "watch", to: "unknown" }));
  // A second message from the same socket is a processing barrier.
  a.send(JSON.stringify({
    t: "candidate", to: joined.json.memberId,
    payload: { type: "candidate", session: "s", share: "sh", link: "l", attempt: 1, candidate: `CAND-${marker} typ host` },
  }));
  await waitFor("candidate forwarded", () => signals(seen).find((m) => m.payload?.type === "candidate"));
  a.close();
  await waitFor("disconnect grace expiry", () => seen.find((m) =>
    m.t === "roster" && m.entries.every((entry) => entry.id !== created.json.memberId)));
  await srv.kill(); // SIGTERM flushes the current aggregate window.

  const data = await readFile(file, "utf8");
  const records = data.trim().split("\n").map((line) => JSON.parse(line));
  const has = (event, reason) => records.find((r) => r.event === event && r.reason === reason);
  assert.ok(has("startup", "started"));
  assert.ok(has("shutdown", "stopped"));
  assert.equal(has("watch", "forwarded")?.count, 7);
  assert.equal(has("watch", "rejected")?.count, 1);
  assert.equal(has("signal", "offer")?.count, 1);
  assert.equal(has("signal", "forwarded")?.count, 1);
  assert.ok(has("websocket", "closed"));
  assert.equal(has("grace_expiry", "expired")?.count, 1);
  for (const record of records) {
    assert.deepEqual(Object.keys(record).sort(), ["count", "event", "reason", "ts"]);
    assert.ok(Number.isSafeInteger(record.count) && record.count > 0);
  }
  for (const secret of [marker, password, created.json.code, created.json.token,
    joined.json.token, created.json.memberId, joined.json.memberId]) {
    assert.ok(!data.includes(secret), "diagnostic file must not disclose identities or media");
    assert.ok(!srv.logs().includes(secret), "stdout must not disclose identities or media");
  }
  assert.equal((await stat(file)).mode & 0o777, 0o600);
  a.terminate();
  b.terminate();

  // A preexisting full file rotates before a new event is appended.
  await writeFile(file, "x".repeat(10 * 1024 * 1024));
  const diag = createDiagnostics(file);
  diag.record("watch", "rejected");
  diag.close();
  assert.equal((await stat(`${file}.1`)).size, 10 * 1024 * 1024);
  assert.equal(JSON.parse((await readFile(file, "utf8")).trim()).event, "watch");
  console.log("diagnostics ok");
} finally {
  if (!srv.child.killed) await srv.kill();
  await rm(directory, { recursive: true, force: true });
}
