// 8-viewer cap: pruned ghosts (leave/kick/timeout) must not eat capacity.
// Fills a Broadcast room to the cap, proves the 9th ask is refused, then
// proves an explicit leave frees exactly one slot for a fresh ask.
import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const PORT = 18795;
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
    mode: "broadcast",
  });
  assert.equal(open.status, 200);
  const code = open.json.code;

  const tokens = [];
  for (let i = 0; i < 8; i += 1) {
    const ask = await post("/v1/viewer/ask", {
      code,
      nickname: `Viewer${i}`,
      password: "secret1",
    });
    assert.equal(ask.status, 200);
    assert.equal(ask.json.status, "accepted");
    tokens.push(ask.json.viewer_token);
  }
  const ninth = await post("/v1/viewer/ask", {
    code,
    nickname: "Ninth",
    password: "secret1",
  });
  assert.equal(ninth.status, 429, "9th viewer must be refused");
  assert.equal(ninth.json.error, "full");

  const leave = await post("/v1/member/leave", { token: tokens[0] });
  assert.equal(leave.status, 200);
  const tenth = await post("/v1/viewer/ask", {
    code,
    nickname: "Tenth",
    password: "secret1",
  });
  assert.equal(tenth.status, 200, "leave must free one viewer slot");
  assert.equal(tenth.json.status, "accepted");

  console.log("viewer cap ok");
} finally {
  child.kill("SIGTERM");
}
