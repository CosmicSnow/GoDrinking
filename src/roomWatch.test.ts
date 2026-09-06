// @vitest-environment happy-dom
// Sala rail: joiner Watch affordance — pure helper contracts plus App-level
// interaction tests with a mocked Tauri invoke.
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { mockInvoke } = vi.hoisted(() => ({ mockInvoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onFocusChanged: () => Promise.resolve(() => undefined),
    onResized: () => Promise.resolve(() => undefined),
    isFullscreen: () => Promise.resolve(false),
    setFullscreen: () => Promise.resolve(),
  }),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ revealItemInDir: vi.fn() }));

import App from "./App";
import {
  nextStickyPeople,
  salaRailDiagnostics,
  STICKY_STALE_TICKS,
  watchButtonState,
} from "./RoomStage";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

type FixturePerson = {
  id: string;
  nickname: string;
  state: string;
  master?: boolean;
  share?: boolean;
};

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

async function flush(rounds = 4) {
  for (let i = 0; i < rounds; i += 1) {
    await act(async () => {
      await sleep(20);
    });
  }
}

const CAPS = {
  platform: "macos",
  supported: true,
  native_capture_implemented: true,
  screen_capture_kit: true,
  source_enumeration_available: false,
  screen_recording_authorization: "granted",
  app_audio_exclusion: "unsupported",
  detail: "ok",
};

const snapshotFor = (roster: FixturePerson[]) => ({
  state: "running",
  session_id: null,
  source_id: null,
  bitrate_bps: null,
  native_capture_active: false,
  preview_callback_count: 0,
  preview_frame_count: 0,
  preview_dropped_count: 0,
  preview_error: null,
  detail: "room",
  peer_state: "idle",
  peer_detail: "",
  session_code: "ABC123",
  lan_addresses: [],
  lan_port: null,
  roster,
  self_id: "me",
  session_mode: "room",
  join_mode: "stunar",
});

const hostLive: FixturePerson = {
  id: "host1",
  nickname: "Host",
  state: "connected",
  master: true,
  share: true,
};

function installInvoke(roster: FixturePerson[]) {
  mockInvoke.mockImplementation(async (command: string) => {
    switch (command) {
      case "get_media_capabilities":
        return CAPS;
      case "discover_media_room":
        return ["host1", { type: "offer", sdp: "", id: "host1", offer_attempt: "a1" }, "Host"];
      case "get_media_session_state":
        return snapshotFor(roster);
      case "get_media_preview":
        return null;
      case "poll_stunar_offers":
        return [];
      case "stunar_watch":
        return null;
      default:
        return null;
    }
  });
}

let container: HTMLDivElement | null = null;
let root: Root | null = null;

async function renderApp() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root?.render(createElement(App));
  });
  await flush(2);
}

function setInput(id: string, value: string) {
  const el = document.getElementById(id);
  if (!(el instanceof HTMLInputElement)) throw new Error(`missing input #${id}`);
  // React controlled inputs ignore direct assignment: go through the native
  // setter so onChange fires.
  const nativeSetter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")
    ?.set as ((this: HTMLInputElement, value: string) => void) | undefined;
  if (nativeSetter) nativeSetter.call(el, value);
  else el.value = value;
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

function click(el: Element | null) {
  if (!(el instanceof HTMLElement)) throw new Error("missing clickable element");
  el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
}

async function joinAsWatcher() {
  const navItems = [...(container as HTMLDivElement).querySelectorAll(".nav-item")];
  const watchNav = navItems.find((item) => item.textContent?.includes("Watch"));
  click(watchNav ?? null);
  await flush();
  setInput("join-code", "ABC123");
  setInput("join-nickname", "Joiner");
  setInput("join-password", "secret");
  await flush();
  click((container as HTMLDivElement).querySelector(".controls-panel .primary-cta"));
  await flush(6);
}

const rail = () => (container as HTMLDivElement).querySelector(".room-live-shell");
const watchButtons = () => [...(container as HTMLDivElement).querySelectorAll(".room-watch")];

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem("godrinking.locale", "en");
  localStorage.setItem("godrinking.join_mode", "stunar");
  localStorage.setItem("godrinking.nickname", "Joiner");
  localStorage.setItem("godrinking.rendezvous_url", "https://example.com");
  mockInvoke.mockReset();
});

afterEach(async () => {
  await act(async () => {
    root?.unmount();
  });
  container?.remove();
  container = null;
  root = null;
});

describe("watchButtonState (rail contract)", () => {
  it("enables Watch for a sharing other", () => {
    expect(watchButtonState({ id: "host1", share: true }, new Set())).toEqual({
      watching: false,
      disabled: false,
    });
  });

  it("disables Watch for a non-sharing other", () => {
    expect(watchButtonState({ id: "host1", share: false }, new Set())).toEqual({
      watching: false,
      disabled: true,
    });
  });

  it("keeps an already-watched tile interactive", () => {
    expect(watchButtonState({ id: "host1", share: false }, new Set(["host1"]))).toEqual({
      watching: true,
      disabled: false,
    });
  });
});

describe("salaRailDiagnostics (empty-rail line)", () => {
  it("names self, roster size, and Sala liveness", () => {
    const line = salaRailDiagnostics({
      selfId: "me",
      rosterLength: 0,
      salaAlive: true,
      roomJoined: true,
      sessionMode: "room",
    });
    expect(line).toMatch(/me/);
    expect(line).toMatch(/roster 0/);
    expect(line).toMatch(/alive/);
    expect(line).toMatch(/joined/);
    expect(line).toMatch(/room/);
  });
});

describe("nextStickyPeople (capped staleness, roster-owned master)", () => {
  const self = { id: "me", nickname: "Me", share: false };

  it("never injects self as master on the viewer side", () => {
    const next = nextStickyPeople({ prev: [], incoming: [], self: null, staleTicks: 0 });
    expect(next.people).toEqual([]);
  });

  it("takes master/share from the roster only on the viewer side", () => {
    const next = nextStickyPeople({ prev: [], incoming: [hostLive], self: null, staleTicks: 0 });
    expect(next.people).toEqual([hostLive]);
    expect(next.staleTicks).toBe(0);
  });

  it("retains prev under the cap, then drops stale remotes", () => {
    const stale = { ...hostLive, share: false };
    let state = nextStickyPeople({ prev: [stale], incoming: [], self: null, staleTicks: 0 });
    expect(state.people).toEqual([stale]);
    expect(state.staleTicks).toBe(1);
    state = nextStickyPeople({
      prev: state.people,
      incoming: [],
      self: null,
      staleTicks: STICKY_STALE_TICKS,
    });
    expect(state.people).toEqual([]);
    expect(state.staleTicks).toBe(0);
  });

  it("keeps the host self entry while dropping stale remotes after the cap", () => {
    const selfEntry = { id: "me", nickname: "Me", state: "new", master: true, share: false };
    const stale = { ...hostLive, share: false };
    const retained = nextStickyPeople({
      prev: [selfEntry, stale],
      incoming: [],
      self,
      staleTicks: 0,
    });
    expect(retained.people).toEqual([selfEntry, stale]);
    const dropped = nextStickyPeople({
      prev: retained.people,
      incoming: [],
      self,
      staleTicks: STICKY_STALE_TICKS,
    });
    expect(dropped.people).toEqual([selfEntry]);
  });

  it("injects host self when the roster is empty and nothing is retained", () => {
    const next = nextStickyPeople({ prev: [], incoming: [], self, staleTicks: 0 });
    expect(next.people).toEqual([
      { id: "me", nickname: "Me", state: "new", master: true, share: false },
    ]);
  });
});

describe("Sala rail (mocked invoke)", () => {
  it("share:true other → Watch enabled + click calls stunar_watch{to,start:true}", async () => {
    installInvoke([hostLive]);
    await renderApp();
    await joinAsWatcher();
    expect(rail()).not.toBeNull();
    const buttons = watchButtons();
    expect(buttons).toHaveLength(1);
    expect((buttons[0] as HTMLButtonElement).disabled).toBe(false);
    expect(buttons[0].textContent).toMatch(/Watch/);
    click(buttons[0]);
    await flush();
    expect(mockInvoke).toHaveBeenCalledWith("stunar_watch", {
      request: { to: "host1", start: true },
    });
    expect(
      (container as HTMLDivElement).querySelector(".room-watch.is-on"),
    ).not.toBeNull();
  });

  it("share:false other → Watch disabled and no watch call", async () => {
    installInvoke([{ ...hostLive, share: false }]);
    await renderApp();
    await joinAsWatcher();
    expect(rail()).not.toBeNull();
    const buttons = watchButtons();
    expect(buttons).toHaveLength(1);
    expect((buttons[0] as HTMLButtonElement).disabled).toBe(true);
    expect(mockInvoke).not.toHaveBeenCalledWith(
      "stunar_watch",
      expect.objectContaining({ request: expect.objectContaining({ start: true }) }),
    );
  });

  it("self → hidden from others, no Watch for self", async () => {
    installInvoke([
      { id: "me", nickname: "Me", state: "connected", master: false, share: false },
      hostLive,
    ]);
    await renderApp();
    await joinAsWatcher();
    expect(rail()).not.toBeNull();
    expect(watchButtons()).toHaveLength(1);
    expect(
      (container as HTMLDivElement).querySelector(".room-person.is-you .room-watch"),
    ).toBeNull();
    expect(
      (container as HTMLDivElement).querySelector(".room-person.is-you"),
    ).not.toBeNull();
  });

  it("empty roster → onlyYou + diagnostics visible, no fake master", async () => {
    installInvoke([]);
    await renderApp();
    await joinAsWatcher();
    expect(rail()).not.toBeNull();
    expect(
      (container as HTMLDivElement).querySelector(".roster-empty")?.textContent,
    ).toMatch(/only one here/);
    const diagnostics = (container as HTMLDivElement).querySelector(".room-diagnostics");
    expect(diagnostics).not.toBeNull();
    expect(diagnostics?.textContent).toMatch(/me/);
    expect(diagnostics?.textContent).toMatch(/roster 0/);
    expect((container as HTMLDivElement).querySelector(".roster-crown")).toBeNull();
  });

  it("inSala==false → rail not rendered", async () => {
    installInvoke([hostLive]);
    await renderApp();
    expect(rail()).toBeNull();
    expect((container as HTMLDivElement).querySelector(".room-live-rail")).toBeNull();
  });
});
