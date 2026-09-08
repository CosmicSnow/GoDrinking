// Driver e2e test-only: helpers puros + fluxos com Tauri mockado.
// Segue as convenções de api.test.ts (vi.mock de @tauri-apps/api).
import { beforeEach, describe, expect, it, vi } from "vitest";

const { mockInvoke, mockListen } = vi.hoisted(() => ({
  mockInvoke: vi.fn(),
  mockListen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mockListen }));

import { getE2ePlan, type E2ePlan } from "./api";
import { linkConnected, pickSharer, runE2ePlan, type E2eReport } from "./e2e";

const PLAN: E2ePlan = {
  role: "host",
  server: "http://127.0.0.1:9",
  password: "pw",
  nickname: "host-e2e",
  code_file: "/tmp/code",
  status_file: "/tmp/host.json",
};

const SNAPSHOT = (states: string[]) => ({
  session: { id: "1", state: "open" },
  share: { id: "2", state: "live" },
  links: states.map((state, i) => ({ id: `${i}`, watcher: "w", state })),
  watchers: [],
  roster: [],
});

beforeEach(() => {
  mockInvoke.mockReset();
  mockListen.mockReset().mockResolvedValue(() => undefined);
});

describe("helpers puros", () => {
  it("pickSharer acha quem compartilha e ignora a si mesmo", () => {
    const entries = [
      { id: "me", nickname: "eu", master: false, share: false },
      { id: "host", nickname: "h", master: true, share: true },
    ];
    expect(pickSharer(entries, "me")).toBe("host");
    expect(pickSharer(entries, "host")).toBe(null);
    expect(pickSharer([], null)).toBe(null);
  });

  it("linkConnected exige link connected", () => {
    expect(linkConnected(null)).toBe(false);
    expect(linkConnected(SNAPSHOT(["negotiating"]) as never)).toBe(false);
    expect(linkConnected(SNAPSHOT(["negotiating", "connected"]) as never)).toBe(true);
  });

  it("getE2ePlan chama o comando sem args e aceita null", async () => {
    mockInvoke.mockResolvedValueOnce(null);
    await expect(getE2ePlan()).resolves.toBe(null);
    expect(mockInvoke).toHaveBeenCalledWith("get_e2e_plan");
    mockInvoke.mockResolvedValueOnce(PLAN);
    await expect(getE2ePlan()).resolves.toEqual(PLAN);
  });
});

describe("runE2ePlan host", () => {
  it("create → publica code → share → connected+keyframe", async () => {
    const hooks: { media: ((e: never) => void) | null } = { media: null };
    mockListen.mockImplementation((event: string, cb: (e: never) => void) => {
      if (event === "media-event") hooks.media = cb;
      return Promise.resolve(() => undefined);
    });
    mockInvoke.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "set_server":
          return Promise.resolve("http://127.0.0.1:9");
        case "create_room":
          return Promise.resolve("ABC123");
        case "start_share":
          return Promise.resolve(undefined);
        case "set_quality":
          return Promise.resolve({
            profile: { w: 640, h: 360, bitrate_kbps: 1000, fps: 15 },
            generation: 0,
          });
        case "get_media_counters":
          return Promise.resolve({
            connected: true,
            frames: 0,
            keyframes: 1,
            keyframes_seen: true,
            presented: 0,
            effective: {
              profile: { w: 640, h: 360, bitrate_kbps: 1000, fps: 15 },
              generation: 1,
            },
          });
        case "e2e_status":
          return Promise.resolve(undefined);
        case "get_snapshot":
          return Promise.resolve(SNAPSHOT(["connected"]));
        default:
          return Promise.reject(new Error(`unexpected ${cmd}`));
      }
    });
    const reports: E2eReport[] = [];
    const done = runE2ePlan(PLAN, (r) => {
      reports.push(r);
    });
    // Eventos dirigem até connected.
    await new Promise((r) => setTimeout(r, 20));
    hooks.media?.({ payload: { kind: "ice-connected" } } as never);
    await Promise.resolve();
    hooks.media?.({ payload: { kind: "keyframe" } } as never);
    await done;
    expect(mockInvoke).toHaveBeenCalledWith("create_room", {
      nickname: "host-e2e",
      password: "pw",
    });
    expect(mockInvoke).toHaveBeenCalledWith("start_share", { source: "synthetic" });
    // Caminho real do aplicar-qualidade: intent exato (Tauri converte o
    // snake_case Rust para camelCase no IPC — ver api.setQuality), geração bumpada.
    expect(mockInvoke).toHaveBeenCalledWith("set_quality", {
      w: 640,
      h: 360,
      bitrateKbps: 1000,
      fps: 15,
    });
    // Code publicado no status (sem segredos além do code, que é local).
    const room = mockInvoke.mock.calls.find((c) => c[0] === "e2e_status");
    expect(JSON.parse(room?.[1].payload as string).code).toBe("ABC123");
    const last = reports[reports.length - 1];
    expect(last.phase).toBe("quality-applied");
    expect(last.connected).toBe(true);
    expect(last.keyframesSeen).toBe(true);
    expect(last.qualityApplied).toBe(true);
  });
});

describe("runE2ePlan viewer", () => {
  const VPLAN: E2ePlan = { ...PLAN, role: "viewer" };

  it("lê code → join → watch no sharer → connected com frames", async () => {
    const hooks: { signal: ((e: never) => void) | null; media: ((e: never) => void) | null } = {
      signal: null,
      media: null,
    };
    mockListen.mockImplementation((event: string, cb: (e: never) => void) => {
      if (event === "signal-event") hooks.signal = cb;
      if (event === "media-event") hooks.media = cb;
      return Promise.resolve(() => undefined);
    });
    mockInvoke.mockImplementation((cmd: string, args?: unknown) => {
      switch (cmd) {
        case "e2e_read_code":
          return Promise.resolve("ABC123");
        case "get_media_counters":
          return Promise.resolve({ connected: true, frames: 12, keyframes: 1, keyframes_seen: true, presented: 9 });
        case "set_server":
          return Promise.resolve("http://127.0.0.1:9");
        case "join_room":
          return Promise.resolve("viewer-1");
        case "watch":
          expect((args as { member: string }).member).toBe("host-9");
          return Promise.resolve(undefined);
        case "e2e_status":
          return Promise.resolve(undefined);
        case "get_snapshot":
          return Promise.resolve(SNAPSHOT(["connected"]));
        default:
          return Promise.reject(new Error(`unexpected ${cmd}`));
      }
    });
    const reports: E2eReport[] = [];
    const done = runE2ePlan(VPLAN, (r) => {
      reports.push(r);
    });
    // Roster chega: driver assiste quem compartilha.
    await new Promise((r) => setTimeout(r, 20));
    hooks.signal?.({
      payload: {
      kind: "roster",
      entries: [
        { id: "viewer-1", nickname: "v", master: false, share: false },
        { id: "host-9", nickname: "h", master: true, share: true },
      ],
      master: "host-9",
    } } as never);
    await new Promise((r) => setTimeout(r, 20));
    hooks.media?.({ payload: { kind: "ice-connected" } } as never);
    hooks.media?.({ payload: { kind: "stats", frames: 12, keyframes: 1, ice: true, host: 2, srflx: 0 } } as never);
    await done;
    const last = reports[reports.length - 1];
    expect(last.phase).toBe("connected");
    expect(last.frames).toBe(12);
    // O poll de contadores roda em paralelo aos eventos.
    expect(mockInvoke.mock.calls.some((c) => c[0] === "get_media_counters")).toBe(true);
  });
});
