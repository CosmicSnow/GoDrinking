// Preview ao vivo do modal: foco decide, mock nunca invoca, fio é GLP2.
import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import { PreviewPlayer, SelfViewPlayer, appFocusNow } from "./PreviewPlayer";

describe("appFocusNow (puro: janela E documento)", () => {
  it("só focado + visível roda o preview", () => {
    expect(appFocusNow(true, false)).toBe(true);
    expect(appFocusNow(false, false)).toBe(false);
    expect(appFocusNow(true, true)).toBe(false);
    expect(appFocusNow(false, true)).toBe(false);
  });
});

describe("PreviewPlayer (mock: sem Tauri, sem invoke)", () => {
  it("sem Tauri mostra o placeholder honesto, nunca o canvas", () => {
    expect(typeof window).toBe("undefined");
    const html = renderToStaticMarkup(
      createElement(PreviewPlayer, { kind: "camera", id: "0", active: true }),
    );
    expect(html).toContain("preview-mock");
    expect(html).toContain("só no app");
    expect(html).not.toContain("<canvas");
  });

  it("inativo também não monta canvas no mock", () => {
    const html = renderToStaticMarkup(
      createElement(PreviewPlayer, { kind: "display", id: "1", active: false }),
    );
    expect(html).toContain("preview-mock");
  });
});

describe("SelfViewPlayer (mock: sem Tauri, sem invoke)", () => {
  it("sem Tauri mostra o placeholder honesto, nunca o canvas", () => {
    const html = renderToStaticMarkup(
      createElement(SelfViewPlayer, { active: true, nickname: "Ana" }),
    );
    expect(html).toContain("selfview-mock");
    expect(html).toContain("só no app");
    expect(html).not.toContain("<canvas");
  });
});
