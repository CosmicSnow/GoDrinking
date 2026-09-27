// Reconnecting during the disconnect grace must extend the member's grace
// from the latest disconnect, rather than leaving the original timer active.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  connectFails,
  sleep,
} from "./test_helpers.mjs";

const graceMs = 1000;
const srv = await spawnServer({
  DISCONNECT_GRACE_MS: String(graceMs),
  GC_INTERVAL_MS: "25",
});

function closed(ws) {
  return new Promise((resolve) => {
    ws.once("close", resolve);
    ws.once("error", resolve);
  });
}

try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", {
    nickname: "Ada",
    password: "secret1",
  });
  assert.equal(created.status, 200);

  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.status, 200);
  const token = joined.json.token;

  const first = await connectOk(base, token);
  const t0 = Date.now();
  const firstClosed = closed(first);
  first.close();
  await firstClosed;

  // Reconnect and disconnect well before the first grace expires.
  await sleep(200);
  const second = await connectOk(base, token);
  const secondClosed = closed(second);
  second.close();
  await secondClosed;
  const secondDisconnectAt = Date.now();

  // The original deadline has passed, but the second disconnect's grace has
  // not. The same member token must still be accepted during this interval.
  const checkAt = t0 + graceMs + 75;
  await sleep(Math.max(0, checkAt - Date.now()));
  assert.ok(
    Date.now() < secondDisconnectAt + graceMs,
    "reconnect check must occur before the latest disconnect grace expires",
  );
  const third = await connectOk(base, token);
  const thirdClosed = closed(third);
  third.close();
  await thirdClosed;
  const latestDisconnectAt = Date.now();

  // Once the latest disconnect's grace has expired, the member is removed.
  await sleep(Math.max(0, latestDisconnectAt + graceMs + 100 - Date.now()));
  await connectFails(base, token);

  console.log("reconnect grace ok");
} finally {
  await srv.kill();
}
