// Session status helpers shared by Host (share) and Viewer (watch) popups.
// Viewer stats are measured live via RTCPeerConnection.getStats().
// Host stats are the configured targets + session snapshot (the native
// encoder bitrate target lives in Rust; actual received bitrate is what
// the Viewer measures — the mismatch diagnoses congestion vs config).

export type ViewerStats = {
  sampledAt: number;
  connectionState: string | null;
  iceState: string | null;
  bitrateMbps: number | null;
  resolution: string | null;
  fps: number | null;
  codec: string | null;
  jitterMs: number | null;
  packetsReceived: number | null;
  packetsLost: number | null;
  lossPercent: number | null;
  rttMs: number | null;
  framesDecoded: number | null;
  framesDropped: number | null;
  dropPercent: number | null;
  liveDelaySec: number | null;
};

export type ViewerStatsPrev = {
  bytesReceived: number;
  timestampMs: number;
} | null;

export const emptyViewerStats = (): ViewerStats => ({
  sampledAt: Date.now(),
  connectionState: null,
  iceState: null,
  bitrateMbps: null,
  resolution: null,
  fps: null,
  codec: null,
  jitterMs: null,
  packetsReceived: null,
  packetsLost: null,
  lossPercent: null,
  rttMs: null,
  framesDecoded: null,
  framesDropped: null,
  dropPercent: null,
  liveDelaySec: null,
});

/** Target encoder bitrate for each quality preset (matches Rust TransmissionQuality::bitrate). */
export const qualityTargetMbps: Record<"low" | "medium" | "high", number> = {
  low: 1.5,
  medium: 4,
  high: 8,
};

/** Limites do slider de bitrate custom (Mbps). O backend aceita 0.25–100. */
export const BITRATE_MIN_MBPS = 1;
export const BITRATE_MAX_MBPS = 50;
/** Limites do slider do piso (Mbps). Auto = ¼ do alvo. */
export const FLOOR_MIN_MBPS = 0.25;
export const FLOOR_MAX_MBPS = 10;

export const autoFloorMbps = (targetMbps: number) =>
  Math.max(FLOOR_MIN_MBPS, Math.round((targetMbps / 4) * 100) / 100);

export const qualityTargetLabel: Record<"low" | "medium" | "high", string> = {
  low: "720p 30 · 1.5 Mbps · H.264",
  medium: "1080p 30 · 4 Mbps · H.264",
  high: "1080p 60 · 8 Mbps · H.264",
};

export async function collectViewerStats(
  pc: RTCPeerConnection,
  video: HTMLVideoElement | null,
  prev: ViewerStatsPrev,
): Promise<{ stats: ViewerStats; prev: ViewerStatsPrev }> {
  const stats: ViewerStats = { ...emptyViewerStats(), connectionState: pc.connectionState ?? null, iceState: pc.iceConnectionState ?? null };
  try {
    const report = await pc.getStats();
    let inboundVideo: any = null;
    let pair: any = null;
    let codecMime: string | null = null;
    report.forEach((entry: any) => {
      if (entry.type === "inbound-rtp" && (entry.kind === "video" || entry.mediaType === "video") && !entry.isRemote) {
        if (!inboundVideo || (entry.framesReceived ?? 0) >= (inboundVideo.framesReceived ?? 0)) inboundVideo = entry;
      }
      if (entry.type === "candidate-pair" && (entry.nominated || entry.selected || entry.state === "succeeded")) {
        if (!pair || entry.nominated) pair = entry;
      }
    });
    if (inboundVideo) {
      const bytes = typeof inboundVideo.bytesReceived === "number" ? inboundVideo.bytesReceived : null;
      const now = typeof inboundVideo.timestamp === "number" ? inboundVideo.timestamp : Date.now();
      if (bytes !== null && prev) {
        const dt = (now - prev.timestampMs) / 1000;
        const db = bytes - prev.bytesReceived;
        if (dt > 0 && db >= 0) stats.bitrateMbps = Math.round(((db * 8) / dt / 1_000_000) * 100) / 100;
      }
      const nextPrev: ViewerStatsPrev = bytes !== null ? { bytesReceived: bytes, timestampMs: now } : prev;
      const w = inboundVideo.frameWidth as number | undefined;
      const h = inboundVideo.frameHeight as number | undefined;
      if (w && h) stats.resolution = w + "×" + h;
      if (typeof inboundVideo.framesPerSecond === "number") stats.fps = Math.round(inboundVideo.framesPerSecond * 10) / 10;
      if (typeof inboundVideo.jitter === "number") stats.jitterMs = Math.round(inboundVideo.jitter * 1000 * 10) / 10;
      if (typeof inboundVideo.packetsReceived === "number") stats.packetsReceived = inboundVideo.packetsReceived;
      if (typeof inboundVideo.packetsLost === "number") stats.packetsLost = inboundVideo.packetsLost;
      if (stats.packetsReceived !== null && stats.packetsLost !== null) {
        const total = stats.packetsReceived + stats.packetsLost;
        stats.lossPercent = total > 0 ? Math.round((stats.packetsLost / total) * 1000) / 10 : 0;
      }
      if (typeof inboundVideo.framesDecoded === "number") stats.framesDecoded = inboundVideo.framesDecoded;
      if (typeof inboundVideo.framesDropped === "number") stats.framesDropped = inboundVideo.framesDropped;
      if (stats.framesDecoded !== null && stats.framesDropped !== null) {
        const total = stats.framesDecoded + stats.framesDropped;
        stats.dropPercent = total > 0 ? Math.round((stats.framesDropped / total) * 1000) / 10 : 0;
      }
      if (typeof inboundVideo.codecId === "string") {
        const codec = report.get(inboundVideo.codecId) as any | undefined;
        if (codec?.mimeType) codecMime = String(codec.mimeType).replace("video/", "").toUpperCase();
      }
      if (stats.fps === null && video && video.videoWidth && video.videoHeight) {
        stats.resolution = stats.resolution ?? video.videoWidth + "×" + video.videoHeight;
      }
      stats.codec = codecMime;
      if (pair && typeof pair.currentRoundTripTime === "number") {
        stats.rttMs = Math.round(pair.currentRoundTripTime * 1000 * 10) / 10;
      }
      if (video && video.buffered.length && Number.isFinite(video.currentTime)) {
        try {
          const end = video.buffered.end(video.buffered.length - 1);
          const delay = end - video.currentTime;
          if (Number.isFinite(delay) && delay >= 0 && delay < 30) stats.liveDelaySec = Math.round(delay * 100) / 100;
        } catch { /* ignore */ }
      }
      return { stats, prev: nextPrev };
    }
    // No inbound video yet: still report RTT if we have a pair.
    report.forEach((entry: any) => {
      if (entry.type === "candidate-pair" && typeof entry.currentRoundTripTime === "number") {
        if (stats.rttMs === null || entry.nominated) stats.rttMs = Math.round(entry.currentRoundTripTime * 1000 * 10) / 10;
      }
    });
    return { stats, prev };
  } catch {
    return { stats, prev };
  }
}
