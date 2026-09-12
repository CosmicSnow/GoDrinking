// End-to-end signaling between two members: offer/answer/candidate/
// ice-complete, share announcements, watch/unwatch, stale attempts.
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

const SUFFIX = Math.random().toString(36).slice(2, 8);
const SDP_MARK = `SDP-${SUFFIX}`;
const CAND_MARK = `CAND-${SUFFIX}`;

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

  const offer = {
    type: "offer",
    session: "s1",
    share: "sh1",
    link: "link-1",
    attempt: 1,
    sdp: `${SDP_MARK}-offer`,
  };
  aWs.send(JSON.stringify({ t: "offer", to: bId, payload: offer }));
  const gotOffer = await waitFor("offer", () =>
    signals(bSeen).find((m) => m.payload?.type === "offer"),
  );
  assert.equal(gotOffer.from, aId);
  assert.equal(gotOffer.to, bId);
  assert.equal(gotOffer.payload.sdp, `${SDP_MARK}-offer`);

  bWs.send(
    JSON.stringify({
      t: "answer",
      to: aId,
      payload: {
        type: "answer",
        session: "s1",
        share: "sh1",
        link: "link-1",
        attempt: 1,
        sdp: `${SDP_MARK}-answer`,
      },
    }),
  );
  await waitFor("answer", () =>
    signals(aSeen).find((m) => m.payload?.sdp === `${SDP_MARK}-answer`),
  );

  aWs.send(
    JSON.stringify({
      t: "candidate",
      to: bId,
      payload: {
        type: "candidate",
        session: "s1",
        share: "sh1",
        link: "link-1",
        attempt: 1,
        candidate: `${CAND_MARK}-1 typ host`,
      },
    }),
  );
  await waitFor("candidate", () =>
    signals(bSeen).find((m) => m.payload?.candidate === `${CAND_MARK}-1 typ host`),
  );

  bWs.send(
    JSON.stringify({
      t: "ice-complete",
      to: aId,
      payload: { type: "ice-complete", session: "s1", share: "sh1", link: "link-1", attempt: 1 },
    }),
  );
  await waitFor("ice-complete", () =>
    signals(aSeen).find((m) => m.payload?.type === "ice-complete"),
  );

  // Share announcements fan out on the roster.
  aWs.send(JSON.stringify({ t: "announce-share" }));
  const sharing = await waitFor("share:true", () =>
    bSeen.find((m) => m.t === "roster" && m.entries.some((e) => e.id === aId && e.share === true)),
  );
  assert.equal(sharing.master_id, aId);
  aWs.send(JSON.stringify({ t: "stop-share" }));
  await waitFor("share:false", () =>
    bSeen.find(
      (m) =>
        m !== sharing &&
        m.t === "roster" &&
        m.entries.some((e) => e.id === aId && e.share === false),
    ),
  );

  // Watch/unwatch forward statelessly.
  aWs.send(JSON.stringify({ t: "watch", to: bId }));
  const watch = await waitFor("watch", () => bSeen.find((m) => m.t === "watch"));
  assert.equal(watch.from, aId);
  bWs.send(JSON.stringify({ t: "unwatch", to: aId }));
  await waitFor("unwatch", () => aSeen.find((m) => m.t === "unwatch"));

  // Stale attempt: offer 2 makes candidate/answer 1 stale; they are dropped
  // and the sender is told.
  aSeen.length = 0;
  bSeen.length = 0;
  aWs.send(
    JSON.stringify({
      t: "offer",
      to: bId,
      payload: {
        type: "offer",
        session: "s1",
        share: "sh1",
        link: "link-1",
        attempt: 2,
        sdp: `${SDP_MARK}-offer2`,
      },
    }),
  );
  await waitFor("offer 2", () =>
    signals(bSeen).find((m) => m.payload?.sdp === `${SDP_MARK}-offer2`),
  );
  bSeen.length = 0;
  bWs.send(
    JSON.stringify({
      t: "candidate",
      to: aId,
      payload: {
        type: "candidate",
        session: "s1",
        share: "sh1",
        link: "link-1",
        attempt: 1,
        candidate: `${CAND_MARK}-stale typ host`,
      },
    }),
  );
  await waitFor("stale error", () =>
    bSeen.find((m) => m.t === "error" && m.error === "stale"),
  );
  await expectNothing("stale delivered", () =>
    signals(aSeen).find((m) => m.payload?.candidate === `${CAND_MARK}-stale typ host`),
  );

  // No SDP or candidate payload in server logs.
  assert.ok(!srv.logs().includes(SDP_MARK), "logs must not contain SDP");
  assert.ok(!srv.logs().includes(CAND_MARK), "logs must not contain candidates");

  aWs.close();
  bWs.close();
  console.log("signaling ok");
} finally {
  await srv.kill();
}
