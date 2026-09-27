// Trusted proxy identity is opt-in: only the configured peer may bind the
// X-Real-IP header to request, rate-limit, and WebSocket identity.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import http from "node:http";
import WebSocket from "ws";
import { spawnServer } from "./test_helpers.mjs";

const IDENTITY_HEADER = "X-Real-IP";
const SPOOFED_HEADER = "X-GoDrinking-Client-IP";
const visitorA = "198.51.100.10";
const visitorB = "198.51.100.20";

function post(base, path, body, headers = {}) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  }).then(async (res) => {
    let json = null;
    try {
      json = await res.json();
    } catch {
      // Status assertions remain useful for non-JSON failures.
    }
    return { status: res.status, json };
  });
}

function postRawHeaders(base, headers) {
  const url = new URL("/v1/rooms", base);
  return new Promise((resolve, reject) => {
    const req = http.request({
      hostname: url.hostname,
      port: url.port,
      method: "POST",
      path: url.pathname,
      headers: [...headers, "Content-Type", "application/json", "Content-Length", "2"],
    }, (res) => {
      res.resume();
      res.once("end", () => resolve(res.statusCode));
    });
    req.once("error", reject);
    req.end("{}");
  });
}

function connectWithIdentity(base, token, clientIp) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${base.replace("http", "ws")}/ws?token=${encodeURIComponent(token)}`, {
      headers: { [IDENTITY_HEADER]: clientIp, [SPOOFED_HEADER]: visitorA },
    });
    const timer = setTimeout(() => {
      ws.terminate();
      reject(new Error("WS open timeout"));
    }, 4000);
    ws.once("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

async function ipv6Available() {
  const probe = http.createServer();
  try {
    await new Promise((resolve, reject) => {
      probe.once("error", reject);
      probe.listen(0, "::1", resolve);
    });
    return true;
  } catch (error) {
    if (["EADDRNOTAVAIL", "EAFNOSUPPORT", "ENETUNREACH"].includes(error.code)) return false;
    throw error;
  } finally {
    if (probe.listening) await new Promise((resolve) => probe.close(resolve));
  }
}

async function spawnIPv6Server() {
  const child = spawn(process.execPath, ["server.mjs"], {
    cwd: new URL(".", import.meta.url),
    env: { ...process.env, PORT: "0", BIND: "::1", TRUSTED_PROXY_PEER: "::1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let logs = "";
  child.stdout.on("data", (buf) => { logs += String(buf); });
  child.stderr.on("data", (buf) => { logs += String(buf); });
  const base = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`IPv6 server start timeout\n${logs}`)), 5000);
    child.stdout.on("data", (buf) => {
      const match = String(buf).match(/listen ::1:(\d+)/);
      if (match) {
        clearTimeout(timer);
        resolve(`http://[::1]:${match[1]}`);
      }
    });
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`IPv6 server exited early: ${code}\n${logs}`)));
  });
  const kill = async () => {
    child.kill("SIGTERM");
    await new Promise((resolve) => {
      const timer = setTimeout(() => { child.kill("SIGKILL"); resolve(); }, 2000);
      child.once("exit", () => { clearTimeout(timer); resolve(); });
    });
  };
  return { base, kill };
}

const trusted = await spawnServer({ TRUSTED_PROXY_PEER: "127.0.0.1" });
let ws;
try {
  const { base } = trusted;

  // Five failed joins attributed to visitor A must not lock out visitor B.
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const denied = await post(base, "/v1/rooms/ZZZZZ1/join", {
      nickname: "Visitor A",
      password: "wrong-password",
    }, { [IDENTITY_HEADER]: visitorA, [SPOOFED_HEADER]: visitorB });
    assert.equal(denied.status, 404);
  }

  const invalidCreate = await post(base, "/v1/rooms", {}, {
    [IDENTITY_HEADER]: visitorB,
    [SPOOFED_HEADER]: visitorB,
  });
  assert.equal(invalidCreate.status, 400, "visitor B should not inherit visitor A's auth ban");

  const created = await post(base, "/v1/rooms", {
    nickname: "Visitor B",
    password: "secret1",
  }, { [IDENTITY_HEADER]: visitorB, [SPOOFED_HEADER]: visitorA });
  assert.equal(created.status, 200);
  assert.ok(created.json?.token);
  ws = await connectWithIdentity(base, created.json.token, visitorB);

  // On a trusted peer, absent, ambiguous, or invalid values fail closed. A
  // subsequent valid identity remains usable, proving bad headers did not
  // mutate the shared proxy identity's lockout state.
  const invalidHeaders = [
    [],
    [SPOOFED_HEADER, visitorB],
    [IDENTITY_HEADER, visitorA, IDENTITY_HEADER, visitorB],
    [IDENTITY_HEADER, `${visitorA}, ${visitorB}`],
    [IDENTITY_HEADER, "not-an-ip"],
  ];
  for (const rawHeaders of invalidHeaders) {
    assert.equal(await postRawHeaders(base, rawHeaders), 400);
    const unaffected = await post(base, "/v1/rooms", {
      nickname: "Visitor B",
      password: "secret1",
    }, { [IDENTITY_HEADER]: visitorB, [SPOOFED_HEADER]: visitorA });
    assert.equal(unaffected.status, 200);
  }
} finally {
  ws?.close();
  await trusted.kill();
}

// An untrusted TCP peer cannot choose its identity. Despite sending distinct
// client-IP values, its requests share the actual peer identity and the
// fifth failed join consequently locks out the following create.
const untrusted = await spawnServer({ TRUSTED_PROXY_PEER: "192.0.2.1" });
try {
  const { base } = untrusted;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const denied = await post(base, "/v1/rooms/ZZZZZ1/join", {
      nickname: "Visitor A",
      password: "wrong-password",
    }, { [IDENTITY_HEADER]: visitorA, [SPOOFED_HEADER]: visitorB });
    assert.equal(denied.status, 404);
  }
  const ignoredHeader = await post(base, "/v1/rooms", {
    nickname: "Visitor B",
    password: "secret1",
  }, { [IDENTITY_HEADER]: visitorB, [SPOOFED_HEADER]: visitorB });
  assert.equal(ignoredHeader.status, 404, "untrusted peer's identity headers must be ignored");
} finally {
  await untrusted.kill();
}

if (await ipv6Available()) {
  const ipv6Proxy = await spawnIPv6Server();
  try {
    const { base } = ipv6Proxy;
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const denied = await post(base, "/v1/rooms/ZZZZZ1/join", {
        nickname: "Visitor A",
        password: "wrong-password",
      }, { [IDENTITY_HEADER]: visitorA, [SPOOFED_HEADER]: visitorB });
      assert.equal(denied.status, 404);
    }
    const unaffected = await post(base, "/v1/rooms", {
      nickname: "Visitor B",
      password: "secret1",
    }, { [IDENTITY_HEADER]: visitorB, [SPOOFED_HEADER]: visitorA });
    assert.equal(unaffected.status, 200, "IPv6 trusted proxy must separate visitor identities");
  } finally {
    await ipv6Proxy.kill();
  }
} else {
  console.log("trusted proxy IPv6 test skipped: ::1 is unavailable");
}

console.log("trusted proxy identity ok");
