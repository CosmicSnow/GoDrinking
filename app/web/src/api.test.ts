// Intents chamam o comando exato de app/src/lib.rs (nome + args).
// Eventos escutam os nomes exatos de app/src/pump.rs.
import { beforeEach, describe, expect, it, vi } from "vitest";

const { mockInvoke, mockListen } = vi.hoisted(() => ({
  mockInvoke: vi.fn(),
  mockListen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mockListen }));

import {
  createRoom,
  getSnapshot,
  joinRoom,
  leaveRoom,
  listSources,
  onMediaEvent,
  onSignalEvent,
  setServer,
  sourceCapabilities,
  startShare,
  stopShare,
  unwatchMember,
  watchMember,
} from "./api";

beforeEach(() => {
  mockInvoke.mockReset().mockResolvedValue(null);
  mockListen.mockReset().mockResolvedValue(() => undefined);
});

describe("intents (comando certo, args certos)", () => {
  it("create_room leva nickname + password e devolve o código", async () => {
    mockInvoke.mockResolvedValueOnce("ABC123");
    await expect(createRoom("Ana", "segredo")).resolves.toBe("ABC123");
    expect(mockInvoke).toHaveBeenCalledWith("create_room", {
      nickname: "Ana",
      password: "segredo",
    });
  });

  it("join_room leva code + nickname + password e devolve o member id", async () => {
    mockInvoke.mockResolvedValueOnce("m-9");
    await expect(joinRoom("ABC123", "Bia", "segredo")).resolves.toBe("m-9");
    expect(mockInvoke).toHaveBeenCalledWith("join_room", {
      code: "ABC123",
      nickname: "Bia",
      password: "segredo",
    });
  });

  it("leave não leva args", async () => {
    await leaveRoom();
    expect(mockInvoke).toHaveBeenCalledWith("leave");
  });

  it("start_share leva a fonte opaca; stop_share não leva args", async () => {
    await startShare("synthetic");
    expect(mockInvoke).toHaveBeenCalledWith("start_share", { source: "synthetic" });
    await startShare("movie:/tmp/a.mp4");
    expect(mockInvoke).toHaveBeenCalledWith("start_share", { source: "movie:/tmp/a.mp4" });
    await stopShare();
    expect(mockInvoke).toHaveBeenCalledWith("stop_share");
  });

  it("watch/unwatch endereçam o membro", async () => {
    await watchMember("m-1");
    expect(mockInvoke).toHaveBeenCalledWith("watch", { member: "m-1" });
    await unwatchMember("m-1");
    expect(mockInvoke).toHaveBeenCalledWith("unwatch", { member: "m-1" });
  });

  it("get_snapshot é leitura sem args; set_server leva a base", async () => {    mockInvoke.mockResolvedValueOnce({ session: { id: null, state: "open" } });
    await getSnapshot();
    expect(mockInvoke).toHaveBeenCalledWith("get_snapshot");
    mockInvoke.mockResolvedValueOnce("http://127.0.0.1:18790");
    await expect(setServer("http://127.0.0.1:18790/")).resolves.toBe(
      "http://127.0.0.1:18790",
    );
    expect(mockInvoke).toHaveBeenCalledWith("set_server", {
      base: "http://127.0.0.1:18790/",
    });
  });
});

describe("eventos (nomes exatos do backend)", () => {
  it("escuta signal-event e entrega o payload", async () => {
    type Handler = (event: { payload: unknown }) => void;
    const handlers = new Map<string, Handler>();
    mockListen.mockImplementation((event: string, cb: Handler) => {
      handlers.set(event, cb);
      return Promise.resolve(() => undefined);
    });
    const seen: unknown[] = [];
    await onSignalEvent((payload) => seen.push(payload));
    expect(mockListen).toHaveBeenCalledWith("signal-event", expect.any(Function));
    handlers.get("signal-event")?.({ payload: { kind: "roster", entries: [], master: null } });
    expect(seen).toEqual([{ kind: "roster", entries: [], master: null }]);
  });

  it("escuta media-event e entrega o payload", async () => {
    type Handler = (event: { payload: unknown }) => void;
    const handlers = new Map<string, Handler>();
    mockListen.mockImplementation((event: string, cb: Handler) => {
      handlers.set(event, cb);
      return Promise.resolve(() => undefined);
    });
    const seen: unknown[] = [];
    await onMediaEvent((payload) => seen.push(payload));
    expect(mockListen).toHaveBeenCalledWith("media-event", expect.any(Function));
    handlers.get("media-event")?.({
      payload: { kind: "stats", frames: 30, keyframes: 2, ice: true, host: 1, srflx: 0 },
    });
    expect(seen).toEqual([
      { kind: "stats", frames: 30, keyframes: 2, ice: true, host: 1, srflx: 0 },
    ]);
  });
});

describe("fontes de captura (nomes exatos do backend)", () => {
  it("list_sources sem args devolve a lista opaca", async () => {
    const listed = [
      { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
    ];
    mockInvoke.mockResolvedValueOnce(listed);
    await expect(listSources()).resolves.toEqual(listed);
    expect(mockInvoke).toHaveBeenCalledWith("list_sources");
  });

  it("source_capabilities sem args devolve suporte com motivos", async () => {
    const caps = {
      display: { supported: true, reason: "ScreenCaptureKit" },
      window: { supported: true, reason: "ScreenCaptureKit" },
      app_audio: { supported: false, reason: "planejado (lane de áudio)" },
      exclusion: { supported: true, reason: "SCContentFilter" },
    };
    mockInvoke.mockResolvedValueOnce(caps);
    await expect(sourceCapabilities()).resolves.toEqual(caps);
    expect(mockInvoke).toHaveBeenCalledWith("source_capabilities");
  });
});
