// Verificador de atualização: semver simples, escolha de asset por
// plataforma (espelha site/app/app.vue) e silêncio em falha.
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  UpdateModal,
} from "./views";
import {
  checkForUpdate,
  detectPlatform,
  isNewer,
  normalizeTag,
  parseVersion,
  pickAsset,
  type ReleaseAsset,
} from "./update";

const assets: ReleaseAsset[] = [
  { id: 1, name: "goDrinking-0.7.7.dmg", browser_download_url: "https://dl/mac.dmg" },
  { id: 2, name: "goDrinking.exe", browser_download_url: "https://dl/win.exe" },
  { id: 3, name: "checksums.txt", browser_download_url: "https://dl/sums.txt" },
];

const release = (overrides: Record<string, unknown> = {}) => ({
  tag_name: "v0.7.7",
  html_url: "https://github.com/CosmicSnow/GoDrinking/releases/tag/v0.7.7",
  assets,
  ...overrides,
});

const okFetch = (body: unknown) =>
  ((() => Promise.resolve({ ok: true, json: () => Promise.resolve(body) })) as unknown as typeof fetch);

describe("normalizeTag/parseVersion/isNewer", () => {
  it("aceita v0.7.7 e 0.7.7", () => {
    expect(normalizeTag("v0.7.7")).toBe("0.7.7");
    expect(normalizeTag("0.7.7")).toBe("0.7.7");
    expect(normalizeTag("V0.7.7")).toBe("0.7.7");
  });

  it("rejeita tag que não é versão", () => {
    expect(normalizeTag("nightly")).toBeNull();
    expect(normalizeTag("")).toBeNull();
  });

  it("parseVersion extrai o trio", () => {
    expect(parseVersion("0.7.7")).toEqual([0, 7, 7]);
    expect(parseVersion("10.2.3-beta")).toEqual([10, 2, 3]);
    expect(parseVersion("abc")).toBeNull();
  });

  it("só é nova quando estritamente maior", () => {
    expect(isNewer("0.7.7", "0.7.8")).toBe(true);
    expect(isNewer("0.7.7", "0.8.0")).toBe(true);
    expect(isNewer("0.7.7", "1.0.0")).toBe(true);
    expect(isNewer("0.7.7", "0.7.7")).toBe(false);
    expect(isNewer("0.7.8", "0.7.7")).toBe(false);
    expect(isNewer("0.7.7", "lixo")).toBe(false);
  });
});

describe("detectPlatform/pickAsset", () => {
  it("Windows -> win, macOS -> mac", () => {
    expect(detectPlatform("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe("win");
    expect(detectPlatform("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)")).toBe("mac");
    expect(detectPlatform("Mozilla/5.0 (X11; Linux x86_64)")).toBe("other");
  });

  it("mac pega o .dmg, win pega o goDrinking.exe", () => {
    expect(pickAsset(assets, "mac")?.browser_download_url).toBe("https://dl/mac.dmg");
    expect(pickAsset(assets, "win")?.browser_download_url).toBe("https://dl/win.exe");
  });

  it("sem asset da plataforma devolve null", () => {
    const onlyDmg = [assets[0]];
    expect(pickAsset(onlyDmg, "win")).toBeNull();
    expect(pickAsset([], "mac")).toBeNull();
  });

  it("preferências do win seguem o site (setup > msi > portable > exe)", () => {
    const list: ReleaseAsset[] = [
      { name: "app.exe", browser_download_url: "u-generic" },
      { name: "app-portable.exe", browser_download_url: "u-portable" },
      { name: "app.msi", browser_download_url: "u-msi" },
      { name: "app-setup.exe", browser_download_url: "u-setup" },
    ];
    expect(pickAsset(list, "win")?.browser_download_url).toBe("u-setup");
    const noSetup = list.slice(0, 3);
    expect(pickAsset(noSetup, "win")?.browser_download_url).toBe("u-msi");
  });
});

describe("checkForUpdate", () => {
  it("devolve info quando há versão nova", async () => {
    const info = await checkForUpdate("0.7.6", {
      fetchFn: okFetch(release()),
      userAgent: "Macintosh",
    });
    expect(info?.latest).toBe("0.7.7");
    expect(info?.tag).toBe("v0.7.7");
    expect(info?.assetUrl).toBe("https://dl/mac.dmg");
    expect(info?.releasesUrl).toContain("github.com/CosmicSnow/GoDrinking");
  });

  it("null quando já está na última", async () => {
    await expect(
      checkForUpdate("0.7.7", { fetchFn: okFetch(release()), userAgent: "Macintosh" }),
    ).resolves.toBeNull();
  });

  it("silencia 404 (repo privado), rede e tag inválida", async () => {
    const notFound = (() => Promise.resolve({ ok: false })) as unknown as typeof fetch;
    await expect(checkForUpdate("0.7.6", { fetchFn: notFound })).resolves.toBeNull();
    const failing = (() => Promise.reject(new Error("offline"))) as unknown as typeof fetch;
    await expect(checkForUpdate("0.7.6", { fetchFn: failing })).resolves.toBeNull();
    await expect(
      checkForUpdate("0.7.6", { fetchFn: okFetch(release({ tag_name: "nightly" })) }),
    ).resolves.toBeNull();
  });

  it("sem fetch disponível devolve null", async () => {
    const realFetch = globalThis.fetch;
    // @ts-expect-error - simula ambiente sem fetch
    globalThis.fetch = undefined;
    try {
      await expect(checkForUpdate("0.7.6", {})).resolves.toBeNull();
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  it("sem asset direto, assetUrl é null mas ainda avisa", async () => {
    const info = await checkForUpdate("0.7.6", {
      fetchFn: okFetch(release({ assets: [] })),
      userAgent: "Macintosh",
    });
    expect(info?.latest).toBe("0.7.7");
    expect(info?.assetUrl).toBeNull();
  });
});

describe("UpdateModal", () => {
  const info = {
    latest: "0.7.8",
    tag: "v0.7.8",
    assetUrl: "https://dl/mac.dmg",
    releasesUrl: "https://github.com/CosmicSnow/GoDrinking/releases/latest",
  };

  it("mostra versão atual -> nova com os 3 botões", () => {
    const html = renderToStaticMarkup(
      createElement(UpdateModal, {
        current: "0.7.7",
        info,
        open: true,
        onClose: () => undefined,
        onOpenUrl: () => undefined,
      }),
    );
    expect(html).toContain("Atualização disponível");
    expect(html).toContain("v0.7.8");
    expect(html).toContain("v0.7.7");
    expect(html).toContain("Cancelar");
    expect(html).toContain("Baixar do site");
    expect(html).toContain("Baixar direto");
    expect(html).toContain('role="dialog"');
  });

  it("fechado esconde via hidden; sem asset direto esconde o 3º botão", () => {
    const closed = renderToStaticMarkup(
      createElement(UpdateModal, {
        current: "0.7.7",
        info,
        open: false,
        onClose: () => undefined,
        onOpenUrl: () => undefined,
      }),
    );
    expect(closed).toContain("hidden");

    const noAsset = renderToStaticMarkup(
      createElement(UpdateModal, {
        current: "0.7.7",
        info: { ...info, assetUrl: null },
        open: true,
        onClose: () => undefined,
        onOpenUrl: () => undefined,
      }),
    );
    expect(noAsset).toContain("Baixar do site");
    expect(noAsset).not.toContain("Baixar direto");
  });

  it("botões chamam onOpenUrl com a URL certa (fio direto no markup)", () => {
    // O clique é fio direto (onClick={() => onOpenUrl(url)}); aqui o
    // markup estático garante que os dois botões de download existem.
    const html = renderToStaticMarkup(
      createElement(UpdateModal, {
        current: "0.7.7",
        info,
        open: true,
        onClose: () => undefined,
        onOpenUrl: () => undefined,
      }),
    );
    expect(html).toContain("Baixar do site");
    expect(html).toContain("Baixar direto");
  });
});
