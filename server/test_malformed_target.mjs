import assert from "node:assert/strict";
import net from "node:net";
import { spawnServer } from "./test_helpers.mjs";

const SOCKET_TIMEOUT_MS = 2000;

function sendMalformedRequest(base, upgrade) {
  const { port } = new URL(base);
  const request = upgrade
    ? "GET //[ HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    : "GET //[ HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";

  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: "127.0.0.1", port: Number(port) });
    let settled = false;
    const finish = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      if (error) reject(error);
      else resolve();
    };
    const timer = setTimeout(() => finish(new Error("malformed request socket timeout")), SOCKET_TIMEOUT_MS);
    socket.setTimeout(SOCKET_TIMEOUT_MS, () => finish(new Error("malformed request socket timeout")));
    socket.once("error", () => finish());
    socket.once("close", () => finish());
    socket.once("data", () => finish());
    socket.once("connect", () => socket.write(request));
  });
}

async function checkTarget(upgrade) {
  const srv = await spawnServer();
  try {
    await sendMalformedRequest(srv.base, upgrade);
    assert.equal(srv.child.exitCode, null, `${upgrade ? "upgrade" : "GET"} malformed target must not crash the server`);

    let response;
    try {
      response = await fetch(`${srv.base}/health`, { signal: AbortSignal.timeout(SOCKET_TIMEOUT_MS) });
    } catch {
      assert.fail(`${upgrade ? "upgrade" : "GET"} malformed target must leave /health available`);
    }
    assert.equal(response.status, 200, `${upgrade ? "upgrade" : "GET"} malformed target must leave /health available`);
    await response.arrayBuffer();
    assert.equal(srv.child.exitCode, null, `${upgrade ? "upgrade" : "GET"} malformed target must leave the server running`);
  } finally {
    await srv.kill();
  }
}

const failures = [];
for (const upgrade of [false, true]) {
  try {
    await checkTarget(upgrade);
  } catch (error) {
    failures.push(error);
  }
}
if (failures.length) throw new AggregateError(failures, "malformed target checks failed");
console.log("malformed target ok");
