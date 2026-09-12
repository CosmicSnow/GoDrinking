// Envelope limits: 64 candidates/attempt, 8 KiB/candidate, 64 KiB/message,
// exact keys, no relay candidates.
import assert from "node:assert/strict";
import {
  spawnServer,
  post,
  connectOk,
  collect,
  signals,
  waitFor,
  expectNothing,
} from "./test_helpers.mjs";

const srv = await spawnServer();
try {
  const { base } = srv;

  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  const aId = created.json.memberId;
  const bId = joined.json.memberId;

  const aWs = await connectOk(base, created.json.token);
  const bWs = await connectOk(base, joined.json.token);
  const aSeen = collect(aWs);
  const bSeen = collect(bWs);
  const ids = { session: "s", share: "sh", link: "flood", attempt: 1 };
  const cand = (i) => ({ type: "candidate", ...ids, candidate: `c${i} typ host` });

  for (let i = 0; i < 64; i += 1) {
    aWs.send(JSON.stringify({ t: "candidate", to: bId, payload: cand(i) }));
  }
  await waitFor(
    "64 candidates",
    () => (signals(bSeen).filter((m) => m.payload?.link === "flood").length === 64 ? true : null),
    6000,
  );
  bSeen.length = 0;
  aSeen.length = 0;
  aWs.send(JSON.stringify({ t: "candidate", to: bId, payload: cand(64) }));
  await waitFor("budget error", () =>
    aSeen.find((m) => m.t === "error" && m.error === "stale"),
  );
  await expectNothing("65th delivered", () => signals(bSeen).length);

  // A new attempt resets the budget.
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "offer",
      to: bId,
      payload: { type: "offer", session: "s", share: "sh", link: "flood", attempt: 2, sdp: "v=0" },
    }),
  );
  await waitFor("offer 2", () =>
    signals(bSeen).find((m) => m.payload?.attempt === 2 && m.payload?.type === "offer"),
  );
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: { type: "candidate", session: "s", share: "sh", link: "flood", attempt: 2, candidate: "fresh typ host" },
    }),
  );
  await waitFor("fresh candidate", () =>
    signals(bSeen).find((m) => m.payload?.candidate === "fresh typ host"),
  );

  // Oversize candidate (>8 KiB) is invalid.
  aSeen.length = 0;
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: { type: "candidate", session: "s", share: "sh", link: "big", attempt: 1, candidate: "x".repeat(8193) },
    }),
  );
  await waitFor("oversize error", () =>
    aSeen.find((m) => m.t === "error" && m.error === "invalid"),
  );
  await expectNothing("oversize delivered", () => signals(bSeen).length);

  // Extra keys are rejected.
  aSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: { type: "candidate", session: "s", share: "sh", link: "x", attempt: 1, candidate: "c typ host", extra: 1 },
    }),
  );
  await waitFor("extra-key error", () =>
    aSeen.find((m) => m.t === "error" && m.error === "invalid"),
  );

  // Unknown kinds are rejected.
  aWs.send(JSON.stringify({ t: "prank", to: bId, payload: {} }));
  await waitFor("unknown error", () =>
    aSeen.find((m) => m.t === "error" && m.error === "invalid"),
  );

  // Relay candidates are rejected (no TURN); srflx passes on a clean link.
  aSeen.length = 0;
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: { type: "candidate", session: "s", share: "sh", link: "ice", attempt: 1, candidate: "candidate:1 1 udp 1 203.0.113.7 5000 typ srflx raddr 10.0.0.1 rport 5000" },
    }),
  );
  await waitFor("srflx", () =>
    signals(bSeen).find((m) => m.payload?.link === "ice"),
  );
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: { type: "candidate", session: "s", share: "sh", link: "ice2", attempt: 1, candidate: "candidate:1 1 udp 1 192.0.2.1 5000 typ relay raddr 10.0.0.1 rport 5000" },
    }),
  );
  await waitFor("relay error", () =>
    aSeen.find((m) => m.t === "error" && m.error === "invalid"),
  );
  await expectNothing("relay delivered", () => signals(bSeen).length);
  assert.ok(aId && bId);

  // Oversize transport frame (>64 KiB): the socket dies, the peer is untouched.
  const big = "y".repeat(70 * 1024);
  const closed = new Promise((resolve) => {
    aWs.once("close", resolve);
    aWs.once("error", resolve);
  });
  aWs.send(big);
  await Promise.race([
    closed,
    new Promise((_, reject) => setTimeout(() => reject(new Error("oversize: socket survived")), 4000)),
  ]);

  bWs.close();
  console.log("limits ok");
} finally {
  await srv.kill();
}
