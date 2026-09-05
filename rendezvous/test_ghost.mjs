// Ghost-roster regression: disconnects must shrink the roster fast.
// - broadcast: WS close -> pruned after GHOST_GRACE_MS; explicit leave works;
//   never-connected ask entries die on heartbeat TTL.
// - sala: member WS close -> dropMember path, roster shrinks.
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18792;
const base = `http://127.0.0.1:${PORT}`;

function post(path, body) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).then(async (res) => ({ status: res.status, json: await res.json() }));
}

function connectWs(role, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://127.0.0.1:${PORT}/v1/ws?role=${role}&token=${token}`);
    const timer = setTimeout(() => reject(new Error(`${role} ws timeout`)), 4000);
    ws.on("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.on("error", reject);
  });
}

async function waitFor(label, cond, timeoutMs = 12000) {
  const start = Date.now();
  for (;;) {
    if (cond()) return;
    if (Date.now() - start > timeoutMs) throw new Error(`timeout: ${label}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

const child = spawn(process.execPath, ["server.mjs"], {
  cwd: here,
  env: {
    ...process.env,
    PORT: String(PORT),
    BIND: "127.0.0.1",
    HEARTBEAT_TTL_MS: "15000",
    GHOST_GRACE_MS: "1500",
    GC_INTERVAL_MS: "500",
  },
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
  // ---- broadcast: WS close prunes the ghost ----
  const open = await post("/v1/host/open", { nickname: "Ada", password: "secret1", mode: "broadcast" });
  assert.equal(open.status, 200);
  const code = open.json.code;
  // Keep the room alive past the viewer TTL (heartbeat 5/min < limit 6/min).
  const hb = setInterval(() => {
    post("/v1/host/heartbeat", { host_token: open.json.host_token }).catch(() => {});
  }, 12000);
  const hostMsgs = [];
  const hostWs = await connectWs("host", open.json.host_token);
  hostWs.on("message", (d) => hostMsgs.push(JSON.parse(d.toString())));
  const lastRoster = () => [...hostMsgs].reverse().find((m) => m.t === "roster");

  const v = await post("/v1/viewer/ask", { code, nickname: "Vic", password: "secret1" });
  assert.equal(v.json.status, "accepted");
  const vWs = await connectWs("viewer", v.json.viewer_token);
  await waitFor("roster shows Vic", () => lastRoster()?.entries.length === 1);
  vWs.close();
  await waitFor("roster drops Vic after WS close", () => lastRoster()?.entries.length === 0);
  console.log("broadcast ws-close ghost pruned ok");

  // ---- broadcast: explicit leave drops immediately ----
  const w = await post("/v1/viewer/ask", { code, nickname: "Wil", password: "secret1" });
  assert.equal(w.json.status, "accepted");
  await waitFor("roster shows Wil", () => lastRoster()?.entries.length === 1);
  const leave = await post("/v1/member/leave", { token: w.json.viewer_token });
  assert.equal(leave.status, 200);
  await waitFor("roster drops Wil after leave", () => lastRoster()?.entries.length === 0);
  console.log("broadcast explicit leave ok");

  // ---- broadcast: never-connected ask dies on heartbeat TTL ----
  const x = await post("/v1/viewer/ask", { code, nickname: "Xan", password: "secret1" });
  assert.equal(x.json.status, "accepted");
  await waitFor("roster shows Xan", () => lastRoster()?.entries.length === 1);
  await waitFor("roster drops stale Xan", () => lastRoster()?.entries.length === 0, 30000);
  clearInterval(hb);
  console.log("broadcast stale ask pruned ok");
  hostWs.close();
  await post("/v1/host/close", { host_token: open.json.host_token });

  // ---- sala: member WS close drops via dropMember ----
  const open2 = await post("/v1/host/open", { nickname: "Ada", password: "secret1", mode: "room" });
  assert.equal(open2.status, 200);
  const hostMsgs2 = [];
  const hostWs2 = await connectWs("host", open2.json.host_token);
  hostWs2.on("message", (d) => hostMsgs2.push(JSON.parse(d.toString())));
  const lastRoster2 = () => [...hostMsgs2].reverse().find((m) => m.t === "roster");
  const m = await post("/v1/viewer/ask", { code: open2.json.code, nickname: "Bob", password: "secret1" });
  const mWs = await connectWs("viewer", m.json.viewer_token);
  await waitFor("sala roster shows Bob", () => lastRoster2()?.entries.some((e) => e.id === m.json.member_id));
  mWs.close();
  await waitFor("sala roster drops Bob after WS close", () => !lastRoster2()?.entries.some((e) => e.id === m.json.member_id));
  console.log("sala ws-close member pruned ok");
  hostWs2.close();

  console.log("ghost roster ok");
} finally {
  child.kill("SIGTERM");
}
