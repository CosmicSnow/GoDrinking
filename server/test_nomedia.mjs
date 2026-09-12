// Media boundary: only the documented routes exist, the only dependency is
// `ws`, the source has no media/transport surface, and no secret (password,
// token, SDP, candidate) ever reaches the logs.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import {
  spawnServer,
  post,
  rawRequest,
  connectOk,
  connectFails,
  collect,
  signals,
  waitFor,
} from "./test_helpers.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const MARK = Math.random().toString(36).slice(2, 10);

const srv = await spawnServer();
try {
  const { base } = srv;

  const health = await rawRequest(base, "GET", "/health");
  assert.equal(health.status, 200);

  // Every non-documented surface is denied: media-ish POSTs, media-ish
  // paths under a room, wrong methods, unknown routes.
  for (const [method, path, body] of [
    ["POST", "/v1/media", {}],
    ["POST", "/v1/upload", {}],
    ["POST", "/v1/frames", {}],
    ["POST", "/v1/diagnostics", {}],
    ["POST", "/v1/recording", {}],
    ["POST", "/v1/turn", {}],
    ["GET", "/v1/rooms", undefined],
    ["PUT", "/v1/rooms", {}],
    ["POST", "/v1/rooms/ABCDEF/offer", {}],
    ["POST", "/v1/rooms/ABCDEF/answer", {}],
    ["POST", "/nope", {}],
  ]) {
    const res = await rawRequest(base, method, path, body);
    assert.ok(
      res.status === 404 || res.status === 400,
      `${method} ${path} must not exist (got ${res.status})`,
    );
  }

  // WS without a token is rejected at upgrade.
  await connectFails(base, "");

  // Only `ws` is a dependency.
  const pkg = JSON.parse(await readFile(`${here}/package.json`, "utf8"));
  assert.deepEqual(Object.keys(pkg.dependencies ?? {}), ["ws"]);

  // No media/transport surface in the server source.
  const src = await readFile(`${here}/server.mjs`, "utf8");
  for (const banned of [
    "node:dgram",
    "dgram",
    "RTCPeerConnection",
    "mediasoup",
    "ffmpeg",
    "gstreamer",
    "createSocket",
  ]) {
    assert.ok(!src.includes(banned), `server source must not contain ${banned}`);
  }

  // A full signaling round leaves no secret in the logs.
  const password = `pw-${MARK}-secret`;
  const created = await post(base, "/v1/rooms", { nickname: "Ada", password });
  const joined = await post(base, `/v1/rooms/${created.json.code}/join`, {
    nickname: "Bob",
    password,
  });
  const aWs = await connectOk(base, created.json.token);
  const bWs = await connectOk(base, joined.json.token);
  const aSeen = collect(aWs);
  const bSeen = collect(bWs);
  const sdpMark = `SDP-${MARK}`;
  const candMark = `CAND-${MARK}`;
  aWs.send(
    JSON.stringify({
      t: "offer",
      to: joined.json.memberId,
      payload: {
        type: "offer",
        session: "s",
        share: "sh",
        link: "l",
        attempt: 1,
        sdp: sdpMark,
      },
    }),
  );
  await waitFor("offer", () => signals(bSeen).find((m) => m.payload?.sdp === sdpMark));
  bWs.send(
    JSON.stringify({
      t: "candidate",
      to: created.json.memberId,
      payload: {
        type: "candidate",
        session: "s",
        share: "sh",
        link: "l",
        attempt: 1,
        candidate: `${candMark} typ host`,
      },
    }),
  );
  await waitFor("candidate echo", () =>
    signals(aSeen).find((m) => m.payload?.candidate === `${candMark} typ host`),
  );
  const logs = srv.logs();
  for (const secret of [password, created.json.token, joined.json.token, sdpMark, candMark]) {
    assert.ok(!logs.includes(secret), "logs must not contain secrets or media payloads");
  }

  aWs.close();
  bWs.close();
  console.log("nomedia ok");
} finally {
  await srv.kill();
}
