import { describe, expect, it } from "vitest";
import { frameToRgba, parsePlayerFrame, type PlayerFrame } from "./playerRenderer";

function wire(seq: number, width: number, height: number, format: number, payload: number[]): ArrayBuffer {
  const out = new ArrayBuffer(20 + payload.length);
  const bytes = new Uint8Array(out);
  bytes.set([0x47, 0x4c, 0x50, 0x32]);
  const view = new DataView(out);
  view.setUint32(4, seq, true); view.setUint32(8, width, true); view.setUint32(12, height, true); view.setUint32(16, format, true);
  bytes.set(payload, 20);
  return out;
}

describe("GLP2 player frames", () => {
  it("parses RGBA packed frames", () => {
    const frame = parsePlayerFrame(wire(7, 1, 1, 0, [1, 2, 3, 255]));
    expect(frame).toEqual({ seq: 7, width: 1, height: 1, format: 0, pixels: expect.any(Uint8Array) });
    expect([...frame.pixels]).toEqual([1, 2, 3, 255]);
  });

  it("parses tight I420 and NV12 without expanding pixels", () => {
    for (const format of [1, 2]) {
      const frame = parsePlayerFrame(wire(1, 2, 2, format, new Array(6).fill(128)));
      expect(frame.pixels.length).toBe(6);
      expect(frame.format).toBe(format);
      expect(() => parsePlayerFrame(wire(1, 2, 2, format, new Array(8).fill(128)))).toThrow();
      expect(() => parsePlayerFrame(wire(1, 3, 3, format, new Array(17).fill(128)))).toThrow();
    }
  });

  it("rejects bad magic, formats, dimensions, and exact-length violations", () => {
    const cases = [
      wire(0, 1, 1, 0, [0, 0, 0]),
      wire(0, 1, 1, 3, [0, 0, 0, 0]),
      wire(0, 1, 1, 2, [0, 0, 0, 0]),
      wire(0, 3, 2, 2, new Array(12).fill(0)),
      wire(0, 0, 1, 0, []),
    ];
    for (const value of cases) expect(() => parsePlayerFrame(value)).toThrow();
    const magic = wire(0, 1, 1, 0, [0, 0, 0, 0]); new Uint8Array(magic)[0] = 0;
    expect(() => parsePlayerFrame(magic)).toThrow();
    expect(() => parsePlayerFrame(new ArrayBuffer(19))).toThrow();
  });
});

describe("player frame CPU conversion", () => {
  it("converts limited-range black and white I420 correctly", () => {
    const black: PlayerFrame = { seq: 0, width: 2, height: 2, format: 1, pixels: new Uint8Array([16, 16, 16, 16, 128, 128]) };
    const white: PlayerFrame = { seq: 0, width: 2, height: 2, format: 1, pixels: new Uint8Array([235, 235, 235, 235, 128, 128]) };
    expect([...frameToRgba(black)]).toEqual([0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
    expect([...frameToRgba(white)]).toEqual([255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]);
  });

  it("converts NV12 and preserves RGBA without a blank fallback", () => {
    const nv12: PlayerFrame = { seq: 0, width: 2, height: 2, format: 2, pixels: new Uint8Array([81, 81, 81, 81, 90, 240]) };
    const red = frameToRgba(nv12);
    expect(red[0]).toBeGreaterThan(240); expect(red[1]).toBeLessThan(40); expect(red[2]).toBeLessThan(40);
    const rgba: PlayerFrame = { seq: 1, width: 1, height: 1, format: 0, pixels: new Uint8Array([9, 8, 7, 6]) };
    expect([...frameToRgba(rgba)]).toEqual([9, 8, 7, 6]);
  });
});
