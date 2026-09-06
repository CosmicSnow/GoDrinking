// LOCAL-ONLY gate for the visual E2E suite. This scaffold must never run in
// CI: it launches two packaged goDrinking instances (needs a TCC-granted
// .app, a display, and loopback media). Root package.json has no script that
// reaches this directory, so CI cannot pick it up; this gate is defense in
// depth plus a clear error for accidental runs.
import { spawn } from "node:child_process";

if (process.env.GODRINKING_E2E !== "1") {
  console.error(
    "Refusing: visual E2E is LOCAL-ONLY. Re-run with GODRINKING_E2E=1. See docs/VISUAL_E2E.md.",
  );
  process.exit(2);
}
if (process.env.CI === "true" || process.env.CI === "1") {
  console.error("Refusing: visual E2E must NOT run in CI (CI env detected).");
  process.exit(2);
}

const child = spawn("npx", ["wdio", "run", "wdio.conf.mjs"], {
  stdio: "inherit",
  shell: false,
});
child.on("exit", (code) => process.exit(code ?? 1));
