import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import "./index.css";
import "./App.css";

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
type RunningApp = { name: string; bundle_id?: string | null; pid: number };
type Capabilities = { supported: boolean; screen_capture_kit: boolean; source_enumeration_available: boolean; screen_recording_authorization: "granted" | "not_granted" | "unsupported"; app_audio_exclusion: "enhanced" | "best_effort" | "unsupported"; detail: string };
type Snapshot = { state: string; session_id: string | null; source_id: number | null; native_capture_active: boolean; preview_callback_count: number; preview_frame_count: number; preview_dropped_count: number; preview_error: string | null; detail: string; peer_state: string; peer_detail: string; session_code: string | null; lan_addresses: string[]; lan_port: number | null };
type Signal = { type: "offer" | "answer"; sdp: string };

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
  const [session, setSession] = useState<Snapshot | null>(null);
  const [joinCode, setJoinCode] = useState("");
  const [notice, setNotice] = useState("");
  const [copied, setCopied] = useState(false);
  const [sessionAction, setSessionAction] = useState<"idle" | "starting" | "stopping" | "joining">("idle");
  const [watchIce, setWatchIce] = useState<"idle" | "connecting" | "connected" | "lost">("idle");
  const [watchCode, setWatchCode] = useState("");
  const [watchZoom, setWatchZoom] = useState(1);
  const [cinema, setCinema] = useState(false);
  const [windowFullscreen, setWindowFullscreen] = useState(false);
  const [elementFullscreen, setElementFullscreen] = useState(false);
  const [watchVolume, setWatchVolume] = useState(80);
  const [watchMuted, setWatchMuted] = useState(false);
  const [escHint, setEscHint] = useState(false);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const remoteRef = useRef<HTMLVideoElement>(null);
  const peerRef = useRef<RTCPeerConnection | null>(null);
  const remoteStreamRef = useRef<MediaStream | null>(null);
  const liveSettingsApplied = useRef(false);
  const active = session?.state === "running" || Boolean(session?.native_capture_active);
  const connected = session?.peer_state === "connected";
  const watchConnected = watchIce === "connected";
  const lanConnected = mode === "watch" ? watchConnected : connected;
  const canStart = Boolean(caps?.supported && caps.screen_recording_authorization === "granted");
  const audioExclusion = caps?.app_audio_exclusion === "enhanced";
  const roomLabel = session?.session_code ? `${session.session_code}${session.lan_addresses[0] ? ` · ${session.lan_addresses[0]}` : ""}` : "";

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
    } catch {
      setApps([]);
    }
  };

  useEffect(() => { void refreshCapabilities(); }, []);
  useEffect(() => {
    if (!caps?.source_enumeration_available) return;
    void loadSources();
    void loadApps();
  }, [caps?.source_enumeration_available]);
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
      const preset = qualityPresets[quality];
      const next = await invokeMedia<Snapshot>("create_media_session", {
        request: {
          source: sourceKind === "display" ? "screen" : "window",
          source_id: sourceId,
          quality,
          resolution: preset.resolution,
          frame_rate: preset.frame_rate,
          system_audio: systemAudio,
          excluded_apps: excludedApps
        }
      });
      setSession(next);
      setNotice(next.detail);
      try {
        await invokeMedia<Signal>("create_media_peer_offer");
        const refreshed = await invokeMedia<Snapshot>("get_media_session_state");
        setSession(refreshed);
        setNotice(refreshed.session_code ? `Session ${refreshed.session_code} is live. Share that code.` : refreshed.detail);
      } catch (error) {
        setNotice(`Capture is running, but the offer failed: ${diagnosticError(error, "unknown error")}`);
      }
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
  const joinRoom = async () => {
    if (sessionAction !== "idle") return;
    const code = joinCode.trim().toUpperCase();
    if (code.length < 4) {
      setNotice("Enter the 6-character session code.");
      return;
    }
    setSessionAction("joining");
    setNotice("Looking for the host on your network…");
    try {
      const [host, offer] = await invokeMedia<[string, Signal]>("discover_media_room", { request: { code } });
      const pc = new RTCPeerConnection({ iceServers: [] });
      peerRef.current?.close();
      peerRef.current = pc;
      setWatchIce("connecting");
      setWatchCode(code);
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
          setNotice(`Connected to ${code}.`);
        } else if (state === "failed" || state === "disconnected" || state === "closed") {
          setWatchIce("lost");
          setNotice("Connection lost.");
          setWatchZoom(1);
          leaveImmersive();
        }
      };
      pc.oniceconnectionstatechange = handleIce;
      handleIce();
      await invokeMedia("submit_media_room_answer", { request: { host, answer: { type: "answer", sdp: local.sdp } } });
      if (pc.iceConnectionState !== "connected" && pc.iceConnectionState !== "completed") setNotice(`Joined ${code}. Waiting for media…`);
    } catch (error) {
      peerRef.current?.close();
      peerRef.current = null;
      setWatchIce("idle");
      setNotice(diagnosticError(error, "Could not join the session."));
    } finally {
      setSessionAction("idle");
    }
  };
  const copyCode = async () => {
    if (!session?.session_code) return;
    await navigator.clipboard?.writeText(session.session_code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
  };
  const filteredSources = sources.filter((item) => item.kind === sourceKind);
  const permissionLabel = caps === null ? "Checking Screen Recording access…" : caps.screen_recording_authorization === "granted" ? "Screen Recording ready" : caps.screen_recording_authorization === "not_granted" ? "Screen Recording permission needed" : "Native capture unavailable";

  return (
    <div className={`app-shell${cinema ? " cinema" : ""}`}>
      <aside className="sidebar">
        <div className="brand"><span className="brand-mark"><i/><i/><i/></span><span>goDrinking</span></div>
        <div className="nav-label">Workspace</div>
        <nav aria-label="Main navigation">
          <button className={`nav-item ${mode === "share" ? "active" : ""}`} onClick={() => setMode("share")}><Icon name="grid"/> Share screen</button>
          <button className={`nav-item ${mode === "watch" ? "active" : ""}`} onClick={() => setMode("watch")}><Icon name="monitor"/> Watch</button>
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
            <h1>{mode === "share" ? <>Share your screen,<br/><em>stay close.</em></> : watchConnected ? <>Watching <em>{watchCode}</em></> : <>Enter a code,<br/><em>watch live.</em></>}</h1>
          </div>
        </div>
        {mode === "watch" ? (
          <div className={watchConnected ? "watch-live-grid" : "workspace-grid"}>
            {!watchConnected && (
              <section className="panel controls-panel">
                <div className="panel-title"><div><span className="section-kicker">01 / Join</span><h2>Session code</h2></div></div>
                <input className="native-source" value={joinCode} onChange={(event) => setJoinCode(event.target.value.toUpperCase())} placeholder="ABC123" maxLength={8}/>
                <p className="start-hint">{notice}</p>
                <button className="primary-cta" disabled={sessionAction !== "idle"} onClick={() => void joinRoom()}>
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
                      <span className="watch-chip watch-chip-code">{watchCode}</span>
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
              {watchConnected && <p className="watch-status-line">{notice}</p>}
            </section>
          </div>
        ) : (
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
                  <label className="native-select-label" htmlFor="exclude-apps">Don't share audio from</label>
                  <select id="exclude-apps" className="native-source" value="" onChange={(event) => {
                    const token = event.target.value;
                    event.target.value = "";
                    if (token) setExcludedApps((current) => current.includes(token) ? current : [...current, token]);
                  }}>
                    <option value="">Choose an app</option>
                    {apps.map((app) => <option key={`${app.pid}-${app.bundle_id ?? app.name}`} value={app.bundle_id || app.name}>{app.name}</option>)}
                  </select>
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
              <p className="start-hint">{notice}</p>
              {active ? (
                <button className="primary-cta" disabled={sessionAction !== "idle"} onClick={() => void stopSharing()}>{sessionAction === "stopping" ? "Stopping…" : "Stop native session"}</button>
              ) : (
                <button className="primary-cta" disabled={!canStart || sessionAction !== "idle"} onClick={() => void startSharing()}>{sessionAction === "starting" ? "Starting…" : "Start native session"}</button>
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
                <div className="panel-title"><div><span className="section-kicker">02 / Connect</span><h2>Pass this code</h2></div><Icon name="wifi" size={19}/></div>
                <div className="signal-block">
                  <label>Session code</label>
                  <input className="signal-input" readOnly value={roomLabel} placeholder="Start a session to create a code"/>
                  <button className="copy-button" onClick={() => void copyCode()} disabled={!session?.session_code}><Icon name="copy" size={15}/>{copied ? "Copied" : "Copy"}</button>
                </div>
                <p className="signaling-status">{session?.peer_detail || "The other person opens Watch and enters this code on the same network."}</p>
              </section>
            </div>
          </div>
        )}
      </main>
    </div>
  );
}

export default App;
