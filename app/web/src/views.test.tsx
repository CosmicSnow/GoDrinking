// Render de snapshot sem timers nem DOM: props entram, markup estático sai.
// Estados vazios sempre trazem diagnóstico — nunca área muda silenciosa.
import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import {
  HomeScreen,
  RoomScreen,
  linkLabel,
  salaLabel,
  shareLabel,
  sourceKindOf,
  validateCode,
  validateNickname,
  validatePassword,
  validateSource,
} from "./views";
import type { OwnerSnapshot, RoomMember } from "./api";

const noop = (..._args: unknown[]): void => undefined;

const snapshotFixture = (overrides: Partial<OwnerSnapshot> = {}): OwnerSnapshot => ({
  session: { id: "sess:1", state: "open" },
  share: { id: null, state: "stopped" },
  links: [],
  watchers: [],
  roster: [],
  ...overrides,
});

const roomProps = (overrides: Partial<Parameters<typeof RoomScreen>[0]> = {}) => ({
  roomCode: "ABC123",
  nickname: "Ana",
  snapshot: snapshotFixture(),
  roster: [] as RoomMember[],
  selfId: "m-1",
  selfNickname: "Ana",
  watching: [] as string[],
  source: "synthetic",
  onSource: noop,
  sources: [],
  sourcesError: null as string | null,
  caps: null,
  busy: false,
  error: null as string | null,
  lastSignal: null as string | null,
  lastMedia: null as string | null,
  stats: null,
  onListSources: noop,
  onRefresh: noop,
  onLeave: noop,
  onShare: noop,
  onStopShare: noop,
  onWatch: noop,
  onUnwatch: noop,
  ...overrides,
});

describe("validação (regras do backend, sem segredos nas mensagens)", () => {
  it("apelido 2–24, charset restrito", () => {
    expect(validateNickname("A")).not.toBeNull();
    expect(validateNickname("Ana")).toBeNull();
    expect(validateNickname("a".repeat(25))).not.toBeNull();
    expect(validateNickname("a<b")).not.toBeNull();
  });

  it("senha 4–64 obrigatória", () => {
    expect(validatePassword("123")).not.toBeNull();
    expect(validatePassword("segredo")).toBeNull();
  });

  it("código e fonte", () => {
    expect(validateCode("AB")).not.toBeNull();
    expect(validateCode("ABC123")).toBeNull();
    expect(validateSource("synthetic")).toBeNull();
    expect(validateSource("movie:/tmp/a.mp4")).toBeNull();
    expect(validateSource("display:1")).toBeNull();
    expect(validateSource("window:42")).toBeNull();
    expect(validateSource("screen")).not.toBeNull();
    expect(validateSource("movie:")).not.toBeNull();
    expect(validateSource("display:")).not.toBeNull();
    expect(validateSource("window:")).not.toBeNull();
  });
});

describe("rótulos de estado (espelham core/src/state.rs)", () => {
  it("traduz os estados sem inventar", () => {
    expect(salaLabel("open")).toBe("Aberta");
    expect(shareLabel("live")).toBe("No ar");
    expect(linkLabel("negotiating")).toBe("Negociando");
    expect(linkLabel("connected")).toBe("Conectado");
  });
});

describe("HomeScreen", () => {
  it("renderiza criar/entrar sem vazar a senha de volta", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "http://127.0.0.1:18790",
        onServer: noop,
        tab: "join",
        onTab: noop,
        nickname: "Ana",
        onNickname: noop,
        password: "segredo-super",
        onPassword: noop,
        code: "ABC123",
        onCode: noop,
        busy: false,
        error: null,
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Criar sala");
    expect(html).toContain("Entrar");
    expect(html).toContain('type="password"');
    // A senha digitada jamais volta como texto visível: só dentro do
    // próprio input mascarado (o navegador nunca a exibe).
    const visibleText = html.replace(/<input[^>]*>/g, "");
    expect(visibleText).not.toContain("segredo-super");
  });

  it("mostra o erro sem área muda", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "",
        onServer: noop,
        tab: "create",
        onTab: noop,
        nickname: "",
        onNickname: noop,
        password: "",
        onPassword: noop,
        code: "",
        onCode: noop,
        busy: false,
        error: "Senha: 4 a 64 caracteres (obrigatória).",
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Senha: 4 a 64 caracteres");
  });
});

describe("RoomScreen (snapshot → markup, sem inferência)", () => {
  const members: RoomMember[] = [
    { id: "m-1", nickname: "Ana", master: true, share: false },
    { id: "m-2", nickname: "Bia", master: false, share: true },
  ];

  it("roster com master/share, sem botão de watch para si", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ roster: members })));
    expect(html).toContain("ABC123");
    expect(html).toContain("♛");
    expect(html).toContain("compartilhando");
    expect(html).toContain("(você)");
    expect(html).toContain("Assistir");
    // Um botão Assistir (só Bia compartilha e não é você).
    expect(html.match(/Assistir/g)?.length).toBe(1);
  });

  it("membro assistido mostra Parar de ver", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ roster: members, watching: ["m-2"] })),
    );
    expect(html).toContain("Parar de ver");
    expect(html).toContain("pedido de watch");
  });

  it("roster vazio traz diagnóstico, nunca lista muda", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(html).toContain("Nenhum membro visível ainda");
    expect(html).toContain("Atualizar");
  });

  it("links do snapshot com estado; sem links, diagnóstico", () => {
    const withLinks = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({
            share: { id: "share:1", state: "live" },
            links: [{ id: "link:1", watcher: "Bia", state: "negotiating" }],
            watchers: ["Bia"],
          }),
        }),
      ),
    );
    expect(withLinks).toContain("Bia");
    expect(withLinks).toContain("Negociando");
    expect(withLinks).toContain("No ar");

    const empty = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(empty).toContain("Nenhum link ativo no snapshot");
  });

  it("estado de vídeo real: sem link conectado vs janela nativa + apresentados", () => {
    const withoutLink = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(withoutLink).toContain("Sem vídeo: nenhum link conectado");
    expect(withoutLink).toContain("ainda sem amostra do evento de mídia");

    const connected = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({
            links: [{ id: "lnk:1", watcher: "m-2", state: "connected" }],
          }),
          stats: { frames: 120, keyframes: 4, ice: true, presented: 118 },
        }),
      ),
    );
    expect(connected).toContain("Vídeo na janela nativa");
    expect(connected).toContain("Frames recebidos: 120");
    expect(connected).toContain("apresentados: 118");
  });

  it("sem snapshot, diagnóstico pede Atualizar (nunca estado inventado)", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ snapshot: null })));
    expect(html).toContain("sem snapshot — toque Atualizar");
    expect(html).not.toContain("Aberta");
  });

  it("diagnóstico mostra último erro e últimos eventos", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ error: "sala cheia", lastSignal: "roster (2 membro(s))", lastMedia: "stats (30 frames)" }),
      ),
    );
    expect(html).toContain("sala cheia");
    expect(html).toContain("roster (2 membro(s))");
    expect(html).toContain("stats (30 frames)");
  });
});

describe("fontes de captura (select + capacidades)", () => {
  it("sourceKindOf deriva o tipo do seletor opaco", () => {
    expect(sourceKindOf("synthetic")).toBe("synthetic");
    expect(sourceKindOf("movie:/a.mp4")).toBe("movie");
    expect(sourceKindOf("display:1")).toBe("display");
    expect(sourceKindOf("window:42")).toBe("window");
    expect(sourceKindOf("lixo")).toBe("synthetic");
  });

  it("display sem lista mostra o botão Listar + dica de permissão", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ source: "display:" })),
    );
    expect(html).toContain("Listar telas");
    expect(html).toContain("perm");
  });

  it("capacidade negada desabilita a opção com o motivo", () => {
    const no = { supported: false, reason: "planejado (WGC/DXGI)" };
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "synthetic",
          caps: {
            display: no,
            window: no,
            app_audio: no,
            exclusion: no,
          },
        }),
      ),
    );
    expect(html).toContain("planejado (WGC/DXGI)");
  });

  it("lista populada oferece as fontes pelo nome", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:",
          sources: [
            { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
          ],
        }),
      ),
    );
    expect(html).toContain("Display 1 · 2560x1440");
  });
});
