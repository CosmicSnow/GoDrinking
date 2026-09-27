// Candidate budget is retained for a link even when opaque attempt ids recur.
import assert from "node:assert/strict";
import { spawnServer, post, connectOk, waitFor } from "./test_helpers.mjs";

const srv = await spawnServer();
let hostWs;
let peerWs;
const errors = [];
let forwardedOffers = 0;
let forwardedCandidates = 0;

try {
  const { base } = srv;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password: "secret1" });
  assert.equal(created.status, 200);
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password: "secret1",
  });
  assert.equal(joined.status, 200);

  hostWs = await connectOk(base, created.json.token);
  peerWs = await connectOk(base, joined.json.token);
  hostWs.on("message", (data) => {
    try {
      const message = JSON.parse(String(data));
      if (message.t === "error") errors.push(message.error);
    } catch {
      // Ignore non-JSON frames.
    }
  });
  peerWs.on("message", (data) => {
    try {
      const message = JSON.parse(String(data));
      if (message.t !== "signal") return;
      if (message.payload?.type === "offer") forwardedOffers += 1;
      if (message.payload?.type === "candidate") forwardedCandidates += 1;
    } catch {
      // Ignore non-JSON frames; retain counters only, never signaling payloads.
    }
  });

  const sendOffer = (attempt) => hostWs.send(JSON.stringify({
    t: "offer",
    to: joined.json.memberId,
    payload: {
      type: "offer",
      session: "budget-test",
      share: "budget-test",
      link: "budget-link",
      attempt,
      sdp: "v=0",
    },
  }));
  const sendCandidate = (attempt, id) => hostWs.send(JSON.stringify({
    t: "candidate",
    to: joined.json.memberId,
    payload: {
      type: "candidate",
      session: "budget-test",
      share: "budget-test",
      link: "budget-link",
      attempt,
      candidate: `candidate:${id} 1 udp 2122260223 192.0.2.1 ${10000 + id} typ host`,
    },
  }));

  sendOffer("alpha");
  await waitFor("alpha offer forwarded", () => forwardedOffers === 1);
  for (let id = 1; id <= 64; id += 1) sendCandidate("alpha", id);
  await waitFor("64 candidates forwarded", () => forwardedCandidates === 64);
  assert.deepEqual(errors, [], "the first 64 distinct candidates should be accepted");

  sendCandidate("alpha", 65);
  await waitFor("65th candidate rejected", () => errors.includes("stale"));
  assert.equal(forwardedCandidates, 64);

  sendOffer("beta");
  await waitFor("beta offer forwarded", () => forwardedOffers === 2);
  sendOffer("alpha");
  await waitFor("revisited alpha offer forwarded", () => forwardedOffers === 3);
  sendCandidate("alpha", 66);
  await waitFor("candidate after revisiting alpha rejected", () => errors.length === 2);
  assert.deepEqual(errors, ["stale", "stale"]);
  assert.equal(forwardedCandidates, 64, "revisiting an attempt must not reset the link budget");

  console.log("attempt budget ok");
} finally {
  hostWs?.close();
  peerWs?.close();
  await srv.kill();
}
