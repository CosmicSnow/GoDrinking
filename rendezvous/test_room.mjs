import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18791;
const base = `http://127.0.0.1:${PORT}`;

function post(path, body) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).then(async (res) => ({ status: res.status, json: await res.json() }));
}

const child = spawn(process.execPath, ["server.mjs"], {
  cwd: here,
  env: { ...process.env, PORT: String(PORT), BIND: "127.0.0.1", HEARTBEAT_TTL_MS: "8000" },
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
  assert.equal(open.json.mode, "room");
  assert.ok(open.json.host_token);
  assert.ok(open.json.member_id);
  const code = open.json.code;
  const hostToken = open.json.host_token;

  const b = await post("/v1/viewer/ask", { code, nickname: "Bob", password: "secret1" });
  assert.equal(b.json.status, "accepted");
  const c = await post("/v1/viewer/ask", { code, nickname: "Cyd", password: "secret1" });
  assert.equal(c.json.status, "accepted");

  const leaveHost = await post("/v1/member/leave", { host_token: hostToken });
  assert.equal(leaveHost.status, 200);

  const hbB = await post("/v1/member/heartbeat", { token: b.json.viewer_token });
  assert.equal(hbB.status, 200);
  assert.equal(hbB.json.master_id, b.json.member_id);

  await post("/v1/member/leave", { token: b.json.viewer_token });
  await post("/v1/member/leave", { token: c.json.viewer_token });

  const gone = await post("/v1/viewer/ask", { code, nickname: "Dan", password: "secret1" });
  assert.equal(gone.status, 404);

  const broadcast = await post("/v1/host/open", {
    nickname: "Ada",
    password: "secret1",
  });
  assert.equal(broadcast.json.mode, "broadcast");

  const open2 = await post("/v1/host/open", {
    nickname: "Ada",
    password: "secret1",
    mode: "room",
  });
  const code2 = open2.json.code;
  const b2 = await post("/v1/viewer/ask", { code: code2, nickname: "Bob", password: "secret1" });
  assert.equal(b2.status, 200);
  // Member-to-member signal is accepted by the HTTP-less WS path; the REST
  // surface only needs to keep the room alive for that.
  await post("/v1/member/heartbeat", { token: b2.json.viewer_token });
  await post("/v1/member/leave", { host_token: open2.json.host_token });
  await post("/v1/member/leave", { token: b2.json.viewer_token });

  const open3 = await post("/v1/host/open", {
    nickname: "Ada",
    password: "secret1",
    mode: "room",
  });
  const code3 = open3.json.code;
  const masterId = open3.json.member_id;
  const hostWs = new WebSocket(`ws://127.0.0.1:${PORT}/v1/ws?role=host&token=${open3.json.host_token}`);
  const hostMsgs = [];
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("host ws timeout")), 4000);
    hostWs.on("open", () => {
      clearTimeout(timer);
      resolve();
    });
    hostWs.on("error", reject);
  });
  hostWs.on("message", (data) => hostMsgs.push(JSON.parse(String(data))));
  const bob = await post("/v1/viewer/ask", { code: code3, nickname: "Bob", password: "secret1" });
  const bobWs = new WebSocket(`ws://127.0.0.1:${PORT}/v1/ws?role=viewer&token=${bob.json.viewer_token}`);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("bob ws timeout")), 4000);
    bobWs.on("open", () => {
      clearTimeout(timer);
      resolve();
    });
    bobWs.on("error", reject);
  });
  bobWs.send(JSON.stringify({ t: "watch", to: masterId }));
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.ok(
    hostMsgs.some((msg) => msg.t === "watch" && msg.from === bob.json.member_id && msg.to === masterId),
    "host should receive watch",
  );
  bobWs.send(JSON.stringify({ t: "unwatch", to: masterId }));
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.ok(
    hostMsgs.some((msg) => msg.t === "unwatch" && msg.from === bob.json.member_id),
    "host should receive unwatch",
  );
  hostWs.close();
  bobWs.close();
  await post("/v1/member/leave", { host_token: open3.json.host_token });
  await post("/v1/member/leave", { token: bob.json.viewer_token });

  console.log("room mode ok");
} finally {
  child.kill("SIGTERM");
}
