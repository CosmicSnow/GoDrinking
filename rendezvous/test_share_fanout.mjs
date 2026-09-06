// Sala share-flag fanout: member A opens the Sala and starts sharing; member
// B (joining from the SAME IP, as on one PC) must see A share:true/state
// sharing with verbatim master on B's next roster broadcast, and share:false
// after A stops. Every share-flip must reach hostWs + ALL member sockets, and
// the heartbeat echo must carry share/state/master verbatim.
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18797;
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
    const hit = messages.filter((msg) => msg.t === "roster").find(predicate);
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
  // A opens the Sala and connects the host socket.
  const open = await post("/v1/host/open", {
    nickname: "Ana",
    password: "secret1",
    mode: "room",
  });
  assert.equal(open.status, 200);
  const code = open.json.code;
  const hostToken = open.json.host_token;
  const memberA = open.json.member_id;
  const hostWs = await connect("host", hostToken);
  const hostMsgs = collect(hostWs);

  // A starts sharing (the Rust host synchronous-publish path).
  const start = await post("/v1/host/heartbeat", { host_token: hostToken, share: true });
  assert.equal(start.status, 200);
  // Heartbeat echo carries the roster verbatim.
  const echoStart = start.json.entries.find((entry) => entry.id === memberA);
  assert.equal(echoStart.share, true);
  assert.equal(echoStart.state, "sharing");
  assert.equal(echoStart.master, true);

  // B joins from the SAME IP as A; a third member reuses A's nickname.
  // Member ids must still differ: uniqueness is independent of IP/nickname.
  const bob = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(bob.json.status, "accepted");
  const memberB = bob.json.member_id;
  const cyd = await post("/v1/viewer/ask", { code, nickname: "Ana", password: "secret1" });
  assert.equal(cyd.json.status, "accepted");
  const memberC = cyd.json.member_id;
  assert.notEqual(memberA, memberB, "same-IP members must have distinct memberIds");
  assert.notEqual(memberA, memberC, "same-nickname members must have distinct memberIds");
  assert.notEqual(memberB, memberC, "members must have distinct memberIds");

  const bobWs = await connect("viewer", bob.json.viewer_token);
  const bobMsgs = collect(bobWs);

  // B's next roster broadcast shows A sharing with verbatim master, and B
  // verbatim non-sharing.
  const sharing = await waitForRoster(
    bobMsgs,
    (msg) => msg.entries.some((entry) => entry.id === memberA && entry.share === true),
    "B sees A share:true",
  );
  const entryA = sharing.entries.find((entry) => entry.id === memberA);
  assert.equal(entryA.share, true);
  assert.equal(entryA.state, "sharing");
  assert.equal(entryA.master, true);
  const entryB = sharing.entries.find((entry) => entry.id === memberB);
  assert.equal(entryB.share, false);
  assert.equal(entryB.state, "accepted");
  assert.equal(entryB.master, false);

  // The flip fanned out to the host socket too, not only to members.
  const hostFlip = await waitForRoster(
    hostMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === memberA && entry.share === true) &&
      msg.entries.some((entry) => entry.id === memberB),
    "host sees A share:true",
  );
  assert.equal(hostFlip.entries.find((entry) => entry.id === memberB).share, false);

  // B's own heartbeat echo carries A's share:true verbatim.
  const bobHb = await post("/v1/member/heartbeat", { token: bob.json.viewer_token });
  assert.equal(bobHb.status, 200);
  const bobEchoA = bobHb.json.entries.find((entry) => entry.id === memberA);
  assert.equal(bobEchoA.share, true);
  assert.equal(bobEchoA.state, "sharing");
  assert.equal(bobEchoA.master, true);

  // A stops: share:false is visible to B on the broadcast and on the echo.
  const stop = await post("/v1/host/heartbeat", { host_token: hostToken, share: false });
  assert.equal(stop.status, 200);
  assert.equal(
    stop.json.entries.find((entry) => entry.id === memberA).share,
    false,
  );
  const stopped = await waitForRoster(
    bobMsgs,
    (msg) =>
      msg.entries.some(
        (entry) => entry.id === memberA && entry.share === false && entry.state === "accepted",
      ),
    "B sees A share:false",
  );
  assert.equal(stopped.entries.find((entry) => entry.id === memberA).master, true);
  const hostCalm = await waitForRoster(
    hostMsgs,
    (msg) =>
      msg.entries.some((entry) => entry.id === memberA && entry.share === false),
    "host sees A share:false",
  );
  assert.ok(hostCalm);

  hostWs.close();
  bobWs.close();
  await post("/v1/member/leave", { host_token: hostToken });
  await post("/v1/member/leave", { token: bob.json.viewer_token });
  await post("/v1/member/leave", { token: cyd.json.viewer_token });

  console.log("share fanout ok");
} finally {
  child.kill("SIGTERM");
}
