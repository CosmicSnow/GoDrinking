import { describe, expect, it } from "vitest";
import {
  AnswerGenerationGuard,
  buildBroadcastAnswerEnvelope,
  buildSalaAnswerEnvelope,
  rejectedVideoLine,
  resolveAnswerIdentity,
  videoSectionRejected,
} from "./sdp";

// Mirrors the incident: the Windows host logged
// `m=video 0 UDP/TLS/RTP/SAVPF 0` and the Mac viewer sat silent.
const REJECTED = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 UDP/TLS/RTP/SAVPF 0\r\n";
const ACCEPTED =
  "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=rtpmap:102 H264/90000\r\n";

describe("videoSectionRejected", () => {
  it("detects the incident port-0 answer", () => {
    expect(videoSectionRejected(REJECTED)).toBe(true);
    expect(rejectedVideoLine(REJECTED)).toBe("m=video 0 UDP/TLS/RTP/SAVPF 0");
  });

  it("accepts a normal video answer", () => {
    expect(videoSectionRejected(ACCEPTED)).toBe(false);
    expect(rejectedVideoLine(ACCEPTED)).toBeNull();
  });

  it("handles lf-only sdp and multiple sections", () => {
    const multi = "m=audio 9 UDP/TLS/RTP/SAVPF 111\nm=video 0 UDP/TLS/RTP/SAVPF 102\n";
    expect(videoSectionRejected(multi)).toBe(true);
  });

  it("does not flag sdp without a video section", () => {
    expect(videoSectionRejected("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n")).toBe(false);
  });
});

describe("resolveAnswerIdentity (watcher-side member id, traced not guessed)", () => {
  it("prefers viewer_member_id for dual-role processes", () => {
    expect(
      resolveAnswerIdentity({ viewer_member_id: "viewer-9", self_id: "host-1" }),
    ).toBe("viewer-9");
  });

  it("falls back to self_id on the viewer branch", () => {
    expect(resolveAnswerIdentity({ self_id: "me" })).toBe("me");
  });

  it("is null when the snapshot has not arrived yet", () => {
    expect(resolveAnswerIdentity(null)).toBeNull();
    expect(resolveAnswerIdentity(undefined)).toBeNull();
    expect(resolveAnswerIdentity({})).toBeNull();
  });
});

describe("buildSalaAnswerEnvelope (id=self, to handled by caller)", () => {
  it("carries id=self + sdp + opaque attempt echo, never the sharer id", () => {
    const envelope = buildSalaAnswerEnvelope("me", "v=0-fake", '{"attempt":7}');
    expect(envelope).toEqual({
      type: "answer",
      sdp: "v=0-fake",
      id: "me",
      offer_attempt: '{"attempt":7}',
    });
    expect(envelope.id).not.toBe("sharer-1");
  });

  it("round-trips the attempt verbatim without interpreting it", () => {
    const attempt = '  {"epoch":{"session":[3]},"attempt":12}  ';
    expect(buildSalaAnswerEnvelope("me", "sdp", attempt).offer_attempt).toBe(attempt);
  });
});

describe("buildBroadcastAnswerEnvelope (id=host-minted viewer id)", () => {
  it("echoes the viewer id the host minted the offer for", () => {
    const envelope = buildBroadcastAnswerEnvelope("viewer-9", "v=0-fake", "b1");
    expect(envelope).toEqual({
      type: "answer",
      sdp: "v=0-fake",
      id: "viewer-9",
      offer_attempt: "b1",
    });
  });
});

describe("AnswerGenerationGuard (older redelivery never kills newer PC)", () => {
  it("a second begin supersedes the first", () => {
    const guard = new AnswerGenerationGuard();
    const older = guard.begin("sharer-1");
    expect(older.isCurrent()).toBe(true);
    const newer = guard.begin("sharer-1");
    expect(older.isCurrent()).toBe(false);
    expect(newer.isCurrent()).toBe(true);
  });

  it("invalidate supersedes in-flight invocations (explicit Unwatch)", () => {
    const guard = new AnswerGenerationGuard();
    const flight = guard.begin("sharer-1");
    guard.invalidate("sharer-1");
    expect(flight.isCurrent()).toBe(false);
  });

  it("tracks members independently and reset clears all", () => {
    const guard = new AnswerGenerationGuard();
    const a = guard.begin("a");
    const b = guard.begin("b");
    guard.invalidate("a");
    expect(a.isCurrent()).toBe(false);
    expect(b.isCurrent()).toBe(true);
    guard.reset();
    expect(b.isCurrent()).toBe(false);
  });
});
