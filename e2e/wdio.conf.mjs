// WebDriverIO config for LOCAL-ONLY visual E2E (two packaged instances).
// Prerequisites (see docs/VISUAL_E2E.md): tauri-driver listening on
// localhost:4444, two separately-installed app copies (HOST_APP, JOINER_APP),
// GODRINKING_E2E=1. Multiremote drives host + joiner in one spec.
const hostApp = process.env.GODRINKING_HOST_APP ?? "";
const joinerApp = process.env.GODRINKING_JOINER_APP ?? hostApp;

if (!hostApp) {
  throw new Error("Set GODRINKING_HOST_APP to the packaged .app (or binary) path.");
}

const instanceCaps = (application) => ({
  capabilities: {
    browserName: "wry",
    "tauri:options": { application },
  },
});

export const config = {
  runner: "local",
  hostname: "localhost",
  port: 4444,
  path: "/",
  specs: ["./specs/sala-watch.e2e.mjs"],
  maxInstances: 1,
  capabilities: {
    host: instanceCaps(hostApp),
    joiner: instanceCaps(joinerApp),
  },
  logLevel: "warn",
  bail: 0,
  waitforTimeout: 30000,
  connectionRetryTimeout: 120000,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: { ui: "bdd", timeout: 120000 },
};
