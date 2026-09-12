import { describe, it, expect } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import { StreamPlayer, parsePlayerFrame, clampZoom, applyWheelZoom } from "./StreamPlayer";

describe("stream presentation", () => {
  it("reads binary sequence, dynamic dimensions and exact RGBA bytes", () => {
    const buffer = new ArrayBuffer(20);
    const header = new DataView(buffer);
    header.setUint32(0, 17, true); header.setUint32(4, 2, true); header.setUint32(8, 1, true);
    new Uint8Array(buffer, 12).set([255, 0, 0, 255, 0, 255, 0, 255]);
    const frame = parsePlayerFrame(buffer);
    expect([frame.seq, frame.width, frame.height]).toEqual([17, 2, 1]);
    expect([...frame.pixels]).toEqual([255, 0, 0, 255, 0, 255, 0, 255]);
    expect(() => parsePlayerFrame(buffer.slice(0, 19))).toThrow();
    expect(() => parsePlayerFrame(new ArrayBuffer(8))).toThrow();
  });
  it("scroll down zooms out to 1× without leaving leftover pan", () => {
    expect(clampZoom(-2)).toBe(1); expect(clampZoom(30)).toBe(4);
    const size = { width: 400, height: 300 };
    const cursor = { x: 300, y: 200 };
    const inZoom = applyWheelZoom({ scale: 1, x: 0, y: 0 }, -80, cursor, size);
    expect(inZoom.scale).toBeCloseTo(1.2);
    const cx = cursor.x - size.width / 2;
    const cy = cursor.y - size.height / 2;
    expect((cx - inZoom.x) / inZoom.scale).toBeCloseTo(cx);
    expect((cy - inZoom.y) / inZoom.scale).toBeCloseTo(cy);
    expect(applyWheelZoom({ scale: 1.2, x: 40, y: -10 }, 80, cursor, size)).toEqual({ scale: 1, x: 0, y: 0 });
  });

  it("bounds zoom and offers the same volume/fullscreen/return controls in a popup", () => {
    const html = renderToStaticMarkup(createElement(StreamPlayer, { member: "a", nickname: "Ana", popupWindow: true }));
    for (const label of ["Voltar à sala", "Volume de Ana", "Ampliar", "100%", "<canvas"]) expect(html).toContain(label);
    expect(html).not.toContain("Em pop-up");
  });

  it("in-room tile keeps pin, pop-up, zoom and per-stream volume", () => {
    const html = renderToStaticMarkup(createElement(StreamPlayer, {
      member: "a", nickname: "Ana", onPin: () => undefined, onStop: () => undefined,
    }));
    for (const label of ["Fixar", "Pop-up", "Ampliar", "100%", "Volume de Ana", "Parar de ver", "Aguardando vídeo"]) {
      expect(html).toContain(label);
    }
    expect(html).not.toContain("Em pop-up");
  });
});
