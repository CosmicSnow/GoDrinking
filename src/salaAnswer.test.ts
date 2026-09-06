// @vitest-environment happy-dom
// Sala answer identity + Viewer robustness (App-level, mocked Tauri invoke
// and WebRTC): the answer envelope carries id=self + to=sharer + the opaque
// attempt echo; ICE failure surfaces status; an older redelivery never
// closes a newer PC; tile mount (re)binds the stored remotesRef stream.
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

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

const OFFER_SDP =
  "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000\r\n";
const ANSWER_SDP =
  "v=0\r\no=- 7 7 IN IP4 127.0.0.1\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000\r\n";

type FixturePerson = {
  id: string;
  nickname: string;
  state: string;
  master?: boolean;
  share?: boolean;
};

type SalaOffer = { from: string; sdp: string; offer_attempt: string };

// --- WebRTC fakes -----------------------------------------------------------

class FakeStream {
  tracks: Array<{ kind: string; readyState: string }> = [];
  addTrack(track: { kind: string; readyState: string }) {
    this.tracks.push(track);
  }
  getTracks() {
    return [...this.tracks];
  }
  getVideoTracks() {
    return this.tracks.filter((track) => track.kind === "video");
  }
  getAudioTracks() {
    return this.tracks.filter((track) => track.kind === "audio");
  }
}

// happy-dom type-checks HTMLMediaElement.srcObject against its own
// MediaStream: chain the fake below it so `video.srcObject = fake` passes
// the setter while the fake keeps its script-driven track lists.
Object.setPrototypeOf(
  FakeStream.prototype,
  ((globalThis as Record<string, unknown>).MediaStream as { prototype: object }).prototype,
);

class FakePC {  static instances: FakePC[] = [];
  ontrack: ((event: { streams: FakeStream[]; track: null }) => void) | null = null;
  oniceconnectionstatechange: (() => void) | null = null;
  iceConnectionState = "new";
  iceGatheringState = "complete";
  localDescription: { type: string; sdp: string } | null = null;
  closed = false;
  closeCount = 0;
  fromTag: string | null = null;
  constructor() {
    FakePC.instances.push(this);
  }
  async setRemoteDescription(_desc: unknown) {
    // accepted
  }
  async createAnswer() {
    return { type: "answer", sdp: ANSWER_SDP };
  }
  async setLocalDescription(desc: { sdp: string }) {
    this.localDescription = { type: "answer", sdp: desc.sdp };
  }
  close() {
    this.closed = true;
    this.closeCount += 1;
  }
  addEventListener() {}
  removeEventListener() {}
  async getStats() {
    return { forEach: (_cb: unknown) => {}, get: (_id: unknown) => undefined };
  }
  get connectionState() {
    return "new";
  }
  fireTrack(stream: FakeStream) {
    this.ontrack?.({ streams: [stream], track: null });
  }
  fireIce(state: string) {
    this.iceConnectionState = state;
    this.oniceconnectionstatechange?.();
  }
}

// --- Fixtures ---------------------------------------------------------------

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
  viewer_member_id: "me",
  session_mode: "room",
  join_mode: "stunar",
});

const sharer = (id: string, nickname: string): FixturePerson => ({
  id,
  nickname,
  state: "connected",
  master: false,
  share: true,
});

const offerQueue: SalaOffer[] = [];
const salaAnswers: Array<{ to: string; answer: Record<string, unknown> }> = [];
const broadcastAnswers: Array<{
  host: string;
  answer: Record<string, unknown>;
  join_mode: string;
}> = [];

type DiscoverValue = [string, Record<string, unknown>, string];

function installInvoke(opts: { roster: FixturePerson[]; discover: DiscoverValue }) {
  mockInvoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case "get_media_capabilities":
        return CAPS;
      case "discover_media_room":
        return opts.discover;
      case "get_media_session_state":
        return snapshotFor(opts.roster);
      case "get_media_preview":
        return null;
      case "poll_stunar_offers":
        return offerQueue.splice(0, offerQueue.length);
      case "stunar_watch":
        return null;
      case "requeue_stunar_offer":
        return null;
      case "send_stunar_room_answer": {
        const request = (args as { request: { to: string; answer: Record<string, unknown> } })
          .request;
        salaAnswers.push({ to: request.to, answer: request.answer });
        return null;
      }
      case "submit_media_room_answer": {
        const request = (
          args as {
            request: {
              host: string;
              answer: Record<string, unknown>;
              join_mode: string;
            };
          }
        ).request;
        broadcastAnswers.push({
          host: request.host,
          answer: request.answer,
          join_mode: request.join_mode,
        });
        return null;
      }
      default:
        return null;
    }
  });
}

// --- React harness ----------------------------------------------------------

let container: HTMLDivElement | null = null;
let root: Root | null = null;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

async function flush(rounds = 4) {
  for (let i = 0; i < rounds; i += 1) {
    await act(async () => {
      await sleep(20);
    });
  }
}

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

async function joinSalaAndWatch(id: string, nickname: string) {
  await renderApp();
  await joinAsWatcher();
  expect(rail()).not.toBeNull();
  // Session poll (500ms) publishes the roster + our member id first.
  await act(async () => {
    await sleep(650);
  });
  const button = watchButtons().find((item) => {
    const row = item.closest(".room-person");
    return row?.textContent?.includes(nickname);
  });
  expect(button, `Watch button for ${nickname}`).not.toBeUndefined();
  expect((button as HTMLButtonElement).disabled).toBe(false);
  click(button ?? null);
  await flush();
  expect(mockInvoke).toHaveBeenCalledWith("stunar_watch", {
    request: { to: id, start: true },
  });
}

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem("godrinking.locale", "en");
  localStorage.setItem("godrinking.nickname", "Joiner");
  localStorage.setItem("godrinking.rendezvous_url", "https://example.com");
  mockInvoke.mockReset();
  offerQueue.length = 0;
  salaAnswers.length = 0;
  broadcastAnswers.length = 0;
  FakePC.instances = [];
  (globalThis as Record<string, unknown>).RTCPeerConnection =
    FakePC as unknown as typeof RTCPeerConnection;
  (globalThis as Record<string, unknown>).MediaStream = FakeStream as unknown as typeof MediaStream;
  const proto = (globalThis as Record<string, unknown>).HTMLVideoElement as
    | { prototype: { play?: unknown } }
    | undefined;
  if (proto?.prototype) {
    proto.prototype.play = () => Promise.resolve();
  }
});

afterEach(async () => {
  await act(async () => {
    root?.unmount();
  });
  container?.remove();
  container = null;
  root = null;
});

describe("Sala answer identity (id=self, to=sharer, attempt echo)", () => {
  it(
    "answers with our member id, routed to the sharer, echoing the attempt",
    async () => {
      localStorage.setItem("godrinking.join_mode", "stunar");
      installInvoke({
        roster: [sharer("sharer-1", "Sharer")],
        discover: ["tok", { type: "offer", sdp: "", offer_attempt: "a0" }, "Host"],
      });
      await joinSalaAndWatch("sharer-1", "Sharer");
      offerQueue.push({ from: "sharer-1", sdp: OFFER_SDP, offer_attempt: "att-1" });
      await act(async () => {
        await sleep(1300);
      });
      expect(salaAnswers).toHaveLength(1);
      expect(salaAnswers[0].to).toBe("sharer-1");
      expect(salaAnswers[0].answer).toMatchObject({
        type: "answer",
        id: "me",
        offer_attempt: "att-1",
      });
      expect(salaAnswers[0].answer.sdp).toContain("m=video 9");
    },
    20000,
  );

  it(
    "an older redelivery never closes the newer PC (only att-2 is answered)",
    async () => {
      localStorage.setItem("godrinking.join_mode", "stunar");
      installInvoke({
        roster: [sharer("sharer-1", "Sharer")],
        discover: ["tok", { type: "offer", sdp: "", offer_attempt: "a0" }, "Host"],
      });
      await joinSalaAndWatch("sharer-1", "Sharer");
      const before = FakePC.instances.length;
      offerQueue.push(
        { from: "sharer-1", sdp: OFFER_SDP, offer_attempt: "att-1" },
        { from: "sharer-1", sdp: OFFER_SDP, offer_attempt: "att-2" },
      );
      await act(async () => {
        await sleep(1300);
      });
      const fresh = FakePC.instances.slice(before);
      expect(fresh.length).toBe(2);
      const [older, newer] = fresh;
      // The older attempt was superseded (its PC closed); the newer PC —
      // the live link — was never closed by the older invocation.
      expect(older.closed).toBe(true);
      expect(newer.closed).toBe(false);
      expect(newer.closeCount).toBe(0);
      expect(salaAnswers).toHaveLength(1);
      expect(salaAnswers[0].answer).toMatchObject({
        id: "me",
        offer_attempt: "att-2",
      });
    },
    20000,
  );

  it(
    "ICE failure on a Sala PC surfaces link status instead of silent black",
    async () => {
      localStorage.setItem("godrinking.join_mode", "stunar");
      installInvoke({
        roster: [sharer("sharer-1", "Sharer")],
        discover: ["tok", { type: "offer", sdp: "", offer_attempt: "a0" }, "Host"],
      });
      await joinSalaAndWatch("sharer-1", "Sharer");
      offerQueue.push({ from: "sharer-1", sdp: OFFER_SDP, offer_attempt: "att-1" });
      await act(async () => {
        await sleep(1300);
      });
      expect(salaAnswers).toHaveLength(1);
      const pc = FakePC.instances[FakePC.instances.length - 1];
      // Every Sala PC carries an ICE-state handler (Broadcast parity).
      expect(pc.oniceconnectionstatechange).not.toBeNull();
      await act(async () => {
        pc.fireIce("failed");
      });
      // The notice is the status line: open the room desk to read it.
      const settings = (container as HTMLDivElement).querySelector(
        ".room-live-bar .room-bar-btn",
      );
      click(settings);
      await flush();
      const notice = (container as HTMLDivElement).querySelector(".start-hint");
      expect(notice?.textContent).toContain("sharer-1");
      expect(notice?.textContent).toMatch(/failed/i);
    },
    20000,
  );

  it(
    "tile mount (re)binds the stored remotesRef stream",
    async () => {
      localStorage.setItem("godrinking.join_mode", "stunar");
      installInvoke({
        roster: [sharer("sharer-1", "Sharer")],
        discover: ["tok", { type: "offer", sdp: "", offer_attempt: "a0" }, "Host"],
      });
      await joinSalaAndWatch("sharer-1", "Sharer");
      offerQueue.push({ from: "sharer-1", sdp: OFFER_SDP, offer_attempt: "att-1" });
      await act(async () => {
        await sleep(1300);
      });
      expect(salaAnswers).toHaveLength(1);
      // Late media: ontrack fires after the answer round-trip.
      const pc = FakePC.instances[FakePC.instances.length - 1];
      const live = new FakeStream();
      live.addTrack({ kind: "video", readyState: "live" });
      await act(async () => {
        pc.fireTrack(live);
      });
      await flush();
      const video = (container as HTMLDivElement).querySelector(
        'video[data-slot="sharer-1"]',
      ) as HTMLVideoElement | null;
      expect(video).not.toBeNull();
      const bound = (video as unknown as { srcObject: unknown }).srcObject;
      expect(bound).not.toBeNull();
      // Simulate a remount that lost the binding (ontrack will not refire):
      // the mount effect must restore the stored stream when tiles change.
      (video as unknown as { srcObject: unknown }).srcObject = null;
      const pin = [...(container as HTMLDivElement).querySelectorAll(".room-tile-actions button")].find(
        (item) => item.textContent === "Pin",
      );
      click(pin ?? null);
      await flush();
      const rebound = (container as HTMLDivElement).querySelector(
        'video[data-slot="sharer-1"]',
      ) as HTMLVideoElement | null;
      expect(rebound).not.toBeNull();
      expect((rebound as unknown as { srcObject: unknown }).srcObject).toBe(bound);
    },
    20000,
  );
});

describe("Broadcast answer identity", () => {
  it(
    "LAN/Direct echoes the host-minted viewer id (offer.id names us)",
    async () => {
      localStorage.setItem("godrinking.join_mode", "lan");
      installInvoke({
        roster: [],
        discover: [
          "192.168.1.2:41234",
          { type: "offer", sdp: OFFER_SDP, id: "viewer-9", offer_attempt: "b1" },
          "Hosty",
        ],
      });
      await renderApp();
      const navItems = [...(container as HTMLDivElement).querySelectorAll(".nav-item")];
      const watchNav = navItems.find((item) => item.textContent?.includes("Watch"));
      click(watchNav ?? null);
      await flush();
      setInput("join-code", "ABC123");
      setInput("join-nickname", "Joiner");
      await flush();
      click((container as HTMLDivElement).querySelector(".controls-panel .primary-cta"));
      await flush(6);
      expect(broadcastAnswers).toHaveLength(1);
      expect(broadcastAnswers[0].host).toBe("192.168.1.2:41234");
      expect(broadcastAnswers[0].answer).toMatchObject({
        type: "answer",
        id: "viewer-9",
        offer_attempt: "b1",
      });
    },
    20000,
  );

  it(
    "Stunar Broadcast answers with our member id (offer.id names the sharer)",
    async () => {
      localStorage.setItem("godrinking.join_mode", "stunar");
      installInvoke({
        roster: [],
        discover: ["viewer-token", { type: "offer", sdp: OFFER_SDP, id: "host", offer_attempt: "c1" }, ""],
      });
      await renderApp();
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
      expect(broadcastAnswers).toHaveLength(1);
      expect(broadcastAnswers[0].answer).toMatchObject({
        type: "answer",
        id: "me",
        offer_attempt: "c1",
      });
    },
    20000,
  );
});
