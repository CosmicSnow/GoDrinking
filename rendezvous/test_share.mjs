// Sala share-flag publish: a sharing member's heartbeat flips share:true/false
// synchronously, and the next roster broadcast echoes share, state, master
// verbatim per member (so the Watch button lights on StartShare).
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18796;
const base = `http://127.0.0.1:${PORT}`;

function post(path, body) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).then(async (res) => ({ status: res.status, json: await res.json() }));
}

function connect(role, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${PORT}/v1/ws?role=${role}&token=${token}`);
    const timer = setTimeout(() => reject(new Error(`${role} WS timeout`)), 4000);
    ws.once("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.once("error", reject);
  });
}

function collect(ws) {
  const messages = [];
  ws.on("message", (data) => messages.push(JSON.parse(String(data))));
  return messages;
}

async function waitForRoster(messages, predicate, what) {
  const deadline = Date.now() + 4000;
  for (;;) {
    const rosters = messages.filter((msg) => msg.t === "roster");
    const hit = rosters.find(predicate);
    if (hit) return hit;
    if (Date.now() > deadline) throw new Error(`roster timeout: ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

const child = spawn(process.execPath, ["server.mjs"], {
  cwd: here,
  env: { ...process.env, PORT: String(PORT), BIND: "127.0.0.1" },
  stdio: ["ignore", "pipe", "pipe"],
});

await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("server start timeout")), 4000);
  child.stdout.on("data", (buf) => {
    if (String(buf).includes("listen")) {
      clearTimeout(timer);
      resolve();
    }
  });
  child.on("error", reject);
});

try {
  const open = await post("/v1/host/open", {
    nickname: "Ada",
    password: "secret1",
    mode: "room",
  });
  assert.equal(open.status, 200);
  const code = open.json.code;
  const hostToken = open.json.host_token;
  const masterId = open.json.member_id;

  const bob = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(bob.json.status, "accepted");
  const bobMemberId = bob.json.member_id;

  const hostWs = await connect("host", hostToken);
  const bobWs = await connect("viewer", bob.json.viewer_token);
  const hostMsgs = collect(hostWs);
  const bobMsgs = collect(bobWs);

  // Baseline: nobody sharing yet.
  const baseline = await waitForRoster(
    bobMsgs,
    (msg) => msg.entries.some((entry) => entry.id === masterId),
    "baseline roster",
  );
  assert.equal(
    baseline.entries.find((entry) => entry.id === masterId).share,
    false,
  );

  // Bob starts sharing via the synchronous share heartbeat.
  const start = await post("/v1/member/heartbeat", { token: bob.json.viewer_token, share: true });
  assert.equal(start.status, 200);
  const sharing = await waitForRoster(
    hostMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === bobMemberId && entry.share === true),
    "share:true roster",
  );
  const bobSharing = sharing.entries.find((entry) => entry.id === bobMemberId);
  assert.equal(bobSharing.share, true);
  assert.equal(bobSharing.state, "sharing");
  assert.equal(bobSharing.master, false);
  // Verbatim echo for the non-sharing master in the same broadcast.
  const masterStill = sharing.entries.find((entry) => entry.id === masterId);
  assert.equal(masterStill.share, false);
  assert.equal(masterStill.state, "accepted");
  assert.equal(masterStill.master, true);
  // The sharer sees the same verbatim broadcast on their own socket.
  const sharingEcho = await waitForRoster(
    bobMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === bobMemberId && entry.share === true),
    "share:true echo",
  );
  assert.equal(
    sharingEcho.entries.find((entry) => entry.id === bobMemberId).state,
    "sharing",
  );

  // Bob stops: next roster broadcast flips back to false.
  const stop = await post("/v1/member/heartbeat", { token: bob.json.viewer_token, share: false });
  assert.equal(stop.status, 200);
  const stopped = await waitForRoster(
    hostMsgs,
    (msg) =>
      msg.entries.some(
        (entry) => entry.id === bobMemberId && entry.share === false && entry.state === "accepted",
      ),
    "share:false roster",
  );
  assert.equal(stopped.entries.find((entry) => entry.id === bobMemberId).master, false);

  // Host (master) publishes through the host heartbeat path too.
  const hostStart = await post("/v1/host/heartbeat", { host_token: hostToken, share: true });
  assert.equal(hostStart.status, 200);
  const hostSharing = await waitForRoster(
    bobMsgs,
    (msg) => msg.entries.some((entry) => entry.id === masterId && entry.share === true),
    "host share:true roster",
  );
  assert.equal(hostSharing.entries.find((entry) => entry.id === masterId).state, "sharing");
  assert.equal(hostSharing.entries.find((entry) => entry.id === masterId).master, true);
  const hostStop = await post("/v1/host/heartbeat", { host_token: hostToken, share: false });
  assert.equal(hostStop.status, 200);
  await waitForRoster(
    bobMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === masterId && entry.share === false),
    "host share:false roster",
  );

  // Plain heartbeats without a share flag leave the stored flag alone.
  await post("/v1/member/heartbeat", { token: bob.json.viewer_token });
  const calm = await waitForRoster(
    hostMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === bobMemberId && entry.share === false),
    "steady roster",
  );
  assert.ok(calm);

  hostWs.close();
  bobWs.close();
  await post("/v1/member/leave", { host_token: hostToken });
  await post("/v1/member/leave", { token: bob.json.viewer_token });

  console.log("share mode ok");
} finally {
  child.kill("SIGTERM");
}
