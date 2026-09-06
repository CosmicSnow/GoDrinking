// LOCAL-ONLY visual E2E: host Start→Share, joiner Join→Watch→first frame.
// Runs against two packaged instances via multiremote (`host`, `joiner`).
// See docs/VISUAL_E2E.md for setup. Never runs in CI (see ../run.mjs).
import { strict as assert } from "node:assert";
import { mkdirSync, writeFileSync } from "node:fs";

const RENDEZVOUS = process.env.GODRINKING_RENDEZVOUS ?? "";
const PASSWORD = process.env.GODRINKING_PASSWORD ?? "visual-e2e-1";
const ARTIFACTS = new URL("../artifacts/", import.meta.url);

async function shot(browser, name) {
  mkdirSync(ARTIFACTS, { recursive: true });
  await browser.saveScreenshot(new URL(`./${name}.png`, ARTIFACTS).pathname);
}

async function setInput(browser, selector, value) {
  const el = await browser.$(selector);
  await el.waitForDisplayed({ timeout: 15000 });
  await el.setValue(value);
}

async function clickText(browser, selector, text) {
  const els = await browser.$$(selector);
  for (const el of els) {
    if ((await el.getText()).includes(text)) {
      await el.click();
      return;
    }
  }
  throw new Error(`no ${selector} with text "${text}"`);
}

describe("Sala watch (visual, local only)", () => {
  it("host shares, joiner watches to first frame", async () => {
    assert.ok(RENDEZVOUS, "set GODRINKING_RENDEZVOUS to the Stunar URL");
    const { host, joiner } = browser;

    // --- Host: Share tab (default) → Stunar + Room → Open room → Share. ---
    await clickText(host, ".segmented button", "Stunar");
    await setInput(host, "#share-nickname", "E2EHost");
    await setInput(host, "#share-rendezvous", RENDEZVOUS);
    await setInput(host, "#share-password", PASSWORD);
    await clickText(host, ".segmented button", "Room");
    await shot(host, "01-host-room-form");
    await host.$(".controls-panel .primary-cta, .panel .primary-cta").click();
    const codeEl = await host.$(".signal-input");
    await codeEl.waitForExist({ timeout: 30000 });
    const code = (await codeEl.getValue()).trim().slice(0, 6);
    assert.match(code, /^[A-Z0-9]{4,}$/);
    await shot(host, "02-host-room-open");

    const shareBtn = await host.$(".room-share-btn");
    await shareBtn.waitForClickable({ timeout: 30000 });
    await shareBtn.click();
    await shot(host, "03-host-sharing");

    // --- Joiner: Watch tab → Stunar → Join → Watch enabled → click. ---
    await clickText(joiner, ".nav-item", "Watch");
    await clickText(joiner, ".segmented button", "Stunar");
    await setInput(joiner, "#join-rendezvous", RENDEZVOUS);
    await setInput(joiner, "#join-code", code);
    await setInput(joiner, "#join-nickname", "E2EJoiner");
    await setInput(joiner, "#join-password", PASSWORD);
    await shot(joiner, "04-joiner-form");
    await joiner.$(".controls-panel .primary-cta").click();

    const watchBtn = await joiner.$(".room-watch");
    await watchBtn.waitForExist({ timeout: 60000 });
    await shot(joiner, "05-joiner-rail");
    assert.equal(await watchBtn.isEnabled(), true);
    await watchBtn.click();

    // --- Video slot live: a remote video element with real frames. ---
    await joiner.waitUntil(
      async () => {
        const slots = await joiner.$$("video[data-slot]");
        for (const slot of slots) {
          const ready = await joiner.execute(
            (el) => el.readyState,
            slot,
          );
          if (ready >= 2) return true;
        }
        return false;
      },
      { timeout: 90000, timeoutMsg: "no live video slot on the joiner" },
    );
    await shot(host, "06-host-live");
    await shot(joiner, "06-joiner-live");

    // --- Milestone + offer→answer→IDR evidence to artifacts. ---
    const logs = await joiner.getLogs("browser").catch(() => []);
    const text = logs.map((entry) => entry.message ?? "").join("\n");
    mkdirSync(ARTIFACTS, { recursive: true });
    writeFileSync(new URL("./joiner-console.log", ARTIFACTS), text);
    assert.match(text, /viewer-milestone.*ontrack-fired/);
  });
});
