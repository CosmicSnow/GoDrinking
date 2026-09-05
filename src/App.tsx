import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import "./index.css";
import "./App.css";
import logo from "./assets/logo.png";
import { APP_VERSION, detectLocale, dictionaries, type Copy, type Locale } from "./copy";
import { RoomStage } from "./RoomStage";
import { autoFloorMbps, BITRATE_MAX_MBPS, BITRATE_MIN_MBPS, FLOOR_MAX_MBPS, FLOOR_MIN_MBPS, collectViewerStats, qualityTargetMbps, type ViewerStats, type ViewerStatsPrev } from "./sessionStats";
import { videoSectionRejected } from "./sdp";

type IconName = "grid" | "monitor" | "window" | "game" | "settings" | "help" | "plus" | "copy" | "wifi" | "chevron" | "expand" | "minimize" | "volume" | "volume-off" | "terminal" | "activity" | "folder";
function Icon({ name, size = 18 }: { name: IconName; size?: number }) {
  const paths: Record<IconName, ReactNode> = {
    grid: <><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/></>,
    monitor: <><rect x="3" y="4" width="18" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></>,
    window: <><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M7 6.5h.01M10 6.5h.01"/></>,
    game: <><path d="M7.5 8h9a5 5 0 0 1 4.8 6.4l-1 3.2a2 2 0 0 1-3.4.8l-2.2-2.4H9.3l-2.2 2.4a2 2 0 0 1-3.4-.8l-1-3.2A5 5 0 0 1 7.5 8Z"/><path d="M8 11v4M6 13h4M16.5 12.5h.01M18.5 15h.01"/></>,
    settings: <><circle cx="12" cy="12" r="3"/><path d="M19.4 15a2 2 0 0 1-2.8 2.8"/></>,
    help: <><circle cx="12" cy="12" r="9"/><path d="M9.7 9a2.4 2.4 0 1 1 4.1 1.7c-1.2 1.1-1.8 1.3-1.8 2.8M12 16.8h.01"/></>,
    plus: <path d="M12 5v14M5 12h14"/>,
    copy: <><rect x="8" y="8" width="11" height="11" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></>,
    wifi: <><path d="M3 8.5a14 14 0 0 1 18 0M6 12a9.5 9.5 0 0 1 12 0M9.5 15.5a5 5 0 0 1 5 0"/><path d="M12 19h.01"/></>,
    chevron: <path d="m9 18 6-6-6-6"/>,
    expand: <path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/>,
    minimize: <path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/>,
    volume: <><path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.8 5.8a9 9 0 0 1 0 12.4"/></>,
    "volume-off": <><path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/></>,
    terminal: <><path d="m4 17 6-6-6-6"/><path d="M12 19h8"/></>,
    activity: <><path d="M3 12h4l3 8 4-16 3 8h4"/></>,
    folder: <><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></>
  };
  return <svg className="icon" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{paths[name]}</svg>;
}

type Source = { id: number; kind: "display" | "window"; title?: string; application_name?: string };
type RunningApp = { name: string; bundle_id?: string | null; pid: number; emitting_audio?: boolean };
type Capabilities = { platform?: string; supported: boolean; native_capture_implemented: boolean; screen_capture_kit: boolean; source_enumeration_available: boolean; screen_recording_authorization: "granted" | "not_granted" | "unsupported"; app_audio_exclusion: "enhanced" | "best_effort" | "unsupported"; wasapi?: boolean; process_loopback?: boolean; av1_encode_supported?: boolean; detail: string };
type RosterEntry = { id: string; nickname: string; state: string; master?: boolean; share?: boolean };
type IncomingOffer = { from: string; sdp: string };
type DirectAddress = { ip: string; port: number; version: number; kind: string; copy: string };
type Snapshot = { state: string; session_id: string | null; source_id: number | null; bitrate_bps: number | null; native_capture_active: boolean; preview_callback_count: number; preview_frame_count: number; preview_dropped_count: number; preview_error: string | null; detail: string; peer_state: string; peer_detail: string; session_code: string | null; lan_addresses: string[]; lan_port: number | null; roster?: RosterEntry[]; self_id?: string | null; session_mode?: "broadcast" | "room"; password_set?: boolean; admission?: boolean; join_mode?: string; direct_listen_port?: number | null; direct_addresses?: DirectAddress[]; direct_mapping?: boolean; stunar_state?: string | null; resolution?: string | null; frame_rate?: string | null };
type Signal = { type: "offer" | "answer"; sdp: string; id?: string };
type LogSession = { session: string; timestamp: string; lines: string[] };
type ViewerLinkStats = { id: string; nickname: string; state: string; rtt_ms: number | null };
type SessionLinkStats = { links: ViewerLinkStats[]; target_bps: number; congestion_bps: number | null; floor_bps: number };

const invokeMedia = <T,>(command: string, args?: Record<string, unknown>) => invoke<T>(command, args);
// Tiny WebAudio blips for viewer connect/disconnect (no audio assets).
// Connect rises, disconnect falls; quiet by design.
let blipCtx: AudioContext | null = null;
const blipTone = (freq: number, delay: number, duration: number) => {
  try {
    if (!blipCtx) blipCtx = new AudioContext();
    if (blipCtx.state === "suspended") void blipCtx.resume();
    const start = blipCtx.currentTime + delay;
    const osc = blipCtx.createOscillator();
    const gain = blipCtx.createGain();
    osc.type = "sine";
    osc.frequency.value = freq;
    gain.gain.setValueAtTime(0.0001, start);
    gain.gain.exponentialRampToValueAtTime(0.12, start + 0.015);
    gain.gain.exponentialRampToValueAtTime(0.0001, start + duration);
    osc.connect(gain).connect(blipCtx.destination);
    osc.start(start);
    osc.stop(start + duration + 0.02);
  } catch { /* audio unavailable; stay silent */ }
};
const playConnectBlip = () => { blipTone(660, 0, 0.09); blipTone(990, 0.09, 0.14); };
const playDisconnectBlip = () => { blipTone(520, 0, 0.09); blipTone(360, 0.09, 0.14); };
const diagnosticError = (error: unknown, fallback: string) => error instanceof Error ? error.message : typeof error === "string" ? error : fallback;
const rgbToRgba = (payload: number[], width: number, height: number) => {
  const expected = width * height * 3;
  if (payload.length !== expected) return null;
  const rgba = new Uint8ClampedArray(width * height * 4);
  for (let rgb = 0, pixel = 0; rgb < payload.length; rgb += 3, pixel += 4) {
    rgba[pixel] = payload[rgb];
    rgba[pixel + 1] = payload[rgb + 1];
    rgba[pixel + 2] = payload[rgb + 2];
    rgba[pixel + 3] = 255;
  }
  return rgba;
};
const iceServersFor = (joinMode: string) => joinMode === "lan" ? [] : [{ urls: ["stun:stun.l.google.com:19302"] }];
const waitIce = (pc: RTCPeerConnection) => new Promise<void>((resolve) => {
  if (pc.iceGatheringState === "complete") { resolve(); return; }
  const done = () => { if (pc.iceGatheringState === "complete") { pc.removeEventListener("icegatheringstatechange", done); resolve(); } };
  pc.addEventListener("icegatheringstatechange", done);
  window.setTimeout(() => resolve(), 8000);
});
const qualityPresets = {
  low: { resolution: "720p", frame_rate: "30_fps" },
  medium: { resolution: "1080p", frame_rate: "30_fps" },
  high: { resolution: "1080p", frame_rate: "60_fps" }
} as const;
const taglines = [
  "Rage against the machine",
  "The cake is a lie",
  "War. War never changes.",
  "Fight the power",
  "Information wants to be free",
  "No gods, no masters",
  "Wake up, Neo",
  "Nothing is true, everything is permitted",
  "The revolution will not be televised",
  "Speak, even if your voice shakes",
  "I never asked for this",
  "Censorship is a dead end",
  "Think for yourself",
  "Be water, my friend",
  "The internet interprets censorship as damage",
  "Would you kindly",
  "Do you feel like a hero yet?",
  "Be realistic, demand the impossible",
  "Sous les pavés, la plage",
  "It is forbidden to forbid",
  "If I can't dance, it's not my revolution",
  "When injustice becomes law, resistance becomes duty",
  "Who controls the past controls the future",
  "Question authority",
  "Silence is consent",
  "Speak truth to power",
  "You can't evict an idea",
  "Another world is possible",
  "No pasarán",
  "A luta continua",
  "Independência ou morte",
  "Liberdade ainda que tardia",
  "Quem cala consente"
];

function App() {
  const [locale, setLocale] = useState<Locale>(() => detectLocale());
  const copy: Copy = dictionaries[locale];
  const setLocalePersist = (next: Locale) => {
    setLocale(next);
    try { localStorage.setItem("godrinking.locale", next); } catch { /* ignore */ }
  };
  const [mode, setMode] = useState<"share" | "watch">("share");
  const [caps, setCaps] = useState<Capabilities | null>(null);
  const [sources, setSources] = useState<Source[]>([]);
  const [apps, setApps] = useState<RunningApp[]>([]);
  const [sourceKind, setSourceKind] = useState<"display" | "window">("display");
  const [sourceId, setSourceId] = useState<number | null>(null);
  const [quality, setQuality] = useState<"low" | "medium" | "high">("high");
  // Limite custom do encoder em Mbps. null = seguir o preset de qualidade.
  const [bitrateMbps, setBitrateMbps] = useState<number | null>(null);
  const effectiveMbps = bitrateMbps ?? qualityTargetMbps[quality];
  // Piso custom anti-colapso REMB em Mbps. null = 1 Mbps automático.
  const [minBitrateMbps, setMinBitrateMbps] = useState<number | null>(null);
  // Codec da sessão, fixo no Start (HEVC e H.264 High exigem macOS).
  const [videoCodec, setVideoCodec] = useState<"h264" | "h264high" | "hevc" | "av1">("h264");
  const [videoEncoder, setVideoEncoder] = useState<"auto" | "software" | "hardware">("auto");
  const hevcAvailable = caps?.platform === "macos";
  const av1Available = caps?.av1_encode_supported === true;
  // Resolução/fps explícitos, fixos no Start. "auto" segue o preset de
  // qualidade. A saída respeita o aspecto da fonte e nunca faz upscale.
  const [resolution, setResolution] = useState<"auto" | "480p" | "720p" | "1080p" | "1440p" | "2160p">("auto");
  const [frameFps, setFrameFps] = useState<"auto" | "30_fps" | "60_fps" | "120_fps">("auto");
  const resolvedResolution = resolution === "auto" ? qualityPresets[quality].resolution : resolution;
  const resolvedFrameRate = frameFps === "auto" ? qualityPresets[quality].frame_rate : frameFps;
  const [qualityOpen, setQualityOpen] = useState(false);
  const codecLabel = videoCodec === "hevc" ? "HEVC" : videoCodec === "av1" ? "AV1" : videoCodec === "h264high" ? "H.264 High" : "H.264";
  const qualitySummary = qualityOpen
    ? `${quality[0].toUpperCase()}${quality.slice(1)} · ${resolvedResolution} · ${resolvedFrameRate.replace("_fps", "fps")} · ${codecLabel}`
    : `${quality[0].toUpperCase()}${quality.slice(1)} · ${copy.qualityLine[quality].split(" · ").slice(0, 2).join(" · ")}`;
  const effectiveFloorMbps = Math.min(minBitrateMbps ?? autoFloorMbps(effectiveMbps), effectiveMbps);
  const applyPreset = (next: "low" | "medium" | "high") => {
    setQuality(next);
    setBitrateMbps(null);
    setMinBitrateMbps(null);
    setVideoCodec("h264");
    setResolution("auto");
    setFrameFps("auto");
  };
  const [systemAudio, setSystemAudio] = useState(false);
  const [excludedApps, setExcludedApps] = useState<string[]>([]);
  const [appSearch, setAppSearch] = useState("");
  const [playingOnly, setPlayingOnly] = useState(false);
  const [session, setSession] = useState<Snapshot | null>(null);
  const [joinCode, setJoinCode] = useState("");
  const [sessionMode, setSessionMode] = useState<"broadcast" | "room">("broadcast");
  const [benchNote, setBenchNote] = useState("");
  const [stageId, setStageId] = useState<string>("host");
  const remotesRef = useRef<Map<string, { pc: RTCPeerConnection; stream: MediaStream }>>(new Map());
  const [remoteIds, setRemoteIds] = useState<string[]>([]);
  const [watching, setWatching] = useState<Set<string>>(() => new Set());
  const watchingRef = useRef<Set<string>>(new Set());
  watchingRef.current = watching;
  const [pinned, setPinned] = useState<string | null>(null);
  const [roomJoined, setRoomJoined] = useState(false);
  const [roomDesk, setRoomDesk] = useState(false);
  const salaAliveRef = useRef(false);
  const seenOffers = useRef<Set<string>>(new Set());
  const rejectCountRef = useRef<Map<string, number>>(new Map());
  const [joinMode, setJoinMode] = useState<"lan" | "direct" | "stunar">(() => {
    const saved = localStorage.getItem("godrinking.join_mode");
    return saved === "direct" || saved === "stunar" ? saved : "lan";
  });
  const [directHost, setDirectHost] = useState(() => localStorage.getItem("godrinking.direct_host") ?? "");
  const [rendezvousUrl, setRendezvousUrl] = useState(() => localStorage.getItem("godrinking.rendezvous_url") ?? "");
  const [nickname, setNickname] = useState(() => localStorage.getItem("godrinking.nickname") ?? "");
  const [hostPassword, setHostPassword] = useState("");
  const [hostAdmission, setHostAdmission] = useState(false);
  const [joinPassword, setJoinPassword] = useState("");
  const [notice, setNotice] = useState("");
  const [copied, setCopied] = useState(false);
  const [sessionAction, setSessionAction] = useState<"idle" | "starting" | "stopping" | "joining">("idle");
  const [watchIce, setWatchIce] = useState<"idle" | "connecting" | "connected" | "lost">("idle");
  const [watchCode, setWatchCode] = useState("");
  const [watchHostName, setWatchHostName] = useState("");
  const [watchZoom, setWatchZoom] = useState(1);
  const [cinema, setCinema] = useState(false);
  const [windowFullscreen, setWindowFullscreen] = useState(false);
  const [elementFullscreen, setElementFullscreen] = useState(false);
  const [watchVolume, setWatchVolume] = useState(80);
  const [watchMuted, setWatchMuted] = useState(false);
  const [escHint, setEscHint] = useState(false);
  const [tagline, setTagline] = useState(() => taglines[Math.floor(Math.random() * taglines.length)]);
  const [statsOpen, setStatsOpen] = useState(false);
  const [viewerStats, setViewerStats] = useState<ViewerStats | null>(null);
  const [hostStats, setHostStats] = useState<SessionLinkStats | null>(null);
  const hostLinks = hostStats?.links ?? null;
  const congestionMbps = hostStats?.congestion_bps != null ? Math.round(hostStats.congestion_bps / 1e5) / 10 : null;
  const floorAppliedMbps = hostStats ? Math.round(hostStats.floor_bps / 1e5) / 10 : null;
  const [hostPreviewFps, setHostPreviewFps] = useState<number | null>(null);
  const statsPrevRef = useRef<ViewerStatsPrev>(null);
  const hostPreviewPrevRef = useRef<{ frames: number; t: number } | null>(null);
  const [logsOpen, setLogsOpen] = useState(false);
  const [logs, setLogs] = useState<LogSession[]>([]);
  const [activeLog, setActiveLog] = useState(0);
  const [lastError, setLastError] = useState("");
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const remoteRef = useRef<HTMLVideoElement>(null);
  const peerRef = useRef<RTCPeerConnection | null>(null);
  const remoteStreamRef = useRef<MediaStream | null>(null);
  const liveSettingsApplied = useRef(false);
  const failedNoticeRef = useRef(false);
  const joinSeqRef = useRef(0);
  const active = session?.state === "running" || Boolean(session?.native_capture_active);
  const connected = session?.peer_state === "connected";
  const watchConnected = watchIce === "connected";
  const watchStreamActive = watchConnected || watchIce === "connecting";
  const watchLabel = joinMode === "direct" ? (watchHostName || "via Direct") : watchCode;
  const lanConnected = mode === "watch" ? watchConnected : connected;
  // Windows has no Screen Recording gate (authorization is "unsupported"):
  // capture readiness is native_capture_implemented there.
  const captureReady = caps !== null && (caps.platform === "windows" ? caps.native_capture_implemented : caps.screen_recording_authorization === "granted");
  const canStart = Boolean(caps?.supported && captureReady);
  const audioExclusion = caps?.app_audio_exclusion === "enhanced";
  // Windows per-app exclusion via WASAPI process loopback (one app per session).
  const windowsExclusion = caps?.process_loopback === true;
  const exclusionListAvailable = audioExclusion || windowsExclusion;
  // Windows has no per-app exclusion, but full-device WASAPI loopback works.
  const systemAudioSupported = audioExclusion || caps?.wasapi === true;
  const roomLabel = session?.session_code ? `${session.session_code}${session.lan_addresses[0] ? ` · ${session.lan_addresses[0]}` : ""}` : "";
  const nicknameValid = /^[A-Za-z0-9 _.-]+$/.test(nickname.trim()) && nickname.trim().length >= 2 && nickname.trim().length <= 24;
  const passwordValid = (password: string) => password.length >= 4 && password.length <= 64;
  const stunarHostPasswordValid = joinMode !== "stunar" || passwordValid(hostPassword);
  const stunarJoinPasswordValid = joinMode !== "stunar" || passwordValid(joinPassword);
  const canOpenRoom = nicknameValid && (joinMode !== "stunar" || (Boolean(rendezvousUrl.trim()) && stunarHostPasswordValid));
  const inSala = salaAliveRef.current && (roomJoined || session?.session_mode === "room");
  const onStage = inSala && !roomDesk;
  const roster = session?.roster ?? [];
  const pendingRoster = roster.filter((entry) => entry.state === "pending");
  const connectedRoster = roster.filter((entry) => entry.state !== "pending" && entry.id !== session?.self_id);
  const [stickyPeople, setStickyPeople] = useState<RosterEntry[]>([]);
  const roomPeople = stickyPeople;
  const roomTiles = [
    ...(session?.native_capture_active ? [{ id: "local", nickname: nickname.trim() || "You", stream: null as MediaStream | null, local: true }] : []),
    ...remoteIds.filter((id) => id !== "local" && id !== session?.self_id && watching.has(id)).map((id) => {
      const slot = remotesRef.current.get(id);
      const person = stickyPeople.find((entry) => entry.id === id) ?? roster.find((entry) => entry.id === id);
      return {
        id,
        nickname: person?.nickname || id.slice(0, 8),
        stream: slot?.stream ?? null,
        local: false,
        pc: slot?.pc ?? null
      };
    })
  ];
  const watchIcePrevRef = useRef<"idle" | "connecting" | "connected" | "lost">("idle");
  const rosterIdsRef = useRef<string[]>([]);
  // Watcher ears: beep when our own link connects or drops.
  useEffect(() => {
    const prev = watchIcePrevRef.current;
    if (prev !== watchIce) {
      if (watchIce === "connected") playConnectBlip();
      else if (prev === "connected") playDisconnectBlip();
      watchIcePrevRef.current = watchIce;
    }
  }, [watchIce]);
  // Host ears: beep when a viewer joins or leaves the connected roster.
  useEffect(() => {
    if (mode !== "share" || !active) { rosterIdsRef.current = []; return; }
    const ids = connectedRoster.map((entry) => entry.id);
    const prev = rosterIdsRef.current;
    if (ids.some((id) => !prev.includes(id))) playConnectBlip();
    else if (prev.some((id) => !ids.includes(id))) playDisconnectBlip();
    rosterIdsRef.current = ids;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, active, connectedRoster]);
  const directAddresses = session?.direct_addresses ?? [];
  const joinModeHelp = {
    lan: mode === "share" ? copy.joinLanHost : copy.joinLanWatch,
    direct: mode === "share" ? copy.joinDirectHost : copy.joinDirectWatch,
    stunar: mode === "share" ? copy.joinStunarHost : copy.joinStunarWatch
  }[joinMode];

  useEffect(() => {
    localStorage.setItem("godrinking.nickname", nickname);
  }, [nickname]);
  useEffect(() => {
    localStorage.setItem("godrinking.join_mode", joinMode);
  }, [joinMode]);
  useEffect(() => {
    localStorage.setItem("godrinking.rendezvous_url", rendezvousUrl);
  }, [rendezvousUrl]);
  useEffect(() => {
    localStorage.setItem("godrinking.direct_host", directHost);
  }, [directHost]);

  const refreshCapabilities = async () => {
    try {
      const next = await invokeMedia<Capabilities>("get_media_capabilities");
      setCaps(next);
      return next;
    } catch {
      setNotice("Native media commands are unavailable in this app build.");
      return null;
    }
  };
  const loadSources = async () => {
    try {
      const next = await invokeMedia<Source[]>("get_media_capture_sources");
      setSources(next);
      if (next.length && sourceId === null) setSourceId(next.find((item) => item.kind === sourceKind)?.id ?? next[0].id);
      return next;
    } catch (error) {
      setNotice(diagnosticError(error, "Native sources could not be listed."));
      return null;
    }
  };
  const loadApps = async () => {
    try {
      const next = await invokeMedia<RunningApp[]>("get_media_running_apps");
      setApps(next);
    } catch { /* keep the last list; polled */ }
  };

  useEffect(() => { void refreshCapabilities(); }, []);
  useEffect(() => {
    if (!caps?.source_enumeration_available) return;
    void loadSources();
    void loadApps();
  }, [caps?.source_enumeration_available]);
  useEffect(() => {
    if (mode !== "share" || !systemAudio || !exclusionListAvailable) return undefined;
    void loadApps();
    const timer = window.setInterval(() => void loadApps(), 1000);
    return () => window.clearInterval(timer);
  }, [mode, systemAudio, exclusionListAvailable]);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWindow().onFocusChanged(({ payload }) => {
      if (payload) void refreshCapabilities();
    }).then((cleanup) => { unlisten = cleanup; });
    return () => { unlisten?.(); };
  }, []);
  useEffect(() => {
    const win = getCurrentWindow();
    let unlisten: (() => void) | undefined;
    const sync = () => { void win.isFullscreen().then(setWindowFullscreen).catch(() => undefined); };
    sync();
    void win.onResized(sync).then((cleanup) => { unlisten = cleanup; });
    return () => { unlisten?.(); };
  }, []);
  useEffect(() => {
    const sync = () => setElementFullscreen(Boolean(document.fullscreenElement));
    document.addEventListener("fullscreenchange", sync);
    sync();
    return () => document.removeEventListener("fullscreenchange", sync);
  }, []);
  useEffect(() => {
    const el = remoteRef.current;
    if (!el) return;
    el.muted = watchMuted;
    el.volume = watchMuted ? 0 : watchVolume / 100;
  }, [watchVolume, watchMuted, watchConnected, mode]);
  useEffect(() => {
    if (!watchConnected) return undefined;
    const timer = window.setInterval(() => {
      const el = remoteRef.current;
      if (!el || el.paused || el.seeking || el.readyState < 2) return;
      const end = el.buffered.length ? el.buffered.end(el.buffered.length - 1) : 0;
      if (Number.isFinite(end) && end - el.currentTime > 0.25) el.currentTime = end - 0.05;
    }, 2000);
    return () => window.clearInterval(timer);
  }, [watchConnected]);
  useEffect(() => {
    if (mode !== "watch") setCinema(false);
  }, [mode]);
  // Session status popup: poll live stats 1x/sec while open.
  // Watch mode measures the real received stream via getStats();
  // Share mode shows encoder targets + capture health from the snapshot.
  useEffect(() => {
    if (!statsOpen) return undefined;
    const tick = async () => {
      if (mode === "watch") {
        const pc = peerRef.current;
        if (!pc) { setViewerStats(null); return; }
        const { stats, prev } = await collectViewerStats(pc, remoteRef.current, statsPrevRef.current);
        statsPrevRef.current = prev;
        setViewerStats(stats);
      } else {
        const frames = session?.preview_frame_count;
        const now = Date.now();
        const prev = hostPreviewPrevRef.current;
        if (typeof frames === "number" && prev) {
          const dt = (now - prev.t) / 1000;
          if (dt > 0.5) {
            setHostPreviewFps(Math.round(((frames - prev.frames) / dt) * 10) / 10);
            hostPreviewPrevRef.current = { frames, t: now };
          }
        } else if (typeof frames === "number") {
          hostPreviewPrevRef.current = { frames, t: now };
          setHostPreviewFps(null);
        }
        try {
          const stats = await invokeMedia<SessionLinkStats>("get_media_session_stats");
          setHostStats(stats);
        } catch { /* mantém a última leitura */ }
      }
    };
    void tick();
    const timer = window.setInterval(() => void tick(), 1000);
    return () => window.clearInterval(timer);
  }, [statsOpen, mode, session?.preview_frame_count]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "=") return;
      const target = event.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT" || target.isContentEditable)) return;
      event.preventDefault();
      setTagline((current) => {
        let next = current;
        while (next === current) next = taglines[Math.floor(Math.random() * taglines.length)];
        return next;
      });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  useEffect(() => {
    const el = remoteRef.current;
    const stream = remoteStreamRef.current;
    if (el && stream && el.srcObject !== stream) el.srcObject = stream;
  }, [mode, watchConnected]);
  useEffect(() => {
    if (!cinema) return undefined;
    setEscHint(true);
    const timer = window.setTimeout(() => setEscHint(false), 2700);
    return () => window.clearTimeout(timer);
  }, [cinema]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      if (cinema) { setCinema(false); return; }
      if (document.fullscreenElement) { void document.exitFullscreen().catch(() => undefined); return; }
      const win = getCurrentWindow();
      void win.isFullscreen().then((fullscreen) => {
        if (!fullscreen) return undefined;
        setWindowFullscreen(false);
        return win.setFullscreen(false).catch(() => undefined);
      }).catch(() => undefined);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cinema]);
  useEffect(() => {
    setSourceId(sources.find((item) => item.kind === sourceKind)?.id ?? null);
  }, [sourceKind, sources]);
  useEffect(() => {
    if (!active && !roomJoined) return undefined;
    const poll = async () => {
      try {
        if (!salaAliveRef.current) return;
        const next = await invokeMedia<Snapshot>("get_media_session_state");
        if (!salaAliveRef.current) return;
        setSession(next);
        if (next.preview_error) setNotice(`Native preview: ${next.preview_error}`);
        if (next.state === "failed" && !failedNoticeRef.current) {
          failedNoticeRef.current = true;
          setNotice(next.detail || "Screen sharing failed.");
        } else if (next.state !== "failed" && failedNoticeRef.current) {
          failedNoticeRef.current = false;
        }
        const frame = await invokeMedia<{ width: number; height: number; encoding: string; payload: number[] } | null>("get_media_preview");
        if (frame && frame.encoding === "rgb8_thumbnail" && canvasRef.current) {
          const rgba = rgbToRgba(frame.payload, frame.width, frame.height);
          if (!rgba) return;
          const canvas = canvasRef.current;
          canvas.width = frame.width;
          canvas.height = frame.height;
          canvas.getContext("2d")?.putImageData(new ImageData(rgba, frame.width, frame.height), 0, 0);
        }
      } catch { /* next tick */ }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), 500);
    return () => window.clearInterval(timer);
  }, [active, roomJoined]);
  useEffect(() => {
    if (!inSala) {
      setStickyPeople([]);
      return;
    }
    const incoming = (session?.roster ?? []).filter((entry) => entry.state !== "pending");
    setStickyPeople((prev) => {
      if (incoming.length === 0) {
        if (prev.length > 0) return prev;
        if (session?.self_id) return [{ id: session.self_id, nickname: nickname.trim() || "You", state: "new", master: true, share: Boolean(session.native_capture_active) }];
        return prev;
      }
      const byId = new Map(incoming.map((entry) => [entry.id, entry]));
      if (session?.self_id && !byId.has(session.self_id)) {
        byId.set(session.self_id, { id: session.self_id, nickname: nickname.trim() || "You", state: "new", master: true, share: Boolean(session.native_capture_active) });
      }
      return [...byId.values()];
    });
  }, [inSala, session?.roster, session?.self_id, session?.native_capture_active, nickname]);
  useEffect(() => {
    if (!active || !session?.session_id) { liveSettingsApplied.current = false; return; }
    if (sessionAction !== "idle") return;
    if (!liveSettingsApplied.current) { liveSettingsApplied.current = true; return; }
    void invokeMedia<Snapshot | null>("update_media_session", { request: { quality, bitrate_bps: bitrateMbps !== null ? Math.round(bitrateMbps * 1_000_000) : null, min_bitrate_bps: minBitrateMbps !== null ? Math.round(minBitrateMbps * 1_000_000) : null, resolution: resolution === "auto" ? null : resolution, frame_rate: frameFps === "auto" ? null : frameFps, codec: videoCodec, encoder: videoEncoder, system_audio: systemAudio, excluded_apps: excludedApps } })
      .then((next) => { if (next) setSession(next); })
      .catch((error) => setNotice(`Could not apply the change: ${diagnosticError(error, "unknown error")}`));
  }, [quality, bitrateMbps, minBitrateMbps, resolution, frameFps, systemAudio, excludedApps, active, sessionAction]);
  useEffect(() => () => {
    peerRef.current?.close();
    void invokeMedia("close_media_peer_transport").catch(() => undefined).then(() => invokeMedia("stop_media_session").catch(() => undefined));
  }, []);

  const requestPermission = async () => {
    try {
      const next = await invokeMedia<Capabilities>("request_media_screen_recording_permission");
      setCaps(next);
      setNotice(next.source_enumeration_available ? next.detail : "Turn on goDrinking in System Settings → Privacy & Security → Screen Recording, then quit the app and open it again.");
      if (next.source_enumeration_available) {
        void loadSources();
        void loadApps();
      }
    } catch (error) {
      setNotice(diagnosticError(error, "Screen Recording permission could not be checked."));
    }
  };
  const runBenchmark = async () => {
    try {
      const report = await invokeMedia<{ recommended: "low" | "medium" | "high"; note: string }>("run_media_benchmark");
      applyPreset(report.recommended);
      setBenchNote(report.note);
      try { localStorage.setItem("godrinking.benchmark", JSON.stringify({ ...report, at: Date.now() })); } catch { /* ignore */ }
    } catch (error) {
      setBenchNote(diagnosticError(error, "Could not measure this PC."));
    }
  };
  const startRoomShare = async () => {
    if (sessionAction !== "idle") return;
    if (!canStart) {
      setNotice(caps?.platform === "windows" ? "Native capture is unavailable on this PC." : "Grant Screen Recording access first.");
      return;
    }
    if (sourceId === null && filteredSources.length > 0) {
      setNotice("Choose a display or window before sharing.");
      return;
    }
    setSessionAction("starting");
    setNotice(copy.startHint);
    try {
      if (session?.session_id) {
        const next = await invokeMedia<Snapshot>("start_media_share");
        setSession(next);
        setNotice(next.detail || "Your screen is going out. They still don't go through the server.");
        return;
      }
      const next = await invokeMedia<Snapshot>("create_media_session", {
        request: {
          source: sourceKind === "display" ? "screen" : "window",
          source_id: sourceId,
          quality,
          bitrate_bps: bitrateMbps !== null ? Math.round(bitrateMbps * 1_000_000) : null,
          min_bitrate_bps: minBitrateMbps !== null ? Math.round(minBitrateMbps * 1_000_000) : null,
          codec: videoCodec,
          encoder: videoEncoder,
          resolution: resolvedResolution,
          frame_rate: resolvedFrameRate,
          system_audio: systemAudio,
          excluded_apps: excludedApps,
          password: joinPassword || hostPassword,
          nickname: nickname.trim() || "Host",
          admission: hostAdmission,
          join_mode: joinMode,
          rendezvous_url: joinMode === "stunar" ? rendezvousUrl.trim() : null,
          session_mode: "room",
          attach_only: true,
          share_on_start: true
        }
      });
      setSession(next);
      await invokeMedia("announce_media_share", { start: true }).catch(() => undefined);
      setNotice(next.detail || "Your screen is going out. They still don't go through the server.");
    } catch (error) {
      const message = diagnosticError(error, "Could not share your screen.");
      setLastError(message);
      setNotice(message);
    } finally {
      setSessionAction("idle");
    }
  };
  const stopRoomShare = async () => {
    setSessionAction("stopping");
    try {
      const next = await invokeMedia<Snapshot>("stop_media_share");
      setSession(next);
      setNotice("You stopped sharing. Still in the room.");
    } catch (error) {
      setNotice(diagnosticError(error, "Could not stop sharing."));
    } finally {
      setSessionAction("idle");
    }
  };
  const watchMember = async (id: string) => {
    watchingRef.current = new Set(watchingRef.current).add(id);
    setWatching(new Set(watchingRef.current));
    try {
      await invokeMedia("stunar_watch", { request: { to: id, start: true } });
    } catch (error) {
      setNotice(diagnosticError(error, "Could not watch that person."));
    }
  };
  const unwatchMember = async (id: string) => {
    const next = new Set(watchingRef.current);
    next.delete(id);
    watchingRef.current = next;
    setWatching(next);
    const slot = remotesRef.current.get(id);
    slot?.pc.close();
    remotesRef.current.delete(id);
    setRemoteIds([...remotesRef.current.keys()]);
    if (pinned === id) setPinned(null);
    try {
      await invokeMedia("stunar_watch", { request: { to: id, start: false } });
    } catch { /* already gone */ }
  };
  const startSharing = async () => {
    if (sessionAction !== "idle") return;
    setSessionAction("starting");
    setNotice(copy.startHint);
    try {
      const current = await refreshCapabilities();
      const hostCaptureReady = current !== null && (current.platform === "windows" ? current.native_capture_implemented : current.screen_recording_authorization === "granted");
      if (sessionMode !== "room") {
        if (!current?.supported || !hostCaptureReady) throw new Error(current?.platform === "windows" ? "Native capture is unavailable on this PC." : "Grant Screen Recording access first.");
        if (sourceId === null && filteredSources.length > 0) throw new Error("Choose a display or window before starting.");
      }
      if (!nicknameValid) throw new Error("Enter a nickname (2–24 letters, numbers, spaces, _ - .).");
      if (joinMode === "stunar" && !rendezvousUrl.trim()) throw new Error("Set the Stunar URL in settings.");
      if (joinMode === "stunar" && !passwordValid(hostPassword)) throw new Error("Stunar requires a password (4–64 characters).");
      const next = await invokeMedia<Snapshot>("create_media_session", {
        request: {
          source: sourceKind === "display" ? "screen" : "window",
          source_id: sourceId,
          quality,
          bitrate_bps: bitrateMbps !== null ? Math.round(bitrateMbps * 1_000_000) : null,
          min_bitrate_bps: minBitrateMbps !== null ? Math.round(minBitrateMbps * 1_000_000) : null,
          codec: videoCodec,
          encoder: videoEncoder,
          resolution: resolvedResolution,
          frame_rate: resolvedFrameRate,
          system_audio: systemAudio,
          excluded_apps: excludedApps,
          password: hostPassword,
          nickname: nickname.trim() || "Host",
          admission: hostAdmission,
          join_mode: joinMode,
          rendezvous_url: joinMode === "stunar" ? rendezvousUrl.trim() : null,
          session_mode: sessionMode,
          attach_only: false,
          share_on_start: sessionMode !== "room"
        }
      });
      setSession(next);
      if (sessionMode === "room") {
        salaAliveRef.current = true;
        setRoomJoined(true);
        setRoomDesk(false);
      }
      setNotice(next.session_code ? (sessionMode === "room" ? `Room ${next.session_code} is open. Share if you want.` : `Session ${next.session_code} is live. Share that code.`) : next.detail);
    } catch (error) {
      const recovered = await invokeMedia<Snapshot>("get_media_session_state").catch(() => null);
      setSession(recovered && recovered.state !== "idle" ? recovered : null);
      const message = diagnosticError(error, "Native capture could not start.");
      setLastError(message);
      setNotice(message);
    } finally {
      setSessionAction("idle");
    }
  };
  const clearSalaUi = () => {
    salaAliveRef.current = false;
    joinSeqRef.current += 1;
    peerRef.current?.close();
    peerRef.current = null;
    remoteStreamRef.current = null;
    if (remoteRef.current) remoteRef.current.srcObject = null;
    remotesRef.current.forEach((slot) => slot.pc.close());
    remotesRef.current.clear();
    seenOffers.current.clear();
    setRemoteIds([]);
    setStageId("host");
    setRoomJoined(false);
    setRoomDesk(false);
    setWatching(new Set());
    watchingRef.current = new Set();
    setPinned(null);
    setSession(null);
    setWatchIce("idle");
    setWatchCode("");
    setWatchHostName("");
    setWatchZoom(1);
    setSessionAction("idle");
    leaveImmersive();
  };
  const leaveSala = () => {
    clearSalaUi();
    setNotice("Left the room.");
    const cleanup = () => invokeMedia("close_media_peer_transport").catch(() => undefined)
      .then(() => invokeMedia("stunar_viewer_close").catch(() => undefined))
      .then(() => invokeMedia("stop_media_session").catch(() => undefined));
    void cleanup().then(() => {
      if (salaAliveRef.current) return;
      window.setTimeout(() => { if (!salaAliveRef.current) void cleanup(); }, 1200);
    });
  };
  const stopSharing = async () => {
    clearSalaUi();
    setSessionAction("stopping");
    try {
      await invokeMedia("close_media_peer_transport").catch(() => undefined);
      await invokeMedia("stunar_viewer_close").catch(() => undefined);
      await invokeMedia("stop_media_session");
      setNotice("Session stopped.");
    } catch (error) {
      setNotice(diagnosticError(error, "Native session could not stop."));
    } finally {
      setSessionAction("idle");
    }
  };
  const zoomStep = (direction: 1 | -1) => setWatchZoom((current) => Math.min(3, Math.max(1, Math.round((current + direction * 0.25) * 100) / 100)));
  const toggleFullscreen = async () => {
    if (document.fullscreenElement) {
      await document.exitFullscreen().catch(() => undefined);
      return;
    }
    const win = getCurrentWindow();
    try {
      const next = !(await win.isFullscreen());
      await win.setFullscreen(next);
      setWindowFullscreen(next);
    } catch {
      const el = remoteRef.current as (HTMLVideoElement & { webkitRequestFullscreen?: () => Promise<void> | void }) | null;
      if (el?.requestFullscreen) await el.requestFullscreen().catch(() => undefined);
      else el?.webkitRequestFullscreen?.();
    }
  };
  const leaveImmersive = () => {
    setCinema(false);
    if (document.fullscreenElement) void document.exitFullscreen().catch(() => undefined);
    const win = getCurrentWindow();
    void win.isFullscreen().then((fullscreen) => {
      if (!fullscreen) return undefined;
      setWindowFullscreen(false);
      return win.setFullscreen(false).catch(() => undefined);
    }).catch(() => undefined);
  };
  const disconnectWatch = () => {
    leaveSala();
  };
  const acceptIncomingOffer = async (incoming: IncomingOffer) => {
    if (inSala && !watchingRef.current.has(incoming.from)) return;
    const offerKey = incoming.from + incoming.sdp.slice(0, 24);
    if (seenOffers.current.has(offerKey)) return;
    seenOffers.current.add(offerKey);
    // A failed attempt must be retryable on the next poll: without this,
    // one throw leaves the viewer stuck on "incoming offers" forever with
    // no error anywhere (the Mac-viewer symptom of the Windows incident).
    const allowRetry = () => { seenOffers.current.delete(offerKey); };
    const existing = remotesRef.current.get(incoming.from);
    try {
      existing?.pc.close();
      const pc = new RTCPeerConnection({ iceServers: iceServersFor(joinMode) });
      const stream = new MediaStream();
      pc.ontrack = (event) => {
        event.streams[0]?.getTracks().forEach((track) => stream.addTrack(track));
        if (!event.streams[0] && event.track) stream.addTrack(event.track);
        remotesRef.current.set(incoming.from, { pc, stream });
        setRemoteIds([...remotesRef.current.keys()].filter((id, index, all) => all.indexOf(id) === index));
        if (stageId === "host" && incoming.from !== "host") setStageId(incoming.from);
        const el = document.querySelector<HTMLVideoElement>(`video[data-slot="${incoming.from}"]`);
        if (el && el.srcObject !== stream) el.srcObject = stream;
      };
      remotesRef.current.set(incoming.from, { pc, stream });
      await pc.setRemoteDescription({ type: "offer", sdp: incoming.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      await waitIce(pc);
      const localSdp = pc.localDescription?.sdp;
      if (!localSdp) {
        pc.close();
        allowRetry();
        return;
      }
      // Never ship a dead answer: a browser with no decoder for the session
      // codec answers m=video 0, which only fails loudly on the host while
      // this viewer stays silent. Surface it here and retry (bounded: the
      // poll dedups after two consecutive rejections of the same offer).
      if (videoSectionRejected(localSdp)) {
        pc.close();
        const rejects = (rejectCountRef.current.get(offerKey) ?? 0) + 1;
        rejectCountRef.current.set(offerKey, rejects);
        if (rejects < 2) allowRetry();
        setNotice("This browser could not use the session video (no common decoder).");
        return;
      }
      rejectCountRef.current.delete(offerKey);
      await invokeMedia("send_stunar_room_answer", { request: { to: incoming.from, answer: { type: "answer", sdp: localSdp, id: incoming.from } } });
    } catch (error) {
      allowRetry();
      setNotice(diagnosticError(error, "Could not answer the session."));
    }
  };
  useEffect(() => {
    if (!watchConnected && !active && !roomJoined) return;
    const timer = window.setInterval(() => {
      void invokeMedia<IncomingOffer[]>("poll_stunar_offers").then((offers) => {
        for (const offer of offers) void acceptIncomingOffer(offer);
      }).catch(() => undefined);
    }, 1000);
    return () => window.clearInterval(timer);
  }, [watchConnected, active, roomJoined, joinMode, stageId, inSala]);
  useEffect(() => {
    if (!inSala) return;
    for (const id of [...watching]) {
      const person = roster.find((entry) => entry.id === id);
      if (!person || !person.share) void unwatchMember(id);
    }
  }, [inSala, roster, watching]);
  const joinRoom = async () => {
    if (sessionAction !== "idle") return;
    if (!nicknameValid) {
      setNotice("Enter a nickname (2–24 letters, numbers, spaces, _ - .).");
      return;
    }
    if (joinMode === "stunar") {
      if (!rendezvousUrl.trim()) {
        setNotice("Set the Stunar URL in settings.");
        return;
      }
      if (!passwordValid(joinPassword)) {
        setNotice("Stunar requires a password (4–64 characters).");
        return;
      }
      const code = joinCode.trim().toUpperCase();
      if (code.length < 4) {
        setNotice("Enter the 6-character session code.");
        return;
      }
    } else if (joinMode === "lan") {
      const code = joinCode.trim().toUpperCase();
      if (code.length < 4) {
        setNotice("Enter the 6-character session code.");
        return;
      }
    } else {
      const host = directHost.trim();
      if (!host || !host.includes(":")) {
        setNotice("Enter the host address and port (e.g. 192.168.1.40:41234 or [2001:db8::1]:41234).");
        return;
      }
    }
    const targetLabel = joinMode === "lan" ? joinCode.trim().toUpperCase() : joinMode === "direct" ? directHost.trim() : joinCode.trim().toUpperCase();
    setSessionAction("joining");
    setNotice(joinMode === "lan" ? "Looking for the host on your network…" : joinMode === "direct" ? "Connecting to the host…" : "Waiting for approval…");
    const seq = ++joinSeqRef.current;
    try {
      const request = joinMode === "lan"
        ? { code: joinCode.trim().toUpperCase(), password: joinPassword, nickname: nickname.trim() }
        : joinMode === "direct"
          ? { join_mode: "direct", host: directHost.trim(), password: joinPassword, nickname: nickname.trim() }
          : { join_mode: "stunar", code: joinCode.trim().toUpperCase(), password: joinPassword, nickname: nickname.trim(), rendezvous_url: rendezvousUrl.trim() };
      const [host, offer, hostName] = await invokeMedia<[string, Signal, string]>("discover_media_room", { request });
      if (joinSeqRef.current !== seq) return;
      const displayLabel = joinMode === "direct" ? (hostName.trim() || "via Direct") : targetLabel;
      if (!offer.sdp) {
        setWatchCode(targetLabel);
        setWatchHostName(hostName.trim() || "");
        salaAliveRef.current = true;
        setRoomJoined(true);
        setSessionMode("room");
        setWatchIce("connected");
        setRoomDesk(false);
        const snap = await invokeMedia<Snapshot>("get_media_session_state").catch(() => null);
        if (salaAliveRef.current && snap) setSession(snap);
        setNotice(`In the room ${displayLabel}. Watch who you want.`);
        return;
      }
      const pc = new RTCPeerConnection({ iceServers: iceServersFor(joinMode) });
      peerRef.current?.close();
      peerRef.current = pc;
      setWatchIce("connecting");
      setWatchCode(targetLabel);
      setWatchHostName(hostName.trim() || "");
      pc.ontrack = (event) => {
        const stream = event.streams[0] ?? new MediaStream(event.track ? [event.track] : []);
        remoteStreamRef.current = stream;
        remotesRef.current.set("host", { pc, stream });
        setRemoteIds(["host", ...[...remotesRef.current.keys()].filter((id) => id !== "host")]);
        setStageId("host");
        if (remoteRef.current && remoteRef.current.srcObject !== stream) remoteRef.current.srcObject = stream;
      };
      await pc.setRemoteDescription({ type: "offer", sdp: offer.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      await waitIce(pc);
      const local = pc.localDescription;
      if (!local?.sdp) throw new Error("The viewer could not create an answer.");
      if (videoSectionRejected(local.sdp)) {
        throw new Error("This browser refused the session video (no common decoder — try another browser).");
      }
      const handleIce = () => {
        if (peerRef.current !== pc) return;
        const state = pc.iceConnectionState;
        if (state === "connected" || state === "completed") {
          setWatchIce("connected");
          setNotice(`Connected to ${displayLabel}.`);
        } else if (state === "failed" || state === "disconnected" || state === "closed") {
          setWatchIce("lost");
          setNotice("Connection lost.");
          setWatchZoom(1);
          leaveImmersive();
        }
      };
      pc.oniceconnectionstatechange = handleIce;
      handleIce();
      await invokeMedia("submit_media_room_answer", { request: { host, answer: { type: "answer", sdp: local.sdp, id: offer.id }, join_mode: joinMode } });
      if (joinSeqRef.current !== seq || peerRef.current !== pc) return;
      if (pc.iceConnectionState !== "connected" && pc.iceConnectionState !== "completed") setNotice(`Joined ${displayLabel}. Waiting for media…`);
    } catch (error) {
      peerRef.current?.close();
      peerRef.current = null;
      if (joinSeqRef.current !== seq) return;
      setWatchIce("idle");
      const message = diagnosticError(error, "Could not join.");
      setLastError(message);
      setNotice(message.includes("full") ? "This session is full." : message.includes("declined") ? "The host declined." : message.includes("banned") ? "Could not join." : message);
    } finally {
      setSessionAction("idle");
    }
  };
  const copyText = async (text: string) => {
    await navigator.clipboard?.writeText(text);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
  };
  const loadLogs = async () => {
    try {
      const next = await invokeMedia<LogSession[]>("get_app_logs");
      setLogs(next);
      // Preserve the selected tab across refreshes (newest session stays
      // tab 0, so a clamp keeps the reader where they were).
      setActiveLog((prev) => Math.max(0, Math.min(prev, Math.max(0, next.length - 1))));
    } catch { /* keep the last list */ }
  };
  const openLogs = async () => {
    setLogsOpen(true);
    setActiveLog(0);
    await loadLogs();
  };
  const closeLogs = () => setLogsOpen(false);
  useEffect(() => {
    if (!logsOpen) return undefined;
    const timer = window.setInterval(() => void loadLogs(), 2000);
    return () => window.clearInterval(timer);
  }, [logsOpen]);
  const copyActiveLog = async () => {
    const active = logs[activeLog];
    if (!active) return;
    await copyText(active.lines.join("\n"));
  };
  // One paste covers the whole investigation: diagnostics header plus every
  // session (newest first), so host and viewer logs travel together.
  const copyAllLogs = async () => {
    const header = [
      `goDrinking diagnostics · ${APP_VERSION}`,
      `join mode: ${joinMode}`,
      `rendezvous: ${rendezvousUrl || "—"}`,
      `last error: ${lastError || "—"}`,
      `sessions: ${logs.length}`,
    ].join("\n");
    const body = logs.map((entry) => `===== ${entry.session} (${entry.timestamp}) =====\n${entry.lines.join("\n")}`).join("\n\n");
    await copyText(`${header}\n\n${body}`);
  };
  const openLogsFolder = async () => {
    try {
      const dir = await invokeMedia<string | null>("get_logs_dir");
      if (!dir) { setNotice("Could not locate the logs folder."); return; }
      await revealItemInDir(dir);
    } catch (error) {
      setNotice(diagnosticError(error, "Could not open the logs folder."));
    }
  };
  const clearLogs = async () => {
    try {
      await invokeMedia("clear_app_logs");
      setLogs([]);
      setActiveLog(0);
    } catch { /* keep the list */ }
  };
  const logLabel = (session: string) => {
    const name = session.replace(/^session-/, "").replace(/\.log$/, "");
    const parts = name.split("-");
    if (parts.length >= 4) {
      const [date, time, role, mode] = parts;
      return `${role} · ${mode} · ${date} ${time}`;
    }
    return name;
  };
  const copyCode = async () => {
    if (!session?.session_code) return;
    await copyText(session.session_code);
  };
  const admitViewer = async (id: string) => {
    try {
      const next = await invokeMedia<Snapshot>("admit_media_viewer", { request: { id } });
      setSession(next);
    } catch (error) {
      setNotice(diagnosticError(error, "Could not accept the viewer."));
    }
  };
  const rejectViewer = async (id: string) => {
    try {
      const next = await invokeMedia<Snapshot>("reject_media_viewer", { request: { id } });
      setSession(next);
    } catch (error) {
      setNotice(diagnosticError(error, "Could not decline the viewer."));
    }
  };
  const kickViewer = async (id: string) => {
    try {
      const next = await invokeMedia<Snapshot>("kick_media_viewer", { request: { id } });
      setSession(next);
    } catch (error) {
      setNotice(diagnosticError(error, "Could not disconnect the viewer."));
    }
  };
  const applyCredentials = async () => {
    if (joinMode === "stunar" && !passwordValid(hostPassword)) {
      setNotice("Stunar requires a password (4–64 characters).");
      return;
    }
    try {
      const next = await invokeMedia<Snapshot>("update_media_session_credentials", {
        request: { password: hostPassword, admission: hostAdmission }
      });
      setSession(next);
      setNotice(hostPassword ? "Password updated. Connected viewers stay." : "Password removed. Connected viewers stay.");
    } catch (error) {
      setNotice(diagnosticError(error, "Could not update the session credentials."));
    }
  };
  const toggleAdmission = async (next: boolean) => {
    setHostAdmission(next);
    try {
      const updated = await invokeMedia<Snapshot>("update_media_session_credentials", {
        request: { admission: next }
      });
      setSession(updated);
    } catch (error) {
      setNotice(diagnosticError(error, "Could not change the approval rule."));
    }
  };
  const toggleExcludedApp = (token: string) => setExcludedApps((current) => current.includes(token) ? current.filter((item) => item !== token) : [...current, token]);
  const excludeQuery = appSearch.trim().toLowerCase();
  const visibleApps = apps.filter((app) => {
    if (playingOnly && app.emitting_audio !== true) return false;
    if (excludeQuery && !app.name.toLowerCase().includes(excludeQuery)) return false;
    return true;
  });
  const filteredSources = sources.filter((item) => item.kind === sourceKind);
  const permissionLabel = caps === null ? copy.permissionChecking : caps.platform === "windows" ? (caps.native_capture_implemented ? copy.permissionWinReady : copy.permissionUnavailable) : caps.screen_recording_authorization === "granted" ? copy.permissionReady : caps.screen_recording_authorization === "not_granted" ? copy.permissionNeeded : copy.permissionUnavailable;

  return (
    <div className={`app-shell${cinema ? " cinema" : ""}${onStage ? " room-live" : ""}`}>
      <aside className="sidebar">
        <div className="brand">
          <img className="brand-logo" src={logo} alt=""/>
          <div className="brand-stack">
            <span>goDrinking</span>
            <span className="brand-tagline" title={tagline}>{tagline}</span>
          </div>
        </div>
        <div className="nav-label">{copy.workspace}</div>
        <nav aria-label="Main navigation">
          <button className={`nav-item ${mode === "share" ? "active" : ""}`} onClick={() => setMode("share")}><Icon name="grid"/> {copy.shareNav}</button>
          <button className={`nav-item ${mode === "watch" ? "active" : ""}`} onClick={() => setMode("watch")}><Icon name="monitor"/> {copy.watchNav}{watchStreamActive ? <span className="nav-live"><i/>Live</span> : null}</button>
        </nav>
        <div className="sidebar-spacer"/>
        <div className={`local-card ${lanConnected ? "is-connected" : ""}`}>
          <span className={`status-dot ${lanConnected ? "is-connected" : ""}`}/>
          <div>
            <strong>{copy.lanCardTitle(mode === "watch", lanConnected)}</strong>
            <small>{copy.lanCardDetail(mode === "watch", lanConnected, joinMode)}</small>
          </div>
        </div>
        <div className="sidebar-footer">
          <div className="lang-switch" role="group" aria-label={copy.language}>
            <button type="button" className={locale === "en" ? "selected" : ""} onClick={() => setLocalePersist("en")}>EN</button>
            <button type="button" className={locale === "pt-BR" ? "selected" : ""} onClick={() => setLocalePersist("pt-BR")}>PT</button>
          </div>
          <button className="logs-button" onClick={() => void openLogs()} title="View the last 5 session logs"><Icon name="terminal" size={13}/> View logs</button>
          <div className="version">goDrinking <span>v{APP_VERSION}</span></div>
        </div>
      </aside>
      <main className="main-content">
        <header className="topbar">
          <div className="breadcrumb"><span>Workspace</span><Icon name="chevron" size={13}/><strong>{mode === "share" ? "New session" : watchConnected ? "Watching" : "Join session"}</strong></div>
          <div className="top-actions"><span className="secure"><span className="secure-dot"/> {copy.topSecure(joinMode)}</span></div>
        </header>
        <div className="page-heading">
          <div>
            <div className="eyebrow">{mode === "share" ? copy.shareEyebrow : copy.watchEyebrow} <span>•</span> {joinMode === "stunar" ? "Stunar" : joinMode === "direct" ? "Direct" : "LAN"}</div>
            <h1>{mode === "share" ? <>{copy.shareTitle}<br/><em>{copy.shareTitleEm}</em></> : watchConnected ? <>Watching <em>{watchLabel}</em></> : <>{copy.watchTitle}<br/><em>{copy.watchTitleEm}</em></>}</h1>
          </div>
        </div>
        {onStage && (
          <RoomStage
            tiles={roomTiles}
            people={roomPeople}
            selfId={session?.self_id}
            watching={watching}
            pinned={pinned}
            sharing={Boolean(session?.native_capture_active)}
            shareBusy={sessionAction !== "idle"}
            canShare={canStart}
            roomLabel={session?.session_code || watchLabel}
            onWatch={(id) => void watchMember(id)}
            onUnwatch={(id) => void unwatchMember(id)}
            onPin={setPinned}
            onShare={() => void startRoomShare()}
            onStopShare={() => void stopRoomShare()}
            onSettings={() => setRoomDesk(true)}
            onLeave={leaveSala}
            localCanvas={canvasRef}
            copy={copy}
          />
        )}
        {(watchStreamActive || mode === "watch") && !onStage ? (
          <div className={`${watchConnected ? "watch-live-grid" : "workspace-grid"}${mode === "share" ? " watch-background" : ""}`} aria-hidden={mode === "share" || undefined}>
            {(!watchConnected || (inSala && roomDesk)) && (
              <section className="panel controls-panel">
                <div className="panel-title"><div><span className="section-kicker">{copy.joinKicker}</span><h2>{joinMode === "direct" ? copy.joinHeadingDirect : copy.joinHeadingCode}</h2></div></div>
                <div className="join-mode-block">
                  <span>{copy.joinMode}</span>
                  <div className="segmented">
                    <button className={joinMode === "lan" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("lan")}>LAN</button>
                    <button className={joinMode === "direct" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("direct")}>Direct</button>
                    <button className={joinMode === "stunar" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("stunar")}>Stunar</button>
                  </div>
                  <p className="quality-hint">{joinModeHelp}</p>
                </div>
                {joinMode === "lan" && (
                  <>
                    <label className="native-select-label" htmlFor="join-code">Session code</label>
                    <input id="join-code" className="native-source" value={joinCode} onChange={(event) => setJoinCode(event.target.value.toUpperCase())} placeholder="ABC123" maxLength={8}/>
                  </>
                )}
                {joinMode === "direct" && (
                  <>
                    <label className="native-select-label" htmlFor="join-host">Host (IP:port)</label>
                    <input id="join-host" className="native-source" value={directHost} onChange={(event) => setDirectHost(event.target.value)} placeholder="192.168.1.40:41234 or [2001:db8::1]:41234"/>
                    <p className="quality-hint">Paste the full address the host showed you, including the port. Wrap IPv6 in [ ].</p>
                  </>
                )}
                {joinMode === "stunar" && (
                  <>
                    <label className="native-select-label" htmlFor="join-rendezvous">Stunar URL</label>
                    <input id="join-rendezvous" className="native-source" value={rendezvousUrl} onChange={(event) => setRendezvousUrl(event.target.value)} placeholder="https://rendezvous.example.com"/>
                    <label className="native-select-label" htmlFor="join-code">Session code</label>
                    <input id="join-code" className="native-source" value={joinCode} onChange={(event) => setJoinCode(event.target.value.toUpperCase())} placeholder="ABC123" maxLength={8}/>
                  </>
                )}
                <label className="native-select-label" htmlFor="join-nickname">Nickname</label>
                <input id="join-nickname" className="native-source" value={nickname} onChange={(event) => setNickname(event.target.value)} placeholder="Your name" maxLength={24}/>
                <label className="native-select-label" htmlFor="join-password">Password {joinMode === "stunar" ? "(required)" : "(optional)"}</label>
                <input id="join-password" className="native-source" type="password" value={joinPassword} onChange={(event) => setJoinPassword(event.target.value)} placeholder={joinMode === "stunar" ? "Required" : "Only if the host set one"} maxLength={64}/>
                <p className="start-hint">{notice}</p>
                {inSala && roomDesk ? (
                  <>
                    <button className="primary-cta" onClick={() => setRoomDesk(false)}>{copy.backToRoom}</button>
                    <button className="copy-button" style={{marginTop: 10, width: "100%"}} disabled={sessionAction !== "idle"} onClick={() => void (session?.native_capture_active ? stopRoomShare() : startRoomShare())}>{session?.native_capture_active ? copy.stopShareMine : copy.shareMine}</button>
                    <button className="watch-disconnect" style={{marginTop: 10, width: "100%"}} onClick={() => leaveSala()}>{copy.leaveRoom}</button>
                  </>
                ) : (
                  <button className="primary-cta" disabled={sessionAction !== "idle" || !nicknameValid || !stunarJoinPasswordValid} onClick={() => void joinRoom()}>
                    {sessionAction === "joining" ? copy.joining : copy.joinSession}
                  </button>
                )}
              </section>
            )}
            {!inSala && <section className={`panel preview-panel ${watchConnected ? "watch-preview" : ""}`}>
              <div className="panel-title">
                <div><span className="section-kicker">{copy.incomingKicker}</span><h2>{copy.incomingHeading}</h2></div>
                <span className="panel-title-actions">
                  <button className="status-button" onClick={() => setStatsOpen(true)} title="Session status: bitrate, resolution, fps, delay"><Icon name="activity" size={13}/> Status</button>
                  <span className={`live-badge ${watchConnected ? "is-live" : ""}`}><i/>{watchConnected ? " Live" : watchIce === "connecting" ? " Waiting" : " Standby"}</span>
                </span>
              </div>
              <div className={`preview-screen ${watchConnected ? "watch-stage" : ""}`} style={watchConnected ? ({ "--watch-zoom": watchZoom } as CSSProperties) : undefined}>
                <video ref={remoteRef} className="remote-preview visible" autoPlay playsInline data-slot="host" style={stageId !== "host" ? { opacity: 0, pointerEvents: "none" } : undefined} />
                {[...remotesRef.current.entries()].filter(([id]) => id !== "host").map(([id, slot]) => (
                  <video key={id} data-slot={id} className="remote-preview visible" autoPlay playsInline ref={(el) => { if (el && slot.stream && el.srcObject !== slot.stream) el.srcObject = slot.stream; }} style={stageId !== id ? { opacity: 0, pointerEvents: "none" } : undefined} />
                ))}
                {watchConnected && !inSala && (
                  <>
                    <div className="watch-hud">
                      <span className="watch-chip watch-chip-code">{watchLabel}</span>
                      <span className="watch-chip">Connected</span>
                    </div>
                    {escHint && <div className="watch-esc-hint">Press Esc to exit</div>}
                    <div className="watch-controls" role="toolbar" aria-label="Video controls">
                      <button className="watch-ctl" onClick={() => zoomStep(-1)} disabled={watchZoom <= 1} title="Zoom out" aria-label="Zoom out">&minus;</button>
                      <button className="watch-ctl watch-ctl-zoom" onClick={() => setWatchZoom(1)} title="Reset zoom">{Math.round(watchZoom * 100)}%</button>
                      <button className="watch-ctl" onClick={() => zoomStep(1)} disabled={watchZoom >= 3} title="Zoom in" aria-label="Zoom in">+</button>
                      <span className="watch-ctl-sep" aria-hidden="true"/>
                      <button className="watch-ctl" onClick={() => setWatchMuted((muted) => !muted)} title={watchMuted ? "Unmute" : "Mute"} aria-label={watchMuted ? "Unmute" : "Mute"} aria-pressed={watchMuted}>
                        <Icon name={watchMuted ? "volume-off" : "volume"} size={13}/>
                      </button>
                      <input className="watch-volume" type="range" min={0} max={100} step={1} value={watchMuted ? 0 : watchVolume} onChange={(event) => { setWatchVolume(Number(event.target.value)); if (event.target.value !== "0") setWatchMuted(false); }} title="Volume" aria-label="Volume"/>
                      <span className="watch-ctl-sep" aria-hidden="true"/>
                      <button className="watch-ctl" onClick={() => void toggleFullscreen()} title={windowFullscreen || elementFullscreen ? "Exit fullscreen" : "Fullscreen"}>
                        <Icon name={windowFullscreen || elementFullscreen ? "minimize" : "expand"} size={13}/>{windowFullscreen || elementFullscreen ? "Exit fullscreen" : "Fullscreen"}
                      </button>
                      <button className="watch-ctl" onClick={() => setCinema((value) => !value)} title={cinema ? "Exit video only" : "Video only"}>
                        <Icon name={cinema ? "minimize" : "monitor"} size={13}/>{cinema ? "Exit video only" : "Video only"}
                      </button>
                    </div>
                  </>
                )}
              </div>
              {(watchConnected || watchIce === "connecting") && (
                <div className="watch-status-row">
                  {watchConnected && <p className="watch-status-line">{notice}</p>}
                  <button className="watch-disconnect" onClick={disconnectWatch}>Disconnect</button>
                </div>
              )}
            </section>}
          </div>
        ) : null}
        {mode === "share" && !onStage && (
          <div className="workspace-grid">
            <section className="panel controls-panel">
              <div className="panel-title"><div><span className="section-kicker">{copy.sourceKicker}</span><h2>{copy.sourceHeading}</h2></div></div>
              <div className="permission-strip">
                <span className={`permission-dot ${captureReady ? "ready" : ""}`}/>
                <div><strong>{permissionLabel}</strong><small>{caps?.detail ?? "Checking native media capability…"}</small></div>
                {caps !== null && caps.platform !== "windows" && caps.screen_recording_authorization !== "granted" && <button onClick={() => void requestPermission()}>Check access</button>}
              </div>
              <div className="source-grid" role="radiogroup">
                <button className={`source-card ${sourceKind === "display" ? "selected" : ""}`} onClick={() => setSourceKind("display")}><span className="source-icon"><Icon name="monitor" size={20}/></span><span className="source-copy"><strong>{copy.wholeScreen}</strong><small>{copy.wholeScreenHint}</small></span></button>
                <button className={`source-card ${sourceKind === "window" ? "selected" : ""}`} onClick={() => setSourceKind("window")}><span className="source-icon"><Icon name="window" size={20}/></span><span className="source-copy"><strong>{copy.aWindow}</strong><small>{copy.aWindowHint}</small></span></button>
              </div>
              <label className="native-select-label" htmlFor="native-source">{copy.nativeSource}</label>
              <select id="native-source" className="native-source" value={sourceId ?? ""} onChange={(event) => setSourceId(Number(event.target.value))} disabled={!filteredSources.length}>
                <option value="">{filteredSources.length ? copy.chooseSource : copy.noSources}</option>
                {filteredSources.map((item) => <option key={item.id} value={item.id}>{item.title || item.application_name || `Source ${item.id}`}</option>)}
              </select>
              <div className="quality-options single">
                <div>
                  <span>{copy.quality}</span>
                  <div className="segmented">
                    <button className={quality === "low" ? "selected" : ""} onClick={() => applyPreset("low")}>{copy.qualityLow}</button>
                    <button className={quality === "medium" ? "selected" : ""} onClick={() => applyPreset("medium")}>{copy.qualityMedium}</button>
                    <button className={quality === "high" ? "selected" : ""} onClick={() => applyPreset("high")}>{copy.qualityHigh}</button>
                  </div>
                  <p className="quality-hint">{copy.qualityLine[quality]}</p>
                  {!active && <button className="bitrate-auto" onClick={() => void runBenchmark()}>{copy.measurePc}</button>}
                  {benchNote && <p className="quality-hint">{benchNote}</p>}
                  <button className="manual-toggle" aria-expanded={qualityOpen} aria-controls="quality-fields" onClick={() => setQualityOpen((open) => !open)} title={copy.customize}>
                    <Icon name="chevron" size={13}/> {copy.customize}<span>{qualitySummary}</span>
                  </button>
                  {qualityOpen && (
                  <div id="quality-fields">
                  <div className="bitrate-label-row">
                    <span>{copy.resolution}</span>
                    <code>{resolvedResolution}{resolution === "auto" ? " · preset" : ""}</code>
                  </div>
                  <div className="segmented">
                    <button className={resolution === "auto" ? "selected" : ""} onClick={() => setResolution("auto")} disabled={active} title="Follows the preset">Auto</button>
                    <button className={resolution === "480p" ? "selected" : ""} onClick={() => setResolution("480p")} disabled={active} title="854×480 or smaller">&lt;HD</button>
                    <button className={resolution === "720p" ? "selected" : ""} onClick={() => setResolution("720p")} disabled={active} title="1280×720 or smaller">HD</button>
                    <button className={resolution === "1080p" ? "selected" : ""} onClick={() => setResolution("1080p")} disabled={active} title="1920×1080 or smaller">Full HD</button>
                    <button className={resolution === "1440p" ? "selected" : ""} onClick={() => setResolution("1440p")} disabled={active} title="2560×1440 or smaller">2K</button>
                    <button className={resolution === "2160p" ? "selected" : ""} onClick={() => setResolution("2160p")} disabled={active} title="3840×2160 or smaller">4K</button>
                  </div>
                  <div className="bitrate-label-row">
                    <span>{copy.frameRate}</span>
                    <code>{resolvedFrameRate.replace("_fps", "fps")}{frameFps === "auto" ? " · preset" : ""}</code>
                  </div>
                  <div className="segmented">
                    <button className={frameFps === "auto" ? "selected" : ""} onClick={() => setFrameFps("auto")} disabled={active} title="Follows the preset">Auto</button>
                    <button className={frameFps === "30_fps" ? "selected" : ""} onClick={() => setFrameFps("30_fps")} disabled={active}>30</button>
                    <button className={frameFps === "60_fps" ? "selected" : ""} onClick={() => setFrameFps("60_fps")} disabled={active}>60</button>
                    <button className={frameFps === "120_fps" ? "selected" : ""} onClick={() => setFrameFps("120_fps")} disabled={active} title="Needs a fast source and a fat pipe">120</button>
                  </div>
                  <p className="quality-hint">{copy.resHint}</p>
                  <div className="bitrate-label-row">
                    <span>{copy.codec}</span>
                    <code>{codecLabel}</code>
                  </div>
                  <div className="segmented">
                    <button className={videoCodec === "h264" ? "selected" : ""} onClick={() => setVideoCodec("h264")} disabled={active} title={copy.codecH264Hint}>{copy.codecH264}</button>
                    <button className={videoCodec === "h264high" ? "selected" : ""} onClick={() => setVideoCodec("h264high")} disabled={active} title={copy.codecHighHint}>{copy.codecH264High}</button>
                    <button className={videoCodec === "hevc" ? "selected" : ""} onClick={() => setVideoCodec("hevc")} disabled={active || !hevcAvailable} title={hevcAvailable ? copy.codecHevcHint : copy.hevcUnavailable}>{copy.codecHevc}</button>
                    <button className={videoCodec === "av1" ? "selected" : ""} onClick={() => setVideoCodec("av1")} disabled={active || !av1Available} title={av1Available ? copy.codecAv1Hint : copy.codecAv1Unavailable}>{copy.codecAv1}</button>
                  </div>
                  {videoCodec === "h264" && <p className="quality-hint">{copy.codecH264Hint}</p>}
                  {videoCodec === "h264high" && <p className="quality-hint">{copy.codecHighHint}</p>}
                  {videoCodec === "hevc" && <p className="quality-hint">{copy.codecHevcHint}</p>}
                  {videoCodec === "av1" && <p className="quality-hint">{copy.codecAv1Hint}</p>}
                  {caps?.platform === "windows" && <>
                    <div className="bitrate-label-row">
                      <span>{copy.encoder}</span>
                      <code>{videoEncoder === "hardware" ? "Hardware (NVENC/AMF/QSV)" : videoEncoder === "software" ? "Software (OpenH264)" : "Auto"}</code>
                    </div>
                    <div className="segmented">
                      <button className={videoEncoder === "auto" ? "selected" : ""} onClick={() => setVideoEncoder("auto")} disabled={active} title="Hardware if it works, software otherwise">{copy.encoderAuto}</button>
                      <button className={videoEncoder === "hardware" ? "selected" : ""} onClick={() => setVideoEncoder("hardware")} disabled={active}>{copy.encoderHw}</button>
                      <button className={videoEncoder === "software" ? "selected" : ""} onClick={() => setVideoEncoder("software")} disabled={active}>{copy.encoderSw}</button>
                      </div>
                    </>}
                  <div className="bitrate-block">
                    <div className="bitrate-label-row">
                      <span>{copy.bitrate}</span>
                      {bitrateMbps !== null && <button className="bitrate-auto" onClick={() => setBitrateMbps(null)} title="Back to the preset">auto</button>}
                    </div>
                    <div className="bitrate-row">
                      <input type="range" min={BITRATE_MIN_MBPS} max={BITRATE_MAX_MBPS} step={0.5} value={Math.min(BITRATE_MAX_MBPS, Math.max(BITRATE_MIN_MBPS, effectiveMbps))} onChange={(event) => setBitrateMbps(Number(event.target.value))} aria-label="Target bitrate in Mbps"/>
                      <code>{effectiveMbps} Mbps{bitrateMbps === null ? " · preset" : ""}</code>
                    </div>
                    <p className="quality-hint">{copy.bitrateHint}</p>
                  </div>
                  <div className="bitrate-block">
                    <div className="bitrate-label-row">
                      <span>{copy.floor}</span>
                      {minBitrateMbps !== null && <button className="bitrate-auto" onClick={() => setMinBitrateMbps(null)} title="Back to ¼ of the target">auto</button>}
                    </div>
                    <div className="bitrate-row">
                      <input type="range" min={FLOOR_MIN_MBPS} max={FLOOR_MAX_MBPS} step={0.25} value={Math.min(FLOOR_MAX_MBPS, Math.max(FLOOR_MIN_MBPS, effectiveFloorMbps))} onChange={(event) => setMinBitrateMbps(Number(event.target.value))} aria-label="Bitrate floor in Mbps"/>
                      <code>{effectiveFloorMbps} Mbps{minBitrateMbps === null ? " · auto" : ""}</code>
                    </div>
                    <p className="quality-hint">{copy.floorHint}</p>
                  </div>
                  <button className="copy-button" style={{marginTop: 10}} disabled={active || sessionAction !== "idle"} onClick={() => void runBenchmark()}>{copy.measurePc}</button>
                  {benchNote && <p className="quality-hint">{benchNote}</p>}
                  <p className="quality-hint">{copy.measureHint}</p>
                  </div>
                  )}
                </div>
              </div>
              <label className={`unsupported-option ${systemAudioSupported ? "" : "is-disabled"}`}>
                <div><strong>{copy.systemAudio}</strong><small>{systemAudioSupported ? copy.systemAudioOn : copy.systemAudioOff}</small></div>
                <input type="checkbox" checked={systemAudio} onChange={(event) => setSystemAudio(event.target.checked)} disabled={!systemAudioSupported}/>
              </label>
              {systemAudio && systemAudioSupported && !exclusionListAvailable && (
                <p className="exclude-hint">Full device mix — per-app exclusion needs macOS 14.2+ or a Windows build with process loopback.</p>
              )}
              {systemAudio && exclusionListAvailable && (
                <>
                  <p className="native-select-label">{copy.excludeLabel}</p>
                  <div className="exclude-tools">
                    <input className="native-source exclude-search" type="text" value={appSearch} onChange={(event) => setAppSearch(event.target.value)} placeholder="Search apps" aria-label="Search apps"/>
                    <button className={`exclude-filter ${playingOnly ? "on" : ""}`} onClick={() => setPlayingOnly((value) => !value)} aria-pressed={playingOnly} title="Only apps playing sound">Playing only</button>
                  </div>
                  <div className="exclude-list" role="group" aria-label="Running apps">
                    {visibleApps.length === 0 ? (
                      <p className="exclude-empty">{playingOnly ? "No apps playing sound" : "No apps found"}</p>
                    ) : visibleApps.map((app) => {
                      const token = app.bundle_id || app.name;
                      const selected = excludedApps.includes(token);
                      return (
                        <button key={`${app.pid}-${app.bundle_id ?? app.name}`} type="button" className={`exclude-row ${selected ? "selected" : ""}`} aria-pressed={selected} onClick={() => toggleExcludedApp(token)}>
                          <span className="exclude-row-name">{app.name}</span>
                          {app.emitting_audio === true && <span className="exclude-audio" title="Playing sound" aria-label="Playing sound"><i/><i/><i/></span>}
                        </button>
                      );
                    })}
                  </div>
                  {excludedApps.length > 0 && (
                    <div className="exclude-chips">
                      {excludedApps.map((token) => {
                        const name = apps.find((app) => (app.bundle_id || app.name) === token)?.name ?? token;
                        return (
                          <span className="exclude-chip" key={token}>
                            {name}
                            <button className="exclude-chip-x" aria-label={`Remove ${name}`} title={`Remove ${name}`} onClick={() => setExcludedApps((current) => current.filter((item) => item !== token))}>&times;</button>
                          </span>
                        );
                      })}
                    </div>
                  )}
                  <p className="exclude-hint">{copy.excludeHint}{windowsExclusion && !audioExclusion ? copy.excludeHintWin : ""}</p>
                </>
              )}
              <label className="native-select-label" htmlFor="share-nickname">{copy.nickname}</label>
              <input id="share-nickname" className="native-source" value={nickname} onChange={(event) => setNickname(event.target.value)} placeholder="Your name" maxLength={24}/>
              <div className="join-mode-block">
                <span>{copy.joinMode}</span>
                <div className="segmented">
                  <button className={joinMode === "lan" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("lan")}>LAN</button>
                  <button className={joinMode === "direct" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("direct")}>Direct</button>
                  <button className={joinMode === "stunar" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("stunar")}>Stunar</button>
                </div>
                <p className="quality-hint">{joinModeHelp}</p>
              </div>
              <div className="join-mode-block">
                <span>{copy.sessionMode}</span>
                <div className="segmented">
                  <button className={sessionMode === "broadcast" ? "selected" : ""} disabled={active} onClick={() => setSessionMode("broadcast")}>{copy.broadcast}</button>
                  <button className={sessionMode === "room" ? "selected" : ""} disabled={active} onClick={() => setSessionMode("room")}>{copy.room}</button>
                </div>
                <p className="quality-hint">{sessionMode === "room" ? copy.roomHint : copy.broadcastHint}</p>
              </div>
              {joinMode === "stunar" && (
                <>
                  <label className="native-select-label" htmlFor="share-rendezvous">Stunar URL</label>
                  <input id="share-rendezvous" className="native-source" value={rendezvousUrl} onChange={(event) => setRendezvousUrl(event.target.value)} placeholder="https://rendezvous.example.com"/>
                  <label className="native-select-label" htmlFor="share-password">Password (required)</label>
                  <input id="share-password" className="native-source" type="password" value={hostPassword} onChange={(event) => setHostPassword(event.target.value)} placeholder="4–64 characters" maxLength={64}/>
                </>
              )}
              <p className="start-hint">{notice}</p>
              {inSala && roomDesk && (
                <button className="primary-cta" style={{marginBottom: 8}} onClick={() => setRoomDesk(false)}>{copy.backToRoom}</button>
              )}
              {active ? (
                <>
                  {inSala && (
                    <button className="copy-button" style={{marginTop: 12, width: "100%"}} disabled={sessionAction !== "idle"} onClick={() => void (session?.native_capture_active ? stopRoomShare() : startRoomShare())}>
                      {session?.native_capture_active ? copy.stopShareMine : copy.shareMine}
                    </button>
                  )}
                  <button className="primary-cta" onClick={() => { if (inSala) leaveSala(); else void stopSharing(); }}>{sessionAction === "stopping" ? copy.stopping : inSala ? copy.closeRoom : copy.stop}</button>
                </>
              ) : (
                <button className="primary-cta" disabled={(sessionMode === "room" ? !canOpenRoom : (!canStart || !stunarHostPasswordValid)) || !nicknameValid || sessionAction !== "idle"} onClick={() => void startSharing()}>{sessionAction === "starting" ? copy.starting : sessionMode === "room" ? copy.openRoom : copy.start}</button>
              )}
            </section>
            <div className="right-column">
              <section className="panel preview-panel">
                <div className="panel-title"><div><span className="section-kicker">{copy.previewKicker}</span><h2>{copy.previewHeading}</h2></div><span className="panel-title-actions"><button className="status-button" onClick={() => setStatsOpen(true)} title="Session status: bitrate, resolution, fps, delay"><Icon name="activity" size={13}/> Status</button><span className="live-badge"><i/>{connected ? " Live" : active ? " Capturing" : " Standby"}</span></span></div>
                <div className="preview-screen">
                  <div className="preview-grid"/>
                  <canvas ref={canvasRef} className={`native-preview ${active && !onStage ? "visible" : ""}`} aria-label="Native capture preview"/>
                  <div className={`preview-center ${active ? "is-hidden" : ""}`}><strong>{copy.previewReady}</strong><small>{captureReady ? copy.previewStart : caps?.platform === "windows" ? copy.previewWinDown : copy.previewNeedPerm}</small></div>
                </div>
              </section>
              <section className="panel connect-panel">
                <div className="panel-title"><div><span className="section-kicker">{copy.connectKicker}</span><h2>{joinMode === "direct" ? copy.shareAddress : copy.passCode}</h2></div><Icon name="wifi" size={19}/></div>
                {joinMode === "lan" && (
                  <div className="signal-block">
                    <label>Session code</label>
                    <input className="signal-input" readOnly value={roomLabel} placeholder="Start a session to create a code"/>
                    <button className="copy-button" onClick={() => void copyCode()} disabled={!session?.session_code}><Icon name="copy" size={15}/>{copied ? "Copied" : "Copy"}</button>
                  </div>
                )}
                {joinMode === "direct" && (
                  <div className="direct-address-list">
                    <label>Addresses</label>
                    {!active && <p className="roster-empty">Start a session to create addresses.</p>}
                    {active && directAddresses.length === 0 && <p className="roster-empty">Collecting addresses…</p>}
                    {directAddresses.map((entry) => (
                      <div className="direct-address-row" key={`${entry.kind}-${entry.ip}`}>
                        <span className="direct-address-kind">{entry.kind === "lan" ? "LAN" : entry.kind === "public" ? "Public" : "IPv6"}</span>
                        <code className="direct-address-copy">{entry.copy}</code>
                        <button className="copy-button" onClick={() => void copyText(entry.copy)}><Icon name="copy" size={15}/>{copied ? "Copied" : "Copy"}</button>
                      </div>
                    ))}
                    {!directAddresses.some((entry) => entry.kind === "public") && active && (
                      <p className="signaling-status">No public IPv4. Direct over the internet may fail.</p>
                    )}
                    {session?.direct_mapping === false && directAddresses.some((entry) => entry.kind === "public") && (
                      <p className="signaling-status">Port mapping failed. Viewers on other networks need this port open.</p>
                    )}
                  </div>
                )}
                {joinMode === "stunar" && (
                  <>
                    <div className="signal-block">
                      <label>Session code</label>
                      <input className="signal-input" readOnly value={session?.session_code ?? ""} placeholder="Start a session to create a code"/>
                      <button className="copy-button" onClick={() => void copyCode()} disabled={!session?.session_code}><Icon name="copy" size={15}/>{copied ? "Copied" : "Copy"}</button>
                    </div>
                    <span className={`stunar-chip ${session?.stunar_state === "live" ? "is-live" : session?.stunar_state === "unreachable" ? "is-down" : ""}`}>
                      <i/>{session?.stunar_state === "live" ? "Live" : session?.stunar_state === "unreachable" ? "Relay unreachable" : "Calling…"}
                    </span>
                  </>
                )}
                {active && (
                  <>
                    <div className="signal-block">
                      <label>Password {joinMode === "stunar" ? "(required)" : "(optional)"}</label>
                      <input className="signal-input" type="password" value={hostPassword} onChange={(event) => setHostPassword(event.target.value)} placeholder={session?.password_set ? "Change password" : "Set a password"} maxLength={64}/>
                      <button className="copy-button" onClick={() => void applyCredentials()} disabled={sessionAction !== "idle" || (joinMode === "stunar" && !passwordValid(hostPassword))}>{session?.password_set ? "Update" : "Set password"}</button>
                    </div>
                    <label className={`unsupported-option ${active ? "" : "is-disabled"}`}>
                      <div><strong>Require approval</strong><small>Approve each viewer before they see your screen</small></div>
                      <input type="checkbox" checked={hostAdmission} onChange={(event) => void toggleAdmission(event.target.checked)}/>
                    </label>
                    <div className="roster-block">
                      <label>People</label>
                      {pendingRoster.length > 0 && (
                        <div className="roster-group">
                          {pendingRoster.map((entry) => (
                            <div className="roster-row" key={`pending-${entry.id}`}>
                              <span className="roster-name">{entry.nickname}<small>Waiting for approval</small></span>
                              <span className="roster-actions">
                                <button onClick={() => void admitViewer(entry.id)}>Accept</button>
                                <button onClick={() => void rejectViewer(entry.id)}>Decline</button>
                              </span>
                            </div>
                          ))}
                        </div>
                      )}
                      {connectedRoster.length > 0 && (
                        <div className="roster-group">
                          {connectedRoster.map((entry) => (
                            <div className="roster-row" key={`connected-${entry.id}`}>
                              <span className="roster-name">{entry.master ? <span className="roster-crown" title="Master">♛</span> : null}{entry.nickname}<small>{entry.share ? "Sharing" : entry.state === "connected" ? "Connected" : "Connecting…"}</small></span>
                              <span className="roster-actions">
                                <button onClick={() => void kickViewer(entry.id)}>Disconnect</button>
                              </span>
                            </div>
                          ))}
                        </div>
                      )}
                      {pendingRoster.length === 0 && connectedRoster.length === 0 && (
                        <p className="roster-empty">{joinMode === "direct" ? "No one here yet. Share the address." : "No one here yet. Share the code."}</p>
                      )}
                    </div>
                  </>
                )}
                <p className="signaling-status">{session?.peer_detail || (joinMode === "direct" ? "The other person opens Watch, picks Direct, and enters this address." : joinMode === "stunar" ? "The other person opens Watch, picks Stunar, and enters this code." : "The other person opens Watch and enters this code on the same network.")}</p>
              </section>
            </div>
          </div>
        )}
      </main>
      {statsOpen && (
        <div className="logs-overlay" onClick={() => setStatsOpen(false)}>
          <div className="logs-modal stats-modal" role="dialog" aria-modal="true" aria-label="Session status" onClick={(event) => event.stopPropagation()}>
            <div className="logs-header">
              <div>
                <span className="section-kicker">Diagnostics · live · 1s</span>
                <h2>{mode === "share" ? "Session status · Host" : "Session status · Viewer"}</h2>
              </div>
              <button className="logs-close" onClick={() => setStatsOpen(false)} aria-label="Close status" title="Close">&times;</button>
            </div>
            {mode === "share" ? (
              <>
                <div className="stats-grid">
                  <div><span>Encoder target</span><code>{session?.bitrate_bps ? `${Math.round(session.bitrate_bps / 1e5) / 10} Mbps applied` : `${qualityTargetMbps[quality]} Mbps (preset)`} · {(session?.resolution ?? resolvedResolution)} · {(session?.frame_rate ?? resolvedFrameRate).replace("_fps", "fps")}{bitrateMbps !== null ? " · custom" : ""}</code></div>
                  <div><span>Session / peer</span><code>{session?.state ?? "idle"} / {session?.peer_state ?? "—"}</code></div>
                  {session?.detail ? <div><span>Session detail</span><code className={session?.state === "failed" ? "is-error" : ""} title={session.detail}>{session.detail}</code></div> : null}
                  <div><span>Viewers connected</span><code>{connectedRoster.length}{pendingRoster.length > 0 ? " (+" + pendingRoster.length + " waiting)" : ""}</code></div>
                  <div><span>Join mode</span><code>{joinMode}{session?.session_code ? " · " + session.session_code : ""}</code></div>
                  <div><span>Capture active</span><code>{session?.native_capture_active ? "yes" : active ? "starting…" : "no"}</code></div>
                  <div><span>Capture preview</span><code>{hostPreviewFps !== null ? hostPreviewFps + " fps" : "—"}{session ? " · " + session.preview_frame_count + " frames" : ""}{session && session.preview_dropped_count > 0 ? " · " + session.preview_dropped_count + " dropped" : ""}</code></div>
                  <div><span>Peer detail</span><code title={session?.peer_detail ?? ""}>{session?.peer_detail || "—"}</code></div>
                  <div><span>Preview error</span><code className={session?.preview_error ? "is-error" : ""}>{session?.preview_error || "—"}</code></div>
                  <div><span>Effective (adapted)</span><code className={congestionMbps !== null && session?.bitrate_bps ? (congestionMbps < session.bitrate_bps / 1e6 / 2 ? "is-warn" : "") : ""}>{congestionMbps !== null ? `${congestionMbps} Mbps (REMB)` : "no signal, using the target"}{floorAppliedMbps !== null ? ` · floor ${floorAppliedMbps}` : ""}</code></div>
                </div>
                <div className="stats-links">
                  <span className="stats-links-title">Viewers · link RTT</span>
                  {!hostLinks || hostLinks.length === 0 ? (
                    <code className="stats-links-empty">{active ? "No viewer with an active transport yet. RTT shows up once someone connects." : "Start a session to measure."}</code>
                  ) : hostLinks.map((link) => (
                    <div className="stats-link-row" key={link.id}>
                      <span className="stats-link-name">{link.nickname}<small>{link.state}</small></span>
                      <code className={link.rtt_ms !== null && link.rtt_ms > 150 ? "is-warn" : ""}>{link.rtt_ms !== null ? `${link.rtt_ms} ms` : "measuring…"}</code>
                    </div>
                  ))}
                </div>
                <p className="stats-hint">{copy.statsHostHint}</p>
              </>
            ) : (
              <>
                <div className="stats-grid">
                  <div><span>Received bitrate</span><code className={viewerStats?.bitrateMbps !== null && viewerStats?.bitrateMbps !== undefined && viewerStats.bitrateMbps < 1 ? "is-warn" : ""}>{viewerStats?.bitrateMbps !== null && viewerStats?.bitrateMbps !== undefined ? viewerStats.bitrateMbps + " Mbps" : "—"}</code></div>
                  <div><span>Resolution</span><code>{viewerStats?.resolution ?? "—"}</code></div>
                  <div><span>FPS</span><code>{viewerStats?.fps !== null && viewerStats?.fps !== undefined ? String(viewerStats.fps) : "—"}</code></div>
                  <div><span>Codec</span><code>{viewerStats?.codec ?? "—"}</code></div>
                  <div><span>RTT</span><code>{viewerStats?.rttMs !== null && viewerStats?.rttMs !== undefined ? viewerStats.rttMs + " ms" : "—"}</code></div>
                  <div><span>Jitter</span><code>{viewerStats?.jitterMs !== null && viewerStats?.jitterMs !== undefined ? viewerStats.jitterMs + " ms" : "—"}</code></div>
                  <div><span>Packets (loss)</span><code>{viewerStats?.packetsLost !== null && viewerStats?.packetsLost !== undefined ? viewerStats.packetsLost + " lost" + (viewerStats.lossPercent !== null && viewerStats.lossPercent !== undefined ? " · " + viewerStats.lossPercent + "%" : "") : "—"}</code></div>
                  <div><span>Frames (drop)</span><code>{viewerStats?.framesDecoded !== null && viewerStats?.framesDecoded !== undefined ? viewerStats.framesDecoded + " dec." + (viewerStats.dropPercent !== null && viewerStats.dropPercent !== undefined ? " · " + viewerStats.dropPercent + "% drop" : "") : "—"}</code></div>
                  <div><span>Player delay</span><code>{viewerStats?.liveDelaySec !== null && viewerStats?.liveDelaySec !== undefined ? viewerStats.liveDelaySec + " s" : "—"}</code></div>
                  <div><span>Connection / ICE</span><code>{viewerStats?.connectionState ?? watchIce}{viewerStats?.iceState ? " / " + viewerStats.iceState : ""}</code></div>
                </div>
                {!watchConnected && <p className="stats-hint">No media yet. Numbers show up after you connect.</p>}
                {watchConnected && (viewerStats?.bitrateMbps === null || viewerStats?.bitrateMbps === undefined) && <p className="stats-hint">Sampling… give it a second or two.</p>}
                <p className="stats-hint">{copy.statsViewerHint(quality, qualityTargetMbps[quality])}</p>
              </>
            )}
          </div>
        </div>
      )}
      {logsOpen && (
        <div className="logs-overlay" onClick={closeLogs}>
          <div className="logs-modal" role="dialog" aria-modal="true" aria-label="Session logs" onClick={(event) => event.stopPropagation()}>
            <div className="logs-header">
              <div>
                <span className="section-kicker">Diagnostics</span>
                <h2>Session logs</h2>
              </div>
              <button className="logs-close" onClick={closeLogs} aria-label="Close logs" title="Close">&times;</button>
            </div>
            <div className="logs-diagnostics">
              <div><span>Join mode</span><code>{joinMode}</code></div>
              <div><span>Rendezvous URL</span><code title={rendezvousUrl}>{rendezvousUrl || "—"}</code></div>
              <div><span>Last error</span><code className={lastError ? "is-error" : ""} title={lastError}>{lastError || "—"}</code></div>
            </div>
            {logs.length === 0 ? (
              <p className="logs-empty">No session logs yet. Start a session or try to join one, then come back here.</p>
            ) : (
              <>
                <div className="logs-tabs" role="tablist" aria-label="Session logs">
                  {logs.map((log, index) => (
                    <button key={log.session} role="tab" aria-selected={activeLog === index} className={activeLog === index ? "selected" : ""} title={`${log.session} · ${log.timestamp}`} onClick={() => setActiveLog(index)}>
                      {logLabel(log.session)}
                    </button>
                  ))}
                </div>
                <div className="logs-actions">
                  <button onClick={() => void copyActiveLog()} disabled={!logs[activeLog]}><Icon name="copy" size={12}/> Copy</button>
                  <button onClick={() => void copyAllLogs()} disabled={logs.length === 0}><Icon name="copy" size={12}/> Copy all</button>
                  <button onClick={() => void openLogsFolder()}><Icon name="folder" size={12}/> Folder</button>
                  <button onClick={() => void loadLogs()}>Refresh</button>
                  <button className="is-danger" onClick={() => void clearLogs()}>Clear all</button>
                </div>
                <pre className="logs-content">{logs[activeLog]?.lines.join("\n")}</pre>
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
