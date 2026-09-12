import { useEffect, useRef, useState } from "react";
import { Channel, isTauri, playerAttach, playerDetach, playerAck, playerAudio, playerPopup, playerContext, onPlayerState, onPlayerEnded, type PlayerState } from "./api";

export function parsePlayerFrame(buffer: ArrayBuffer) {
  if (buffer.byteLength < 12) throw new Error("Frame incompleto");
  const header = new DataView(buffer);
  const seq = header.getUint32(0, true), width = header.getUint32(4, true), height = header.getUint32(8, true);
  if (!width || !height || width > 8192 || height > 8192 || buffer.byteLength !== 12 + width * height * 4) throw new Error("Frame inválido");
  return { seq, width, height, pixels: new Uint8ClampedArray(buffer, 12) };
}
export function clampZoom(scale: number) {
  if (!Number.isFinite(scale)) return 1;
  return Math.min(4, Math.max(1, scale));
}
export type PlayerZoom = { scale: number; x: number; y: number };
export function clampPan(zoom: PlayerZoom, width: number, height: number): PlayerZoom {
  if (zoom.scale <= 1) return { scale: 1, x: 0, y: 0 };
  const maxX = width * (zoom.scale - 1) / 2;
  const maxY = height * (zoom.scale - 1) / 2;
  return {
    scale: zoom.scale,
    x: Math.max(-maxX, Math.min(maxX, zoom.x)),
    y: Math.max(-maxY, Math.min(maxY, zoom.y)),
  };
}
/** Roda para cima aumenta; o ponto sob o cursor fica parado. 1× zera o pan. */
export function applyWheelZoom(
  zoom: PlayerZoom,
  deltaY: number,
  cursor: { x: number; y: number } = { x: 0, y: 0 },
  size: { width: number; height: number } = { width: 0, height: 0 },
): PlayerZoom {
  const scale = clampZoom(zoom.scale + (deltaY < 0 ? 0.2 : -0.2));
  if (scale === 1) return { scale, x: 0, y: 0 };
  const cx = cursor.x - size.width / 2;
  const cy = cursor.y - size.height / 2;
  const contentX = zoom.scale === 0 ? 0 : (cx - zoom.x) / zoom.scale;
  const contentY = zoom.scale === 0 ? 0 : (cy - zoom.y) / zoom.scale;
  return clampPan({ scale, x: cx - contentX * scale, y: cy - contentY * scale }, size.width, size.height);
}

interface Props {
  member: string;
  nickname: string;
  pinned?: boolean;
  popupWindow?: boolean;
  onPin?: () => void;
  onStop?: () => void;
}
export function StreamPlayer({ member, nickname, pinned, popupWindow = false, onPin, onStop }: Props) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const host = useRef<HTMLElement>(null);
  const viewport = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<PlayerState>({ member, title: nickname, popup: popupWindow, volume: 1, muted: false, mute_all: false });
  const [hasFrame, setHasFrame] = useState(false);
  const [frozen, setFrozen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const [moving, setMoving] = useState(false);
  const [zoom, setZoom] = useState({ scale: 1, x: 0, y: 0 });
  const drag = useRef<{ x: number; y: number } | null>(null);
  const detached = state.popup && !popupWindow;
  const reset = () => setZoom({ scale: 1, x: 0, y: 0 });
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let raf = 0;
    let frozenTimer: ReturnType<typeof setTimeout> | undefined;
    let unlisten: (() => void) | undefined;
    const token = crypto.randomUUID();
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = buffer => {
      if (disposed) return;
      try {
        const frame = parsePlayerFrame(buffer);
        raf = requestAnimationFrame(() => {
          if (disposed) return;
          let drawn = false;
          try {
            const target = canvas.current;
            const context = target?.getContext("2d");
            if (target && context) {
              if (target.width !== frame.width) target.width = frame.width;
              if (target.height !== frame.height) target.height = frame.height;
              context.putImageData(new ImageData(frame.pixels, frame.width, frame.height), 0, 0);
              drawn = true;
              setHasFrame(true);
              setFrozen(false);
              clearTimeout(frozenTimer);
              frozenTimer = setTimeout(() => setFrozen(true), 1000);
              setError(null);
            }
          } catch { setError("Não foi possível desenhar o vídeo."); }
          void playerAck(member, token, frame.seq, drawn).catch(() => { if (!disposed) setError("Conexão com o player interrompida."); });
        });
      } catch { setError("Frame de vídeo inválido. Reconecte o player."); }
    };
    void (async () => {
      try {
        unlisten = await onPlayerState(next => { if (!disposed && next.member === member) setState(next); });
        if (disposed) { unlisten(); return; }
        const initial = await playerAttach(member, token, channel);
        if (!disposed) setState(initial);
        else await playerDetach(member, token);
      } catch (e) { if (!disposed) setError(String(e)); }
    })();
    return () => { disposed = true; cancelAnimationFrame(raf); clearTimeout(frozenTimer); unlisten?.(); void playerDetach(member, token).catch(() => {}); };
  }, [member, retry]);
  useEffect(() => {
    const el = viewport.current;
    if (!el) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      if (detached) return;
      const bounds = el.getBoundingClientRect();
      setZoom((z) => applyWheelZoom(z, event.deltaY, { x: event.clientX - bounds.left, y: event.clientY - bounds.top }, { width: bounds.width, height: bounds.height }));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [detached]);
  const full = async () => {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else if (host.current?.requestFullscreen) await host.current.requestFullscreen();
      else setError("Tela cheia indisponível nesta janela.");
    } catch { setError("Não foi possível entrar em tela cheia."); }
  };
  const move = async () => {
    setMoving(true); setError(null);
    try { await playerPopup(member, !state.popup); } catch (e) { setError(String(e)); }
    finally { setMoving(false); }
  };
  const audio = (volume: number, muted: boolean) => {
    void playerAudio(member, volume, muted).catch(e => setError(String(e)));
  };
  return <article ref={host} className={`tile stream-player${pinned ? " is-pinned" : ""}${detached ? " is-popup" : ""}`} data-hook="tile-stream" data-member={member} tabIndex={0} aria-label={`Transmissão de ${nickname}`}
    onKeyDown={e => {
      if ((e.target as HTMLElement).matches("input,button,select")) return;
      if (e.key.toLowerCase() === "f") { e.preventDefault(); void full(); }
      if (e.key === "0" || e.key === "Escape") reset();
    }}>
    <div ref={viewport} className="viewport" onDoubleClick={() => { if (!detached) void full(); }}
      onPointerDown={e => { if (zoom.scale > 1 && !detached) { e.currentTarget.setPointerCapture(e.pointerId); drag.current = { x: e.clientX, y: e.clientY }; } }}
      onPointerMove={e => { const previous = drag.current; if (previous) { const bounds = e.currentTarget.getBoundingClientRect(); const dx = e.clientX - previous.x, dy = e.clientY - previous.y; setZoom(z => clampPan({ ...z, x: z.x + dx, y: z.y + dy }, bounds.width, bounds.height)); drag.current = { x: e.clientX, y: e.clientY }; } }}
      onPointerUp={() => { drag.current = null; }} onPointerCancel={() => { drag.current = null; }}>
      <canvas ref={canvas} aria-label={`Vídeo de ${nickname}`} style={{ visibility: detached ? "hidden" : "visible", transform: `translate(${zoom.x}px, ${zoom.y}px) scale(${zoom.scale})` }} />
      {detached ? <div className="watch-center"><span className="popup-symbol" aria-hidden="true">↗</span><strong>Em pop-up</strong><span className="watch-note">Esta transmissão está em outra janela.</span><button className="watch-btn" disabled={moving} onClick={() => void move()}>Trazer para a sala</button></div>
        : !hasFrame ? <div className="watch-center"><span className="watch-note">Aguardando vídeo…</span></div> : null}
    </div>
    {hasFrame && !detached ? <div className={`badge-live${frozen ? " frozen" : ""}`}><i />{frozen ? "PAUSADO" : "AO VIVO"}</div> : null}
    <div className="tile-top"><span className="name-tag">{nickname}</span></div>
    {error ? <div className="player-error" role="alert">{error} <button type="button" onClick={() => { setError(null); setRetry(n => n + 1); }}>Reconectar player</button></div> : null}
    <div className="tile-controls player-controls">
      {onPin ? <button type="button" className="tbtn pin" onClick={onPin} aria-pressed={!!pinned} title={pinned ? "Desafixar" : "Fixar no centro"}>{pinned ? "Desafixar" : "Fixar"}</button> : null}
      <button type="button" className="tbtn" data-testid="player-popup" disabled={moving} onClick={() => void move()} title={state.popup ? "Voltar para a sala" : "Abrir apenas esta transmissão em outra janela"}>{moving ? "Movendo…" : state.popup ? "Voltar à sala" : "Pop-up ↗"}</button>
      {!detached ? <><button type="button" className="tbtn full" onClick={() => void full()} title="Tela cheia (F ou duplo-clique)">Ampliar</button><button type="button" className="tbtn zoom-reset" onClick={reset} title="Restaurar zoom e posição (0)">{Math.round(zoom.scale * 100)}%</button></> : null}
      <div className="player-volume"><button type="button" className={`tbtn mute${state.muted ? " on" : ""}`} onClick={() => audio(state.volume, !state.muted)} aria-pressed={state.muted} title={state.mute_all ? "Áudio geral silenciado" : "Silenciar esta transmissão"}>{state.muted ? "Mudo" : "Som"}</button>
      <input type="range" min="0" max="100" value={Math.round(state.volume * 100)} aria-label={`Volume de ${nickname}`} title={`Volume: ${Math.round(state.volume * 100)}%`} onChange={e => audio(Number(e.target.value) / 100, state.muted)} /></div>
      {onStop ? <button type="button" className="tbtn warn" onClick={onStop}>Parar de ver</button> : null}
    </div>
  </article>;
}

export function PopupPlayer() {
  const [context, setContext] = useState<PlayerState | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let disposed = false;
    let cleanup: (() => void) | undefined;
    void playerContext().then(async value => {
      if (disposed) return;
      setContext(value);
      cleanup = await onPlayerEnded(member => { if (member === value.member) { setError("A transmissão terminou."); setContext(null); } });
      if (disposed) cleanup();
    }).catch(e => { if (!disposed) setError(String(e)); });
    return () => { disposed = true; cleanup?.(); };
  }, []);
  return <main className="popup-player">{context ? <StreamPlayer member={context.member} nickname={context.title || "Transmissão"} popupWindow /> : <p role="status">{error || "Abrindo transmissão…"}</p>}</main>;
}
