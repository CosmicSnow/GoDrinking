// Session status helpers shared by Host (share) and Viewer (watch) popups.
// Viewer stats are measured live via RTCPeerConnection.getStats().
// Host stats are the configured targets + session snapshot (the native
// encoder bitrate target lives in Rust; actual received bitrate is what
// the Viewer measures — the mismatch diagnoses congestion vs config).

export type ViewerStats = {
  sampledAt: number;
  connectionState: string | null;
  iceState: string | null;
  /** True once getStats() reports a nominated/selected candidate pair. */
  hasSelectedPair: boolean | null;
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
  hasSelectedPair: null,
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
  const stats: ViewerStats = { ...emptyViewerStats(), connectionState: pc.connectionState ?? null, iceState: pc.iceConnectionState ?? null, hasSelectedPair: null };
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
      stats.hasSelectedPair = pair !== null;
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
    let sawPair = false;
    report.forEach((entry: any) => {
      if (entry.type === "candidate-pair" && (entry.nominated || entry.selected || entry.state === "succeeded")) sawPair = true;
      if (entry.type === "candidate-pair" && typeof entry.currentRoundTripTime === "number") {
        if (stats.rttMs === null || entry.nominated) stats.rttMs = Math.round(entry.currentRoundTripTime * 1000 * 10) / 10;
      }
    });
    stats.hasSelectedPair = sawPair;
    return { stats, prev };
  } catch {
    return { stats, prev };
  }
}

// --- Phase-2C Viewer playback milestones ---------------------------------
// Pure helpers for the Viewer playback path. Milestone emission itself stays
// in App.tsx (it owns the video element + session/link ids); these helpers
// keep the classification and play() handling testable without DOM/Tauri.
//
// Privacy: milestones carry session code / link id / join mode / counts
// only. Never pass SDP, passwords, or tokens through here.

/** Ordered Viewer playback milestones (each emitted at most once per link). */
export type ViewerPlaybackMilestone =
  | "ontrack-fired"
  | "answer-declined-video"
  | "first-packets"
  | "first-decoded-frame"
  | "first-presentation"
  | "playback-blocked";

/** What the Viewer has observed beyond getStats() (element events/rVFC). */
export type ViewerPlaybackFlags = {
  decodedObserved: boolean;
  presentedObserved: boolean;
};

/** Diagnosable playback stage shown in the Watch UI (grounded, no hype). */
export type ViewerPlaybackStage =
  | "idle"
  | "no-path"
  | "waiting-packets"
  | "receiving-no-decode"
  | "decoded-not-presented"
  | "live";

/**
 * Classify where a Viewer link is stuck, from getStats() evidence plus
 * element observations. Mirrors AGENTS.md "Diagnosing reliability":
 * no selected pair -> signaling/ICE/network; packets but no decoded
 * frames -> codec/packetization; decoded but not presented -> playback.
 */
export function classifyViewerPlayback(
  stats: ViewerStats | null,
  flags: ViewerPlaybackFlags,
): ViewerPlaybackStage {
  if (flags.presentedObserved) return "live";
  if (flags.decodedObserved) return "decoded-not-presented";
  if (!stats) return "idle";
  const packets = stats.packetsReceived ?? 0;
  const decoded = stats.framesDecoded ?? 0;
  if (decoded > 0) return "decoded-not-presented";
  if (packets > 0) return "receiving-no-decode";
  if (stats.hasSelectedPair === true) return "waiting-packets";
  if (stats.hasSelectedPair === false) return "no-path";
  return "idle";
}

/** User-facing status line for a playback stage (Broadcast contract). */
export function viewerPlaybackStatusText(stage: ViewerPlaybackStage): string {
  switch (stage) {
    case "live":
      return "Live.";
    case "no-path":
      return "Still connecting — no network path yet (ICE has not picked a route).";
    case "waiting-packets":
      return "Route is up — waiting for the first packets…";
    case "receiving-no-decode":
      return "Receiving data but the video will not decode on this device.";
    case "decoded-not-presented":
      return "Video decoded but not showing — playback may be blocked.";
    case "idle":
    default:
      return "Waiting for media…";
  }
}

/**
 * Once-per-link milestone dedupe (keyed by link + kind). Returns true when
 * the milestone should be emitted; records it in `seen` so repeats and
 * re-renders stay silent. Pure for tests; App.tsx owns the actual logging.
 */
export function shouldEmitMilestone(
  seen: Set<string>,
  linkId: string,
  kind: ViewerPlaybackMilestone,
): boolean {
  const key = `${linkId}::${kind}`;
  if (seen.has(key)) return false;
  seen.add(key);
  return true;
}

/** Minimal play() surface so tests can fake rejection without DOM. */
export type Playable = {
  play: () => Promise<void> | void;
};

export type PlayOutcome = { ok: boolean; error: string | null };

/**
 * Start playback, catching rejections (autoplay policy, detached element).
 * Never throws and never leaves an unhandled rejection — callers log the
 * outcome as a milestone instead.
 */
export async function startVideoPlayback(video: Playable | null): Promise<PlayOutcome> {
  if (!video) return { ok: false, error: "no element" };
  try {
    const result = video.play();
    if (result && typeof (result as Promise<void>).catch === "function") {
      await (result as Promise<void>);
    }
    return { ok: true, error: null };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : typeof error === "string" ? error : "play failed",
    };
  }
}

// --- Phase-3B intent state-machine + admission + redacted diagnostics -----
// Pure, DOM-free intent machines for the Watch/Share flows. App.tsx owns the
// side effects (Tauri invokes, RTCPeerConnection, video element); these
// helpers own the transitions so tests can drive every edge without a PC.
// Privacy: intent/diagnostic payloads carry session code / link id / join
// mode / counts only. Never SDP, passwords, or tokens.
//
// Watch intent diagram (text):
//   idle --join--> joining --needs-approval--> waiting-approval
//     --admitted--> connecting --offer-ready--> connecting
//     --media-connected--> connected
//   joining/connecting/waiting-approval --blocked--> failed-blocked
//     --leave/reset--> idle ; connected --leave--> idle (leaving on the way)
// Share intent diagram (text):
//   idle --select-source--> selecting-source --start--> starting
//     --started--> sharing --stop--> stopping --stopped--> idle
//   starting/sharing/stopping --failed--> failed --reset--> idle
//   (select-source re-fires any time while sharing: the update is sent
//   immediately and the Session stays open.)

/** Explicit Watch (Viewer join) intent states. */
export type WatchIntentState =
  | "idle"
  | "joining"
  | "waiting-approval"
  | "connecting"
  | "connected"
  | "failed-blocked"
  | "leaving";

/** Events that advance the Watch intent machine. */
export type WatchIntentEvent =
  | "join"
  | "needs-approval"
  | "admitted"
  | "offer-ready"
  | "media-connected"
  | "blocked"
  | "leave"
  | "reset";

/** Explicit Share (Host capture slot) intent states. */
export type ShareIntentState =
  | "idle"
  | "selecting-source"
  | "starting"
  | "sharing"
  | "stopping"
  | "failed";

/** Events that advance the Share intent machine. */
export type ShareIntentEvent =
  | "select-source"
  | "start"
  | "started"
  | "stop"
  | "stopped"
  | "failed"
  | "reset";

/** Why a Watch link is stuck in failed-blocked (classifier-driven). */
export type JoinFailureKind =
  | "no-path"
  | "packets-no-decode"
  | "decoded-not-presented"
  | "declined"
  | "playback-blocked"
  | "error";

/** Admission state for a roster entry (pending vs admitted vs removed). */
export type AdmissionState = "pending" | "admitted" | "rejected" | "kicked";

/** Ordered Viewer playback milestones (canonical order for tests/docs). */
export const MILESTONE_ORDER: ViewerPlaybackMilestone[] = [
  "ontrack-fired",
  "first-packets",
  "first-decoded-frame",
  "first-presentation",
];

/** Side milestones that can fire at any point (not part of the order). */
export const MILESTONE_SIDELINES: ViewerPlaybackMilestone[] = [
  "answer-declined-video",
  "playback-blocked",
];

/** Redacted milestone payload: ids + counts only. No SDP/password/token. */
export type MilestonePayload = {
  milestone: ViewerPlaybackMilestone;
  session: string | null;
  link: string;
  joinMode: string;
  count: number;
};

/**
 * Advance the Watch intent machine. Unknown event/state pairs hold state
 * (never throw from UI input). `blocked` from joining/connecting lands in
 * failed-blocked; `leave` always drifts back toward idle.
 */
export function nextWatchIntent(
  current: WatchIntentState,
  event: WatchIntentEvent,
): WatchIntentState {
  switch (event) {
    case "reset":
      return "idle";
    case "leave":
      return current === "idle" ? "idle" : "leaving";
    case "join":
      return current === "idle" || current === "leaving" || current === "failed-blocked"
        ? "joining"
        : current;
    case "needs-approval":
      return current === "joining" ? "waiting-approval" : current;
    case "admitted":
      return current === "waiting-approval" || current === "joining" ? "connecting" : current;
    case "offer-ready":
      return current === "joining" || current === "waiting-approval" ? "connecting" : current;
    case "media-connected":
      return current === "connecting" || current === "joining" || current === "waiting-approval"
        ? "connected"
        : current;
    case "blocked":
      return current === "connected" || current === "idle" || current === "leaving"
        ? current
        : "failed-blocked";
    default:
      return current;
  }
}

/**
 * Advance the Share intent machine. Source selection is an explicit intent
 * that fires immediately (even mid-session); it never tears the Session.
 */
export function nextShareIntent(
  current: ShareIntentState,
  event: ShareIntentEvent,
): ShareIntentState {
  switch (event) {
    case "reset":
      return "idle";
    case "select-source":
      return current === "sharing" ? "sharing" : "selecting-source";
    case "start":
      return current === "idle" || current === "selecting-source" || current === "failed"
        ? "starting"
        : current;
    case "started":
      return current === "starting" ? "sharing" : current;
    case "stop":
      return current === "sharing" ? "stopping" : current;
    case "stopped":
      return current === "stopping" ? "idle" : current;
    case "failed":
      return current === "idle" ? "idle" : "failed";
    default:
      return current;
  }
}

/**
 * Map a playback stage to the join-failure bucket shown in failed-blocked.
 * Returns null while the link is still making progress (idle/waiting/live).
 */
export function joinFailureForStage(stage: ViewerPlaybackStage): JoinFailureKind | null {
  switch (stage) {
    case "no-path":
      return "no-path";
    case "waiting-packets":
      return "no-path";
    case "receiving-no-decode":
      return "packets-no-decode";
    case "decoded-not-presented":
      return "decoded-not-presented";
    case "idle":
    case "live":
    default:
      return null;
  }
}

/** User-facing Watch intent line (grounded, Broadcast contract). */
export function watchIntentStatusText(
  intent: WatchIntentState,
  stage: ViewerPlaybackStage,
): string {
  switch (intent) {
    case "idle":
      return "Not watching.";
    case "joining":
      return "Joining…";
    case "waiting-approval":
      return "Waiting for the Host to approve…";
    case "connecting":
      return viewerPlaybackStatusText(stage);
    case "connected":
      return stage === "live" ? "Live." : viewerPlaybackStatusText(stage);
    case "failed-blocked": {
      const kind = joinFailureForStage(stage);
      if (kind === "packets-no-decode") return viewerPlaybackStatusText("receiving-no-decode");
      if (kind === "decoded-not-presented") return viewerPlaybackStatusText("decoded-not-presented");
      if (kind === "no-path") return viewerPlaybackStatusText("no-path");
      return "Could not watch this Session. Try again.";
    }
    case "leaving":
      return "Leaving…";
    default:
      return "Waiting for media…";
  }
}

/** User-facing Share intent line (grounded, no hype). */
export function shareIntentStatusText(intent: ShareIntentState): string {
  switch (intent) {
    case "idle":
      return "Not sharing.";
    case "selecting-source":
      return "Source picked — applies right away.";
    case "starting":
      return "Starting capture…";
    case "sharing":
      return "Sharing live.";
    case "stopping":
      return "Stopping…";
    case "failed":
      return "Sharing failed.";
    default:
      return "Not sharing.";
  }
}

/**
 * Describe the source-selection intent (sent immediately, Session stays
 * open). Pure descriptor so the "applies right away" contract is testable.
 */
export function describeSourceSelectionIntent(
  sourceKind: "display" | "window",
  sourceId: number | null,
): { intent: "select-source"; source: "screen" | "window"; sourceId: number | null } {
  return {
    intent: "select-source",
    source: sourceKind === "display" ? "screen" : "window",
    sourceId,
  };
}

/**
 * Classify a roster entry state into pending / admitted / rejected / kicked.
 * Unknown backend states fall back to admitted when the entry is present in
 * the admitted roster, otherwise pending — never silently dropped.
 */
export function admissionStateFor(entryState: string): AdmissionState {
  const normalized = entryState.trim().toLowerCase();
  if (normalized === "pending" || normalized === "waiting" || normalized === "requested") return "pending";
  if (normalized === "rejected" || normalized === "declined" || normalized === "denied") return "rejected";
  if (normalized === "kicked" || normalized === "removed" || normalized === "banned") return "kicked";
  return "admitted";
}

/**
 * Build the redacted milestone payload. By construction it carries ids and
 * counts only — there is no field for SDP, passwords, or tokens, so callers
 * cannot leak them through here.
 */
export function buildMilestonePayload(
  milestone: ViewerPlaybackMilestone,
  session: string | null,
  link: string,
  joinMode: string,
  count = 1,
): MilestonePayload {
  return { milestone, session, link, joinMode, count };
}

/** True when every milestone in `list` respects MILESTONE_ORDER. */
export function isMilestoneOrderValid(list: ViewerPlaybackMilestone[]): boolean {
  let lastIndex = -1;
  for (const kind of list) {
    const index = MILESTONE_ORDER.indexOf(kind);
    if (index === -1) continue; // sidelines (answer-declined/playback-blocked) ignore order
    if (index < lastIndex) return false;
    lastIndex = index;
  }
  return true;
}

const SENSITIVE_PATTERN = /(sdp|password|passwd|token|secret|offer-den|v=0|m=video|m=audio|-----BEGIN)/i;

/**
 * Redaction guard for tests and the copy-paste diagnostics header: flags
 * payloads/text that look like they carry SDP, passwords, or tokens.
 */
export function containsSensitiveField(text: string): boolean {
  return SENSITIVE_PATTERN.test(text);
}
