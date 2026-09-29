import fs from "node:fs";
import path from "node:path";

// Only internal, fixed categories. Never pass client-provided values to record().
const EVENTS = new Map([
  ["startup", new Set(["started"])],
  ["shutdown", new Set(["stopped"])],
  ["room", new Set(["removed"])],
  ["auth", new Set(["denied", "full", "ignored"])],
  ["share", new Set(["announced", "stopped"])],
  ["websocket", new Set(["connected", "closed", "replaced", "ping_timeout", "send_error"])],
  ["grace_expiry", new Set(["expired"])],
  ["heartbeat_expiry", new Set(["expired"])],
  ["pressure", new Set(["inbound_limit", "control_limit", "upgrade_limit", "request_limit", "global_limit", "outbound_limit"])],
  ["watch", new Set(["rejected", "target_offline", "forwarded"])],
  ["signal", new Set(["rejected", "target_offline", "forwarded", "offer", "answer"])],
]);
const WINDOW_MS = 10_000;
const MAX_BYTES = 10 * 1024 * 1024;
const ROTATIONS = 3;

export function createDiagnostics(filePath) {
  const counts = new Map();
  let warningSent = false;
  let failed = false;
  const warnOnce = () => {
    if (warningSent) return;
    warningSent = true;
    console.warn("diagnostic log unavailable");
  };
  const flush = () => {
    if (!filePath || failed || counts.size === 0) return;
    const entries = [...counts.entries()];
    counts.clear();
    try {
      fs.mkdirSync(path.dirname(filePath), { recursive: true, mode: 0o700 });
      let existing = 0;
      try { existing = fs.statSync(filePath).size; } catch {}
      const lines = entries.map(([key, count]) => {
        const [event, reason] = key.split("\0");
        return JSON.stringify({ ts: new Date().toISOString(), event, reason, count }) + "\n";
      }).join("");
      const incoming = Buffer.byteLength(lines);
      if (existing + incoming > MAX_BYTES) {
        try { fs.unlinkSync(`${filePath}.${ROTATIONS}`); } catch {}
        for (let i = ROTATIONS - 1; i >= 1; i -= 1) {
          try { fs.renameSync(`${filePath}.${i}`, `${filePath}.${i + 1}`); } catch {}
        }
        try { fs.renameSync(filePath, `${filePath}.1`); } catch {}
      }
      const fd = fs.openSync(filePath, "a", 0o600);
      try { fs.writeSync(fd, lines); fs.fchmodSync(fd, 0o600); } finally { fs.closeSync(fd); }
    } catch {
      failed = true;
      warnOnce();
    }
  };
  const timer = filePath ? setInterval(flush, WINDOW_MS) : null;
  timer?.unref?.();
  return {
    record(event, reason) {
      if (!filePath || failed || !EVENTS.get(event)?.has(reason)) return;
      const key = `${event}\0${reason}`;
      counts.set(key, (counts.get(key) ?? 0) + 1);
    },
    flush,
    close() { if (timer) clearInterval(timer); flush(); },
  };
}
