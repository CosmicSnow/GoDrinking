// Shared E2E harness: spawns the server on an ephemeral port (PORT=0) and
// tears it down at the end of each file. No residue between suites.
import { spawn } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import WebSocket from "ws";

const here = dirname(fileURLToPath(import.meta.url));

export async function spawnServer(envExtra = {}) {
  const child = spawn(process.execPath, ["server.mjs"], {
    cwd: here,
    env: { ...process.env, PORT: "0", BIND: "127.0.0.1", ...envExtra },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let logs = "";
  child.stdout.on("data", (buf) => {
    logs += String(buf);
  });
  child.stderr.on("data", (buf) => {
    logs += String(buf);
  });
  const base = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("server start timeout")), 5000);
    child.stdout.on("data", (buf) => {
      const m = String(buf).match(/listen \S+:(\d+)/);
      if (m) {
        clearTimeout(timer);
        resolve(`http://127.0.0.1:${m[1]}`);
      }
    });
    child.on("error", reject);
    child.on("exit", (code) => reject(new Error(`server exited early: ${code}\n${logs}`)));
  });
  const kill = async () => {
    child.kill("SIGTERM");
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        resolve();
      }, 2000);
      child.on("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  };
  return { child, base, logs: () => logs, kill };
}

export function post(base, path, body) {
  return fetch(base + path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  }).then(async (res) => {
    let json = null;
    try {
      json = await res.json();
    } catch {
      // non-JSON body: status is the assertion
    }
    return { status: res.status, json };
  });
}

export function rawRequest(base, method, path, body) {
  return fetch(base + path, {
    method,
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  }).then(async (res) => ({ status: res.status, text: await res.text() }));
}

export function connectOk(base, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${base.replace("http", "ws")}/ws?token=${token}`);
    const timer = setTimeout(() => {
      ws.terminate();
      reject(new Error("WS open timeout"));
    }, 4000);
    ws.once("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.once("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

export function connectFails(base, token) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${base.replace("http", "ws")}/ws?token=${token || "bogus"}`);
    const timer = setTimeout(() => reject(new Error("expected WS rejection, stayed open")), 4000);
    ws.once("open", () => {
      clearTimeout(timer);
      ws.terminate();
      reject(new Error("expected WS rejection, got open"));
    });
    const done = () => {
      clearTimeout(timer);
      resolve();
    };
    ws.once("error", done);
    ws.once("close", done);
  });
}

export function collect(ws) {
  const seen = [];
  ws.on("message", (data) => {
    try {
      seen.push(JSON.parse(String(data)));
    } catch {
      // ignore non-JSON frames
    }
  });
  return seen;
}

export const signals = (seen) => seen.filter((m) => m.t === "signal");

export function waitFor(label, fn, timeoutMs = 4000) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      let value;
      try {
        value = fn();
      } catch (err) {
        reject(err);
        return;
      }
      if (value) {
        resolve(value);
        return;
      }
      if (Date.now() - started > timeoutMs) {
        reject(new Error(`timeout: ${label}`));
        return;
      }
      setTimeout(tick, 25);
    };
    tick();
  });
}

export async function expectNothing(label, fn, ms = 400) {
  await sleep(ms);
  if (fn()) throw new Error(`${label} must be rejected/dropped`);
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
