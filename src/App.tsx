import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import "./index.css";
import "./App.css";
import logo from "./assets/logo.png";

type IconName = "grid" | "monitor" | "window" | "game" | "settings" | "help" | "plus" | "copy" | "wifi" | "chevron" | "expand" | "minimize" | "volume" | "volume-off";
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
    "volume-off": <><path d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/></>
  };
  return <svg className="icon" width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{paths[name]}</svg>;
}

type Source = { id: number; kind: "display" | "window"; title?: string; application_name?: string };
type RunningApp = { name: string; bundle_id?: string | null; pid: number; emitting_audio?: boolean };
type Capabilities = { supported: boolean; screen_capture_kit: boolean; source_enumeration_available: boolean; screen_recording_authorization: "granted" | "not_granted" | "unsupported"; app_audio_exclusion: "enhanced" | "best_effort" | "unsupported"; detail: string };
type RosterEntry = { id: string; nickname: string; state: string };
type DirectAddress = { ip: string; port: number; version: number; kind: string; copy: string };
type Snapshot = { state: string; session_id: string | null; source_id: number | null; native_capture_active: boolean; preview_callback_count: number; preview_frame_count: number; preview_dropped_count: number; preview_error: string | null; detail: string; peer_state: string; peer_detail: string; session_code: string | null; lan_addresses: string[]; lan_port: number | null; roster?: RosterEntry[]; password_set?: boolean; admission?: boolean; join_mode?: string; direct_listen_port?: number | null; direct_addresses?: DirectAddress[]; direct_mapping?: boolean; stunar_state?: string | null };
type Signal = { type: "offer" | "answer"; sdp: string; id?: string };

const invokeMedia = <T,>(command: string, args?: Record<string, unknown>) => invoke<T>(command, args);
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
  const [mode, setMode] = useState<"share" | "watch">("share");
  const [caps, setCaps] = useState<Capabilities | null>(null);
  const [sources, setSources] = useState<Source[]>([]);
  const [apps, setApps] = useState<RunningApp[]>([]);
  const [sourceKind, setSourceKind] = useState<"display" | "window">("display");
  const [sourceId, setSourceId] = useState<number | null>(null);
  const [quality, setQuality] = useState<"low" | "medium" | "high">("high");
  const [systemAudio, setSystemAudio] = useState(false);
  const [excludedApps, setExcludedApps] = useState<string[]>([]);
  const [appSearch, setAppSearch] = useState("");
  const [playingOnly, setPlayingOnly] = useState(false);
  const [session, setSession] = useState<Snapshot | null>(null);
  const [joinCode, setJoinCode] = useState("");
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
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const remoteRef = useRef<HTMLVideoElement>(null);
  const peerRef = useRef<RTCPeerConnection | null>(null);
  const remoteStreamRef = useRef<MediaStream | null>(null);
  const liveSettingsApplied = useRef(false);
  const joinSeqRef = useRef(0);
  const active = session?.state === "running" || Boolean(session?.native_capture_active);
  const connected = session?.peer_state === "connected";
  const watchConnected = watchIce === "connected";
  const watchStreamActive = watchConnected || watchIce === "connecting";
  const watchLabel = joinMode === "direct" ? (watchHostName || "via Direct") : watchCode;
  const lanConnected = mode === "watch" ? watchConnected : connected;
  const canStart = Boolean(caps?.supported && caps.screen_recording_authorization === "granted");
  const audioExclusion = caps?.app_audio_exclusion === "enhanced";
  const roomLabel = session?.session_code ? `${session.session_code}${session.lan_addresses[0] ? ` · ${session.lan_addresses[0]}` : ""}` : "";
  const nicknameValid = /^[A-Za-z0-9 _.-]+$/.test(nickname.trim()) && nickname.trim().length >= 2 && nickname.trim().length <= 24;
  const passwordValid = (password: string) => password.length >= 4 && password.length <= 64;
  const stunarHostPasswordValid = joinMode !== "stunar" || passwordValid(hostPassword);
  const stunarJoinPasswordValid = joinMode !== "stunar" || passwordValid(joinPassword);
  const roster = session?.roster ?? [];
  const pendingRoster = roster.filter((entry) => entry.state === "pending");
  const connectedRoster = roster.filter((entry) => entry.state !== "pending");
  const directAddresses = session?.direct_addresses ?? [];
  const joinModeHelp = {
    lan: mode === "share" ? "Same network. They type your code." : "Code from the host on your network.",
    direct: mode === "share" ? "They type your IP and port." : "IP and port the host sent you.",
    stunar: mode === "share" ? "Internet. They type your code." : "Code from the host. Needs the relay."
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
    if (mode !== "share" || !systemAudio || !audioExclusion) return undefined;
    void loadApps();
    const timer = window.setInterval(() => void loadApps(), 1000);
    return () => window.clearInterval(timer);
  }, [mode, systemAudio, audioExclusion]);
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
    if (!active) return undefined;
    const poll = async () => {
      try {
        const next = await invokeMedia<Snapshot>("get_media_session_state");
        setSession(next);
        if (next.preview_error) setNotice(`Native preview: ${next.preview_error}`);
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
  }, [active]);
  useEffect(() => {
    if (!active) { liveSettingsApplied.current = false; return; }
    if (sessionAction !== "idle") return;
    if (!liveSettingsApplied.current) { liveSettingsApplied.current = true; return; }
    void invokeMedia<Snapshot | null>("update_media_session", { request: { quality, system_audio: systemAudio, excluded_apps: excludedApps } })
      .then((next) => { if (next) setSession(next); })
      .catch((error) => setNotice(`Could not apply the change: ${diagnosticError(error, "unknown error")}`));
  }, [quality, systemAudio, excludedApps, active, sessionAction]);
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
  const startSharing = async () => {
    if (sessionAction !== "idle") return;
    setSessionAction("starting");
    setNotice("Starting native capture…");
    try {
      const current = await refreshCapabilities();
      if (!current?.supported || current.screen_recording_authorization !== "granted") throw new Error("Grant Screen Recording access first.");
      if (sourceId === null && filteredSources.length > 0) throw new Error("Choose a display or window before starting.");
      if (!nicknameValid) throw new Error("Enter a nickname (2–24 letters, numbers, spaces, _ - .).");
      if (joinMode === "stunar" && !rendezvousUrl.trim()) throw new Error("Set the Stunar URL in settings.");
      if (joinMode === "stunar" && !passwordValid(hostPassword)) throw new Error("Stunar requires a password (4–64 characters).");
      const preset = qualityPresets[quality];
      const next = await invokeMedia<Snapshot>("create_media_session", {
        request: {
          source: sourceKind === "display" ? "screen" : "window",
          source_id: sourceId,
          quality,
          resolution: preset.resolution,
          frame_rate: preset.frame_rate,
          system_audio: systemAudio,
          excluded_apps: excludedApps,
          password: hostPassword,
          nickname: nickname.trim() || "Host",
          admission: hostAdmission,
          join_mode: joinMode,
          rendezvous_url: joinMode === "stunar" ? rendezvousUrl.trim() : null
        }
      });
      setSession(next);
      setNotice(next.session_code ? `Session ${next.session_code} is live. Share that code.` : next.detail);
    } catch (error) {
      const recovered = await invokeMedia<Snapshot>("get_media_session_state").catch(() => null);
      setSession(recovered && recovered.state !== "idle" ? recovered : null);
      setNotice(diagnosticError(error, "Native capture could not start."));
    } finally {
      setSessionAction("idle");
    }
  };
  const stopSharing = async () => {
    if (sessionAction !== "idle") return;
    setSessionAction("stopping");
    try {
      peerRef.current?.close();
      peerRef.current = null;
      await invokeMedia("close_media_peer_transport").catch(() => undefined);
      await invokeMedia("stop_media_session");
      setSession(null);
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
    joinSeqRef.current += 1;
    peerRef.current?.close();
    peerRef.current = null;
    remoteStreamRef.current = null;
    if (remoteRef.current) remoteRef.current.srcObject = null;
    void invokeMedia("stunar_viewer_close").catch(() => undefined);
    setWatchIce("idle");
    setWatchCode("");
    setWatchHostName("");
    setWatchZoom(1);
    leaveImmersive();
    setSessionAction("idle");
    setNotice("Disconnected.");
  };
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
      const pc = new RTCPeerConnection({ iceServers: joinMode === "lan" ? [] : [{ urls: ["stun:stun.l.google.com:19302"] }] });
      peerRef.current?.close();
      peerRef.current = pc;
      setWatchIce("connecting");
      setWatchCode(targetLabel);
      setWatchHostName(hostName.trim() || "");
      pc.ontrack = (event) => {
        const stream = event.streams[0] ?? new MediaStream(event.track ? [event.track] : []);
        remoteStreamRef.current = stream;
        if (remoteRef.current && remoteRef.current.srcObject !== stream) remoteRef.current.srcObject = stream;
      };
      await pc.setRemoteDescription({ type: "offer", sdp: offer.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      await waitIce(pc);
      const local = pc.localDescription;
      if (!local?.sdp) throw new Error("The viewer could not create an answer.");
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
  const permissionLabel = caps === null ? "Checking Screen Recording access…" : caps.screen_recording_authorization === "granted" ? "Screen Recording ready" : caps.screen_recording_authorization === "not_granted" ? "Screen Recording permission needed" : "Native capture unavailable";

  return (
    <div className={`app-shell${cinema ? " cinema" : ""}`}>
      <aside className="sidebar">
        <div className="brand">
          <img className="brand-logo" src={logo} alt=""/>
          <div className="brand-stack">
            <span>goDrinking</span>
            <span className="brand-tagline" title={tagline}>{tagline}</span>
          </div>
        </div>
        <div className="nav-label">Workspace</div>
        <nav aria-label="Main navigation">
          <button className={`nav-item ${mode === "share" ? "active" : ""}`} onClick={() => setMode("share")}><Icon name="grid"/> Share screen</button>
          <button className={`nav-item ${mode === "watch" ? "active" : ""}`} onClick={() => setMode("watch")}><Icon name="monitor"/> Watch{watchStreamActive ? <span className="nav-live"><i/>Live</span> : null}</button>
        </nav>
        <div className="sidebar-spacer"/>
        <div className={`local-card ${lanConnected ? "is-connected" : ""}`}>
          <span className={`status-dot ${lanConnected ? "is-connected" : ""}`}/>
          <div>
            <strong>{mode === "watch" ? (watchConnected ? "Connected" : "Local network") : connected ? "Peer connected" : "Local network"}</strong>
            <small>{mode === "watch" ? (watchConnected ? "Watching the host" : "P2P on your LAN") : connected ? "Sharing is live" : "P2P on your LAN"}</small>
          </div>
        </div>
        <div className="version">goDrinking <span>v0.1.0</span></div>
      </aside>
      <main className="main-content">
        <header className="topbar">
          <div className="breadcrumb"><span>Workspace</span><Icon name="chevron" size={13}/><strong>{mode === "share" ? "New session" : watchConnected ? "Watching" : "Join session"}</strong></div>
          <div className="top-actions"><span className="secure"><span className="secure-dot"/> Local only</span></div>
        </header>
        <div className="page-heading">
          <div>
            <div className="eyebrow">{mode === "share" ? "Host a room" : "Watch a room"} <span>•</span> {mode === "share" ? "Native capture" : "Live stream"}</div>
            <h1>{mode === "share" ? <>Share your screen,<br/><em>stay close.</em></> : watchConnected ? <>Watching <em>{watchLabel}</em></> : <>Enter a code,<br/><em>watch live.</em></>}</h1>
          </div>
        </div>
        {watchStreamActive || mode === "watch" ? (
          <div className={`${watchConnected ? "watch-live-grid" : "workspace-grid"}${mode === "share" ? " watch-background" : ""}`} aria-hidden={mode === "share" || undefined}>
            {!watchConnected && (
              <section className="panel controls-panel">
                <div className="panel-title"><div><span className="section-kicker">01 / Join</span><h2>{joinMode === "direct" ? "Host address" : "Session code"}</h2></div></div>
                <div className="join-mode-block">
                  <span>Join mode</span>
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
                    <p className="quality-hint">Cole o endereço completo que o Host mostrou (com porta, IPv6 entre [ ]). </p>
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
                <button className="primary-cta" disabled={sessionAction !== "idle" || !nicknameValid || !stunarJoinPasswordValid} onClick={() => void joinRoom()}>
                  {sessionAction === "joining" ? "Joining…" : "Join session"}
                </button>
              </section>
            )}
            <section className={`panel preview-panel ${watchConnected ? "watch-preview" : ""}`}>
              <div className="panel-title">
                <div><span className="section-kicker">Preview</span><h2>Incoming stream</h2></div>
                <span className={`live-badge ${watchConnected ? "is-live" : ""}`}><i/>{watchConnected ? " Live" : watchIce === "connecting" ? " Waiting" : " Standby"}</span>
              </div>
              <div className={`preview-screen ${watchConnected ? "watch-stage" : ""}`} style={watchConnected ? ({ "--watch-zoom": watchZoom } as CSSProperties) : undefined}>
                <video ref={remoteRef} className="remote-preview visible" autoPlay playsInline />
                {watchConnected && (
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
            </section>
          </div>
        ) : null}
        {mode === "share" && (
          <div className="workspace-grid">
            <section className="panel controls-panel">
              <div className="panel-title"><div><span className="section-kicker">01 / Source</span><h2>What do you want to share?</h2></div></div>
              <div className="permission-strip">
                <span className={`permission-dot ${caps?.screen_recording_authorization === "granted" ? "ready" : ""}`}/>
                <div><strong>{permissionLabel}</strong><small>{caps?.detail ?? "Checking native media capability…"}</small></div>
                {caps !== null && caps.screen_recording_authorization !== "granted" && <button onClick={() => void requestPermission()}>Check access</button>}
              </div>
              <div className="source-grid" role="radiogroup">
                <button className={`source-card ${sourceKind === "display" ? "selected" : ""}`} onClick={() => setSourceKind("display")}><span className="source-icon"><Icon name="monitor" size={20}/></span><span className="source-copy"><strong>Whole screen</strong><small>Native display capture</small></span></button>
                <button className={`source-card ${sourceKind === "window" ? "selected" : ""}`} onClick={() => setSourceKind("window")}><span className="source-icon"><Icon name="window" size={20}/></span><span className="source-copy"><strong>A window</strong><small>Choose a native app window</small></span></button>
              </div>
              <label className="native-select-label" htmlFor="native-source">Native source</label>
              <select id="native-source" className="native-source" value={sourceId ?? ""} onChange={(event) => setSourceId(Number(event.target.value))} disabled={!filteredSources.length}>
                <option value="">{filteredSources.length ? "Choose a source" : "No sources available"}</option>
                {filteredSources.map((item) => <option key={item.id} value={item.id}>{item.title || item.application_name || `Source ${item.id}`}</option>)}
              </select>
              <div className="quality-options single">
                <div>
                  <span>Quality</span>
                  <div className="segmented">
                    <button className={quality === "low" ? "selected" : ""} onClick={() => setQuality("low")}>Low</button>
                    <button className={quality === "medium" ? "selected" : ""} onClick={() => setQuality("medium")}>Medium</button>
                    <button className={quality === "high" ? "selected" : ""} onClick={() => setQuality("high")}>High</button>
                  </div>
                  <p className="quality-hint">Low 720p 30 · Medium 1080p 30 · High 1080p 60</p>
                </div>
              </div>
              <label className={`unsupported-option ${audioExclusion ? "" : "is-disabled"}`}>
                <div><strong>Share system audio</strong><small>{audioExclusion ? "Viewers hear your system sound" : "Needs macOS 14.2+ process taps"}</small></div>
                <input type="checkbox" checked={systemAudio} onChange={(event) => setSystemAudio(event.target.checked)} disabled={!audioExclusion}/>
              </label>
              {systemAudio && audioExclusion && (
                <>
                  <p className="native-select-label">Don't share audio from</p>
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
                  <p className="exclude-hint">Viewers won't hear the apps you pick here. For example, pick Discord to mute it.</p>
                </>
              )}
              <label className="native-select-label" htmlFor="share-nickname">Nickname</label>
              <input id="share-nickname" className="native-source" value={nickname} onChange={(event) => setNickname(event.target.value)} placeholder="Your name" maxLength={24}/>
              <div className="join-mode-block">
                <span>Join mode</span>
                <div className="segmented">
                  <button className={joinMode === "lan" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("lan")}>LAN</button>
                  <button className={joinMode === "direct" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("direct")}>Direct</button>
                  <button className={joinMode === "stunar" ? "selected" : ""} disabled={active} onClick={() => setJoinMode("stunar")}>Stunar</button>
                </div>
                <p className="quality-hint">{joinModeHelp}</p>
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
              {active ? (
                <button className="primary-cta" disabled={sessionAction !== "idle"} onClick={() => void stopSharing()}>{sessionAction === "stopping" ? "Stopping…" : "Stop native session"}</button>
              ) : (
                <button className="primary-cta" disabled={!canStart || !nicknameValid || sessionAction !== "idle" || !stunarHostPasswordValid} onClick={() => void startSharing()}>{sessionAction === "starting" ? "Starting…" : "Start native session"}</button>
              )}
            </section>
            <div className="right-column">
              <section className="panel preview-panel">
                <div className="panel-title"><div><span className="section-kicker">Preview</span><h2>What your peer will see</h2></div><span className="live-badge"><i/>{connected ? " Live" : active ? " Capturing" : " Standby"}</span></div>
                <div className="preview-screen">
                  <div className="preview-grid"/>
                  <canvas ref={canvasRef} className={`native-preview ${active ? "visible" : ""}`} aria-label="Native capture preview"/>
                  <div className={`preview-center ${active ? "is-hidden" : ""}`}><strong>Preview is ready</strong><small>{caps?.screen_recording_authorization === "granted" ? "Start a native session to begin" : "Grant Screen Recording access first"}</small></div>
                </div>
              </section>
              <section className="panel connect-panel">
                <div className="panel-title"><div><span className="section-kicker">02 / Connect</span><h2>{joinMode === "direct" ? "Share this address" : "Pass this code"}</h2></div><Icon name="wifi" size={19}/></div>
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
                              <span className="roster-name">{entry.nickname}<small>{entry.state === "connected" ? "Connected" : "Connecting…"}</small></span>
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
    </div>
  );
}

export default App;
