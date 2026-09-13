import { afterEach, describe, expect, it, vi } from "vitest";
import { deliverPlayerFrame } from "./playerFrameDelivery";

afterEach(() => vi.unstubAllGlobals());

describe("video delivery independent of animation callback scheduling", () => {
  it("draws and releases the single flight even if the WebView delays animation callbacks", () => {
    const deferred: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => deferred.push(cb));
    const events: string[] = [];
    for (let frame = 0; frame < 60; frame++) {
      deliverPlayerFrame(() => events.push(`draw ${frame}`), drawn => {
        expect(drawn).toBe(true);
        events.push(`ack ${frame}`);
      });
    }
    expect(events).toHaveLength(120);
    expect(events.slice(0, 4)).toEqual(["draw 0", "ack 0", "draw 1", "ack 1"]);
    expect(deferred).toHaveLength(0);
  });

  it("releases a failed frame without claiming that it was drawn", () => {
    vi.stubGlobal("requestAnimationFrame", () => 1);
    const ack = vi.fn();
    expect(() => deliverPlayerFrame(() => { throw new Error("draw failed"); }, ack)).toThrow("draw failed");
    expect(ack).toHaveBeenCalledExactlyOnceWith(false);
  });
});
