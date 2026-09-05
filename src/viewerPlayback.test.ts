import { describe, expect, it } from "vitest";
import {
  answerWithAttempt,
  offerDedupeKey,
  releaseOfferKey,
  shouldAcceptOffer,
  videoSectionRejected,
} from "./sdp";
import {
  admissionStateFor,
  buildMilestonePayload,
  classifyViewerPlayback,
  containsSensitiveField,
  describeSourceSelectionIntent,
  emptyViewerStats,
  isMilestoneOrderValid,
  joinFailureForStage,
  MILESTONE_ORDER,
  nextShareIntent,
  nextWatchIntent,
  shareIntentStatusText,
  shouldEmitMilestone,
  startVideoPlayback,
  viewerPlaybackStatusText,
  watchIntentStatusText,
  type ViewerPlaybackMilestone,
} from "./sessionStats";

// Phase-2C Viewer playback: milestones + diagnosable stages.

const REJECTED = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 UDP/TLS/RTP/SAVPF 0\r\n";
const ACCEPTED =
  "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000\r\n";

describe("videoSectionRejected (phase-2C guard stays)", () => {
  it("still flags a port-0 video answer", () => {
    expect(videoSectionRejected(REJECTED)).toBe(true);
  });

  it("still accepts a normal video answer", () => {
    expect(videoSectionRejected(ACCEPTED)).toBe(false);
  });
});

describe("startVideoPlayback", () => {
  it("resolves ok when play() succeeds", async () => {
    const outcome = await startVideoPlayback({ play: () => Promise.resolve() });
    expect(outcome).toEqual({ ok: true, error: null });
  });

  it("catches a rejected play() promise instead of throwing", async () => {
    const outcome = await startVideoPlayback({
      play: () => Promise.reject(new Error("NotAllowedError")),
    });
    expect(outcome.ok).toBe(false);
    expect(outcome.error).toContain("NotAllowedError");
  });

  it("catches a synchronous play() throw", async () => {
    const outcome = await startVideoPlayback({
      play: () => {
        throw new Error("detached element");
      },
    });
    expect(outcome.ok).toBe(false);
    expect(outcome.error).toContain("detached element");
  });

  it("handles a missing element without throwing", async () => {
    const outcome = await startVideoPlayback(null);
    expect(outcome.ok).toBe(false);
    expect(outcome.error).not.toBeNull();
  });
});

describe("shouldEmitMilestone", () => {
  it("emits each milestone once per link, but per link independently", () => {
    const seen = new Set<string>();
    expect(shouldEmitMilestone(seen, "host", "ontrack-fired")).toBe(true);
    expect(shouldEmitMilestone(seen, "host", "ontrack-fired")).toBe(false);
    expect(shouldEmitMilestone(seen, "host", "first-packets")).toBe(true);
    expect(shouldEmitMilestone(seen, "peer-2", "ontrack-fired")).toBe(true);
  });

  it("covers the full ordered milestone set", () => {
    const ordered: ViewerPlaybackMilestone[] = [
      "ontrack-fired",
      "answer-declined-video",
      "first-packets",
      "first-decoded-frame",
      "first-presentation",
      "playback-blocked",
    ];
    const seen = new Set<string>();
    for (const kind of ordered) {
      expect(shouldEmitMilestone(seen, "host", kind)).toBe(true);
      expect(shouldEmitMilestone(seen, "host", kind)).toBe(false);
    }
  });
});

describe("classifyViewerPlayback", () => {
  it("is idle with no stats and no observations", () => {
    expect(
      classifyViewerPlayback(null, { decodedObserved: false, presentedObserved: false }),
    ).toBe("idle");
  });

  it("reports no-path when ICE has no selected pair", () => {
    const stats = { ...emptyViewerStats(), hasSelectedPair: false };
    expect(
      classifyViewerPlayback(stats, { decodedObserved: false, presentedObserved: false }),
    ).toBe("no-path");
  });

  it("waits for packets once a route is up", () => {
    const stats = { ...emptyViewerStats(), hasSelectedPair: true };
    expect(
      classifyViewerPlayback(stats, { decodedObserved: false, presentedObserved: false }),
    ).toBe("waiting-packets");
  });

  it("reports packets-but-no-decode from getStats evidence", () => {
    const stats = {
      ...emptyViewerStats(),
      hasSelectedPair: true,
      packetsReceived: 120,
      framesDecoded: 0,
    };
    expect(
      classifyViewerPlayback(stats, { decodedObserved: false, presentedObserved: false }),
    ).toBe("receiving-no-decode");
  });

  it("reports decoded-but-not-presented from getStats evidence", () => {
    const stats = {
      ...emptyViewerStats(),
      hasSelectedPair: true,
      packetsReceived: 120,
      framesDecoded: 30,
    };
    expect(
      classifyViewerPlayback(stats, { decodedObserved: false, presentedObserved: false }),
    ).toBe("decoded-not-presented");
  });

  it("prefers element observations over stats", () => {
    const stats = { ...emptyViewerStats(), hasSelectedPair: true, packetsReceived: 5 };
    expect(
      classifyViewerPlayback(stats, { decodedObserved: true, presentedObserved: false }),
    ).toBe("decoded-not-presented");
    expect(
      classifyViewerPlayback(stats, { decodedObserved: true, presentedObserved: true }),
    ).toBe("live");
  });

  it("is live on presentation even without stats", () => {
    expect(
      classifyViewerPlayback(null, { decodedObserved: true, presentedObserved: true }),
    ).toBe("live");
  });
});

describe("viewerPlaybackStatusText", () => {
  it("distinguishes the three stuck states in plain language", () => {
    const noPath = viewerPlaybackStatusText("no-path");
    const noDecode = viewerPlaybackStatusText("receiving-no-decode");
    const noPresent = viewerPlaybackStatusText("decoded-not-presented");
    expect(new Set([noPath, noDecode, noPresent]).size).toBe(3);
    expect(noPath).toMatch(/ICE|network path/i);
    expect(noDecode).toMatch(/decod/i);
    expect(noPresent).toMatch(/playback|blocked|showing/i);
  });

  it("stays grounded and redacted", () => {
    const stages = [
      "idle",
      "no-path",
      "waiting-packets",
      "receiving-no-decode",
      "decoded-not-presented",
      "live",
    ] as const;
    for (const stage of stages) {
      const text = viewerPlaybackStatusText(stage);
      expect(text.length).toBeGreaterThan(0);
      expect(text).not.toMatch(/sdp|password|token/i);
    }
  });
});

// Phase-3B intent state-machine + diagnostics.

describe("nextWatchIntent (Watch join machine)", () => {
  it("walks join -> approval -> connecting -> connected", () => {
    let state = nextWatchIntent("idle", "join");
    expect(state).toBe("joining");
    state = nextWatchIntent(state, "needs-approval");
    expect(state).toBe("waiting-approval");
    state = nextWatchIntent(state, "admitted");
    expect(state).toBe("connecting");
    state = nextWatchIntent(state, "media-connected");
    expect(state).toBe("connected");
  });

  it("skips approval for Broadcast joins (offer-ready straight in)", () => {
    expect(nextWatchIntent("joining", "offer-ready")).toBe("connecting");
    expect(nextWatchIntent("joining", "media-connected")).toBe("connected");
  });

  it("lands stuck links in failed-blocked and leaves toward idle", () => {
    expect(nextWatchIntent("connecting", "blocked")).toBe("failed-blocked");
    expect(nextWatchIntent("joining", "blocked")).toBe("failed-blocked");
    expect(nextWatchIntent("waiting-approval", "blocked")).toBe("failed-blocked");
    expect(nextWatchIntent("failed-blocked", "leave")).toBe("leaving");
    expect(nextWatchIntent("connected", "leave")).toBe("leaving");
    expect(nextWatchIntent("leaving", "reset")).toBe("idle");
  });

  it("never blocks a live or idle link, and rejoins from failed", () => {
    expect(nextWatchIntent("connected", "blocked")).toBe("connected");
    expect(nextWatchIntent("idle", "blocked")).toBe("idle");
    expect(nextWatchIntent("failed-blocked", "join")).toBe("joining");
  });
});

describe("nextShareIntent (Host slot machine)", () => {
  it("walks start -> sharing -> stop -> idle", () => {
    expect(nextShareIntent("idle", "start")).toBe("starting");
    expect(nextShareIntent("starting", "started")).toBe("sharing");
    expect(nextShareIntent("sharing", "stop")).toBe("stopping");
    expect(nextShareIntent("stopping", "stopped")).toBe("idle");
  });

  it("keeps the Session open across source selection", () => {
    expect(nextShareIntent("sharing", "select-source")).toBe("sharing");
    expect(nextShareIntent("idle", "select-source")).toBe("selecting-source");
    expect(nextShareIntent("selecting-source", "start")).toBe("starting");
  });

  it("recovers from failure via reset/start", () => {
    expect(nextShareIntent("starting", "failed")).toBe("failed");
    expect(nextShareIntent("failed", "reset")).toBe("idle");
    expect(nextShareIntent("failed", "start")).toBe("starting");
  });
});

describe("describeSourceSelectionIntent", () => {
  it("maps display->screen and sends the id through immediately", () => {
    expect(describeSourceSelectionIntent("display", 3)).toEqual({
      intent: "select-source",
      source: "screen",
      sourceId: 3,
    });
    expect(describeSourceSelectionIntent("window", null)).toEqual({
      intent: "select-source",
      source: "window",
      sourceId: null,
    });
  });
});

describe("joinFailureForStage (classifier -> failed-blocked bucket)", () => {
  it("maps no-path, packets-no-decode, decoded-not-presented", () => {
    expect(joinFailureForStage("no-path")).toBe("no-path");
    expect(joinFailureForStage("waiting-packets")).toBe("no-path");
    expect(joinFailureForStage("receiving-no-decode")).toBe("packets-no-decode");
    expect(joinFailureForStage("decoded-not-presented")).toBe("decoded-not-presented");
  });

  it("stays null while progressing or live", () => {
    expect(joinFailureForStage("idle")).toBeNull();
    expect(joinFailureForStage("live")).toBeNull();
  });
});

describe("Stunar offer dedupe + opaque attempt echo", () => {
  it("keys dedupe by from + offer_attempt only", () => {
    expect(offerDedupeKey("a", "1")).toBe(JSON.stringify(["a", "1"]));
    expect(offerDedupeKey("a", "2")).not.toBe(offerDedupeKey("a", "1"));
    expect(offerDedupeKey("b", "1")).not.toBe(offerDedupeKey("a", "1"));
  });

  it("accepts each attempt once, per sender independently", () => {
    const seen = new Set<string>();
    expect(shouldAcceptOffer(seen, "a", "1")).toBe(true);
    expect(shouldAcceptOffer(seen, "a", "1")).toBe(false);
    expect(shouldAcceptOffer(seen, "a", "2")).toBe(true);
    expect(shouldAcceptOffer(seen, "b", "1")).toBe(true);
  });

  it("allows retry after release (failed attempt is not stuck)", () => {
    const seen = new Set<string>();
    expect(shouldAcceptOffer(seen, "a", "9")).toBe(true);
    releaseOfferKey(seen, "a", "9");
    expect(shouldAcceptOffer(seen, "a", "9")).toBe(true);
  });

  it("echoes offer_attempt opaquely into the answer", () => {
    const answer = answerWithAttempt({ type: "answer", sdp: "s", id: "a" }, "attempt-42");
    expect(answer.offer_attempt).toBe("attempt-42");
    expect(answer.sdp).toBe("s");
  });
});

describe("milestone ordering + once-per-link", () => {
  it("orders ontrack -> packets -> decoded -> presentation", () => {
    expect(MILESTONE_ORDER).toEqual([
      "ontrack-fired",
      "first-packets",
      "first-decoded-frame",
      "first-presentation",
    ]);
    expect(isMilestoneOrderValid([...MILESTONE_ORDER])).toBe(true);
    expect(
      isMilestoneOrderValid(["first-presentation", "ontrack-fired"]),
    ).toBe(false);
  });

  it("ignores sidelines in the order check", () => {
    expect(
      isMilestoneOrderValid([
        "ontrack-fired",
        "answer-declined-video",
        "first-packets",
        "playback-blocked",
        "first-decoded-frame",
        "first-presentation",
      ]),
    ).toBe(true);
  });

  it("still emits each milestone once per link", () => {
    const seen = new Set<string>();
    const ordered: ViewerPlaybackMilestone[] = [
      "ontrack-fired",
      "first-packets",
      "first-decoded-frame",
      "first-presentation",
    ];
    for (const kind of ordered) {
      expect(shouldEmitMilestone(seen, "host", kind)).toBe(true);
      expect(shouldEmitMilestone(seen, "host", kind)).toBe(false);
    }
  });
});

describe("intent/admission status text", () => {
  it("names each Watch intent stage in plain language", () => {
    expect(watchIntentStatusText("idle", "idle")).toMatch(/not watching/i);
    expect(watchIntentStatusText("joining", "idle")).toMatch(/joining/i);
    expect(watchIntentStatusText("waiting-approval", "idle")).toMatch(/approv/i);
    expect(watchIntentStatusText("connecting", "no-path")).toMatch(/ICE|network path/i);
    expect(watchIntentStatusText("connected", "live")).toBe("Live.");
    expect(watchIntentStatusText("leaving", "live")).toMatch(/leaving/i);
  });

  it("maps failed-blocked through the classifier buckets", () => {
    expect(watchIntentStatusText("failed-blocked", "no-path")).toMatch(/ICE|network path/i);
    expect(watchIntentStatusText("failed-blocked", "receiving-no-decode")).toMatch(/decod/i);
    expect(watchIntentStatusText("failed-blocked", "decoded-not-presented")).toMatch(
      /playback|blocked|showing/i,
    );
  });

  it("names each Share intent stage without hype", () => {
    expect(shareIntentStatusText("idle")).toMatch(/not sharing/i);
    expect(shareIntentStatusText("selecting-source")).toMatch(/right away/i);
    expect(shareIntentStatusText("starting")).toMatch(/starting/i);
    expect(shareIntentStatusText("sharing")).toMatch(/live/i);
    expect(shareIntentStatusText("stopping")).toMatch(/stopping/i);
    expect(shareIntentStatusText("failed")).toMatch(/failed/i);
  });

  it("keeps every intent line grounded and redacted", () => {
    const intents = [
      "idle",
      "joining",
      "waiting-approval",
      "connecting",
      "connected",
      "failed-blocked",
      "leaving",
    ] as const;
    const stages = [
      "idle",
      "no-path",
      "waiting-packets",
      "receiving-no-decode",
      "decoded-not-presented",
      "live",
    ] as const;
    for (const intent of intents) {
      for (const stage of stages) {
        const text = watchIntentStatusText(intent, stage);
        expect(text.length).toBeGreaterThan(0);
        expect(text).not.toMatch(/sdp|password|token/i);
        expect(containsSensitiveField(text)).toBe(false);
      }
    }
  });
});

describe("admission + redaction", () => {
  it("separates pending vs admitted vs rejected/kicked", () => {
    expect(admissionStateFor("pending")).toBe("pending");
    expect(admissionStateFor("waiting")).toBe("pending");
    expect(admissionStateFor("connected")).toBe("admitted");
    expect(admissionStateFor("new")).toBe("admitted");
    expect(admissionStateFor("rejected")).toBe("rejected");
    expect(admissionStateFor("declined")).toBe("rejected");
    expect(admissionStateFor("kicked")).toBe("kicked");
    expect(admissionStateFor("removed")).toBe("kicked");
  });

  it("builds milestone payloads with ids/counts only", () => {
    const payload = buildMilestonePayload("first-packets", "ABC123", "host", "lan", 2);
    expect(payload).toEqual({
      milestone: "first-packets",
      session: "ABC123",
      link: "host",
      joinMode: "lan",
      count: 2,
    });
    const serialized = JSON.stringify(payload);
    expect(containsSensitiveField(serialized)).toBe(false);
  });

  it("flags sensitive-looking diagnostics text", () => {
    expect(containsSensitiveField("m=video 0 UDP/TLS/RTP/SAVPF 0")).toBe(true);
    expect(containsSensitiveField("answer sdp: v=0")).toBe(true);
    expect(containsSensitiveField("password=hunter2")).toBe(true);
    expect(containsSensitiveField("token abc123")).toBe(true);
    expect(containsSensitiveField("link host · join lan · session ABC123")).toBe(false);
  });
});
