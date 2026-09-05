import { describe, expect, it } from "vitest";
import pkg from "../package.json";
import { APP_VERSION } from "./copy";
import {
  decideSalaOfferGate,
  nextSalaOfferBatch,
  partitionSalaOffers,
  shouldAutoUnwatch,
  type SalaOffer,
} from "./salaOffers";

// Sala frontend offer-loss + version guards.
// Failure A ("drain-then-drop"): the engine poll DRAINs the offer queue, so
// the Sala gate must peek-don't-drop — every offer is answered,
// auto-watched-then-answered, or requeued/buffered, never destroyed.

const offerX: SalaOffer = { from: "peer-x", sdp: "v=0 mock", offer_attempt: "attempt-1" };

describe("offer-no-loss (Sala gate peek-don't-drop)", () => {
  it("requeues an unwatched Sala offer instead of dropping it", () => {
    expect(
      decideSalaOfferGate({ inSala: true, watching: new Set(), roster: [], from: "peer-x" }),
    ).toBe("requeue");
  });

  it("preserves the offer across polls: second poll still yields offerX", () => {
    // Poll 1: [offerX], inSala, watching={} -> requeue. Engine lane missing
    // at runtime, so the App holds it in the local pending buffer.
    const first = partitionSalaOffers({
      inSala: true,
      watching: new Set(),
      roster: [],
      offers: [offerX],
    });
    expect(first.answer).toEqual([]);
    expect(first.autoWatch).toEqual([]);
    expect(first.requeue).toEqual([offerX]);

    // Poll 2: engine drained (fresh poll empty), pending buffer retries.
    const batch = nextSalaOfferBatch(first.requeue, []);
    expect(batch).toEqual([offerX]);

    // Poll 2 variant: engine re-yielded the requeued offer; no double-answer.
    expect(nextSalaOfferBatch([], [offerX, offerX])).toEqual([offerX]);
    expect(nextSalaOfferBatch([offerX], [offerX])).toEqual([offerX]);
  });

  it("auto-watches a roster-live sharer, then answers", () => {
    expect(
      decideSalaOfferGate({
        inSala: true,
        watching: new Set(),
        roster: [{ id: "peer-x", share: true }],
        from: "peer-x",
      }),
    ).toBe("auto-watch");
  });

  it("answers (PC + answer path) once the peer is watched", () => {
    expect(
      decideSalaOfferGate({
        inSala: true,
        watching: new Set(["peer-x"]),
        roster: [],
        from: "peer-x",
      }),
    ).toBe("answer");
  });

  it("answers everything outside Sala (Broadcast unchanged)", () => {
    expect(
      decideSalaOfferGate({ inSala: false, watching: new Set(), roster: [], from: "peer-x" }),
    ).toBe("answer");
  });

  it("never drops on a share:false flap: offer is held, not destroyed", () => {
    const routed = partitionSalaOffers({
      inSala: true,
      watching: new Set(),
      roster: [{ id: "peer-x", share: false }],
      offers: [offerX],
    });
    expect(routed.requeue).toEqual([offerX]);
    expect([...routed.answer, ...routed.autoWatch]).toEqual([]);
  });
});

describe("no-auto-unwatch-flap (watching intent retention)", () => {
  it("retains watching across a single share:false tick", () => {
    expect(
      shouldAutoUnwatch({
        inSala: true,
        roster: [{ id: "peer-x" }],
        selfId: "self",
        id: "peer-x",
      }),
    ).toBe(false);
  });

  it("retains watching while the roster is not known yet", () => {
    expect(
      shouldAutoUnwatch({ inSala: true, roster: [], selfId: "self", id: "peer-x" }),
    ).toBe(false);
  });

  it("unwatches only when the member is gone from a known roster", () => {
    expect(
      shouldAutoUnwatch({
        inSala: true,
        roster: [{ id: "other" }],
        selfId: "self",
        id: "peer-x",
      }),
    ).toBe(true);
  });

  it("never unwatches self or outside Sala", () => {
    expect(
      shouldAutoUnwatch({
        inSala: true,
        roster: [{ id: "other" }],
        selfId: "peer-x",
        id: "peer-x",
      }),
    ).toBe(false);
    expect(
      shouldAutoUnwatch({
        inSala: false,
        roster: [{ id: "other" }],
        selfId: "self",
        id: "peer-x",
      }),
    ).toBe(false);
  });
});

describe("APP_VERSION (single source of truth)", () => {
  it("matches package.json with zero manual bumps", () => {
    expect(APP_VERSION).toBe(pkg.version);
  });

  it("displays 0.6.0", () => {
    expect(APP_VERSION).toBe("0.6.0");
  });
});
