import { describe, expect, it } from "vitest";
import { rejectedVideoLine, videoSectionRejected } from "./sdp";

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
