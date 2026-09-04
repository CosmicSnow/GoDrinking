import { useEffect, useRef, useState, type PointerEvent, type RefObject } from "react";
import { collectViewerStats, type ViewerStatsPrev } from "./sessionStats";
import type { Copy } from "./copy";

export type RoomPerson = {
  id: string;
  nickname: string;
  master?: boolean;
  share?: boolean;
  state?: string;
};

export type RoomTile = {
  id: string;
  nickname: string;
  stream: MediaStream | null;
  local?: boolean;
  pc?: RTCPeerConnection | null;
};

type Props = {
  tiles: RoomTile[];
  people: RoomPerson[];
  selfId?: string | null;
  watching: Set<string>;
  pinned: string | null;
  sharing: boolean;
  shareBusy: boolean;
  canShare: boolean;
  roomLabel: string;
  onWatch: (id: string) => void;
  onUnwatch: (id: string) => void;
  onPin: (id: string | null) => void;
  onShare: () => void;
  onStopShare: () => void;
  onSettings: () => void;
  onLeave: () => void;
  localCanvas?: RefObject<HTMLCanvasElement | null>;
  copy: Copy;
};

export function roomGridShape(count: number): { cols: number; rows: number } {
  if (count <= 1) return { cols: 1, rows: 1 };
  if (count === 2) return { cols: 2, rows: 1 };
  if (count <= 4) return { cols: 2, rows: 2 };
  if (count <= 6) return { cols: 3, rows: 2 };
  if (count <= 9) return { cols: 3, rows: 3 };
  return { cols: 4, rows: Math.ceil(count / 4) };
}

/** Only your own share, plus remote streams you asked to Watch that actually have video. */
export function liveRoomTiles(tiles: RoomTile[], watching: Set<string>): RoomTile[] {
  return tiles.filter((tile) => {
    if (tile.local) return true;
    if (!watching.has(tile.id)) return false;
    return Boolean(tile.stream && tile.stream.getVideoTracks().some((track) => track.readyState !== "ended"));
  });
}

type TileCtl = { zoom: number; panX: number; panY: number; volume: number; muted: boolean };

const defaultCtl = (): TileCtl => ({ zoom: 1, panX: 0, panY: 0, volume: 80, muted: false });

function TileVideo({
  tile,
  localCanvas,
  pinned,
  cinema,
  ctl,
  statsOpen,
  statsText,
  onPin,
  onUnwatch,
  onZoom,
  onPan,
  onVolume,
  onMute,
  onCinema,
  onStats,
  copy,
}: {
  tile: RoomTile;
  localCanvas?: RefObject<HTMLCanvasElement | null>;
  pinned: string | null;
  cinema: boolean;
  ctl: TileCtl;
  statsOpen: boolean;
  statsText: string;
  onPin: (id: string | null) => void;
  onUnwatch: (id: string) => void;
  onZoom: (id: string, zoom: number) => void;
  onPan: (id: string, panX: number, panY: number) => void;
  onVolume: (id: string, volume: number) => void;
  onMute: (id: string, muted: boolean) => void;
  onCinema: (id: string | null) => void;
  onStats: (id: string | null) => void;
  copy: Copy;
}) {
  const zoomOut = Math.max(1, Math.round((ctl.zoom - 0.25) * 100) / 100);
  const zoomIn = Math.min(3, Math.round((ctl.zoom + 0.25) * 100) / 100);
  const drag = useRef<null | { x: number; y: number; panX: number; panY: number }>(null);
  const transform = `translate(${ctl.panX}px, ${ctl.panY}px) scale(${ctl.zoom})`;
  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (ctl.zoom <= 1 || event.button !== 0) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { x: event.clientX, y: event.clientY, panX: ctl.panX, panY: ctl.panY };
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    onPan(tile.id, drag.current.panX + (event.clientX - drag.current.x), drag.current.panY + (event.clientY - drag.current.y));
  };
  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (drag.current) {
      try { event.currentTarget.releasePointerCapture(event.pointerId); } catch { /* already released */ }
    }
    drag.current = null;
  };
  return (
    <div
      className={`room-tile ${pinned === tile.id ? "is-pinned-tile" : ""} ${cinema ? "is-cinema-tile" : ""} ${ctl.zoom > 1 ? "is-zoomed" : ""}`}
      onDoubleClick={() => onPin(pinned === tile.id ? null : tile.id)}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
    >
      {tile.local ? (
        <canvas ref={localCanvas} className="room-video visible" aria-label="Your screen" style={{ transform }}/>
      ) : (
        <video
          className="room-video visible"
          autoPlay
          playsInline
          data-slot={tile.id}
          style={{ transform }}
          ref={(el) => {
            if (!el) return;
            if (tile.stream && el.srcObject !== tile.stream) el.srcObject = tile.stream;
            el.muted = ctl.muted;
            el.volume = ctl.muted ? 0 : ctl.volume / 100;
          }}
        />
      )}
      {statsOpen && <div className="room-tile-stats" onPointerDown={(event) => event.stopPropagation()}>{statsText}</div>}
      <div className="room-tile-hud">
        <span>{tile.nickname}{tile.local ? " · you" : ""}</span>
        <span className="room-tile-actions" onClick={(event) => event.stopPropagation()} onPointerDown={(event) => event.stopPropagation()}>
          <button type="button" className="room-stat-btn" onClick={() => onStats(statsOpen ? null : tile.id)} title={copy.streamStatus} aria-label={copy.streamStatus}>
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M3 12h4l3 8 4-16 3 8h4"/></svg>
          </button>
          <button type="button" onClick={() => onZoom(tile.id, zoomOut)} disabled={ctl.zoom <= 1} title="Zoom out">−</button>
          <button type="button" onClick={() => { onZoom(tile.id, 1); onPan(tile.id, 0, 0); }} title="Reset zoom">{Math.round(ctl.zoom * 100)}%</button>
          <button type="button" onClick={() => onZoom(tile.id, zoomIn)} disabled={ctl.zoom >= 3} title="Zoom in">+</button>
          {!tile.local && (
            <>
              <button type="button" onClick={() => onMute(tile.id, !ctl.muted)} title={ctl.muted ? copy.muted : copy.volume}>{ctl.muted ? copy.muted : copy.volume}</button>
              <input
                className="room-tile-vol"
                type="range"
                min={0}
                max={100}
                step={1}
                value={ctl.muted ? 0 : ctl.volume}
                onChange={(event) => {
                  const value = Number(event.target.value);
                  onVolume(tile.id, value);
                  if (value > 0) onMute(tile.id, false);
                }}
                title="Volume"
                aria-label="Volume"
              />
            </>
          )}
          <button type="button" onClick={() => onCinema(cinema ? null : tile.id)} title={cinema ? copy.videoOnlyExit : copy.videoOnly}>{cinema ? copy.videoOnlyExit : copy.videoOnly}</button>
          <button type="button" onClick={() => onPin(pinned === tile.id ? null : tile.id)}>{pinned === tile.id ? copy.unpinTile : copy.pinTile}</button>
          {!tile.local && (
            <button type="button" onClick={() => onUnwatch(tile.id)}>{copy.unwatchPerson}</button>
          )}
        </span>
      </div>
    </div>
  );
}

export function RoomStage({
  tiles,
  people,
  selfId,
  watching,
  pinned,
  sharing,
  shareBusy,
  canShare,
  roomLabel,
  onWatch,
  onUnwatch,
  onPin,
  onShare,
  onStopShare,
  onSettings,
  onLeave,
  localCanvas,
  copy,
}: Props) {
  const visible = liveRoomTiles(tiles, watching);
  const [ctl, setCtl] = useState<Record<string, TileCtl>>({});
  const [cinemaId, setCinemaId] = useState<string | null>(null);
  const [statsId, setStatsId] = useState<string | null>(null);
  const [statsText, setStatsText] = useState(copy.collecting);
  const statsPrev = useRef<ViewerStatsPrev>(null);
  const ctlOf = (id: string) => ctl[id] ?? defaultCtl();
  const patchCtl = (id: string, next: Partial<TileCtl>) => {
    setCtl((current) => ({ ...current, [id]: { ...defaultCtl(), ...current[id], ...next } }));
  };
  useEffect(() => {
    if (!statsId) return undefined;
    const tile = tiles.find((item) => item.id === statsId);
    const pc = tile?.pc;
    if (!pc) {
      setStatsText(tile?.local ? copy.yourEncoder : copy.noPeerYet);
      return undefined;
    }
    let alive = true;
    const tick = async () => {
      const video = document.querySelector<HTMLVideoElement>(`video[data-slot="${statsId}"]`);
      const { stats, prev } = await collectViewerStats(pc, video, statsPrev.current);
      statsPrev.current = prev;
      if (!alive) return;
      const bits = stats.bitrateMbps != null ? `${stats.bitrateMbps} Mbps` : "—";
      const res = stats.resolution ?? "—";
      const fps = stats.fps != null ? `${stats.fps} fps` : "—";
      const rtt = stats.rttMs != null ? `${stats.rttMs} ms` : "—";
      setStatsText(`${bits} · ${res} · ${fps} · RTT ${rtt}`);
    };
    void tick();
    const timer = window.setInterval(() => void tick(), 1000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [statsId, tiles, copy.yourEncoder, copy.noPeerYet]);
  const stageTiles = cinemaId
    ? visible.filter((tile) => tile.id === cinemaId)
    : pinned
      ? visible.filter((tile) => tile.id === pinned)
      : visible;
  const thumbs = pinned && !cinemaId ? visible.filter((tile) => tile.id !== pinned) : [];
  const shape = roomGridShape(Math.max(stageTiles.length, 1));
  const others = people.filter((person) => person.id !== selfId);
  const me = people.find((person) => person.id === selfId);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (cinemaId) {
        event.preventDefault();
        setCinemaId(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cinemaId]);

  return (
    <div className={`room-live-shell ${cinemaId ? "is-cinema" : ""}`}>
      <header className="room-live-bar">
        <div className="room-live-ident">
          <span className="room-live-kicker">{copy.room}</span>
          <strong>{roomLabel || "Room"}</strong>
        </div>
        <div className="room-live-bar-actions">
          <button type="button" className="room-bar-btn" onClick={onSettings}>{copy.roomSettings}</button>
          <button type="button" className="room-bar-btn is-leave" onClick={onLeave}>{copy.leave}</button>
        </div>
      </header>
      <div className="room-live-body">
        <div className="room-live-stage">
          <div
            className={`room-discord-grid ${pinned && !cinemaId ? "is-spotlight" : ""}`}
            style={{
              gridTemplateColumns: `repeat(${shape.cols}, minmax(0, 1fr))`,
              gridTemplateRows: `repeat(${shape.rows}, minmax(0, 1fr))`,
            }}
          >
            {stageTiles.length === 0 && (
              <div className="room-empty">
                <strong>{copy.nobodyOnScreen}</strong>
                <small>{copy.nobodyOnScreenHint}</small>
              </div>
            )}
            {stageTiles.map((tile) => (
              <TileVideo
                key={tile.id}
                tile={tile}
                localCanvas={localCanvas}
                pinned={pinned}
                cinema={cinemaId === tile.id}
                ctl={ctlOf(tile.id)}
                statsOpen={statsId === tile.id}
                statsText={statsText}
                onPin={onPin}
                onUnwatch={onUnwatch}
                onZoom={(id, zoom) => patchCtl(id, { zoom, ...(zoom <= 1 ? { panX: 0, panY: 0 } : {}) })}
                onPan={(id, panX, panY) => patchCtl(id, { panX, panY })}
                onVolume={(id, volume) => patchCtl(id, { volume })}
                onMute={(id, muted) => patchCtl(id, { muted })}
                onCinema={setCinemaId}
                onStats={setStatsId}
                copy={copy}
              />
            ))}
          </div>
          {thumbs.length > 0 && (
            <div className="room-filmstrip">
              {thumbs.map((tile) => (
                <button
                  type="button"
                  key={tile.id}
                  className="room-thumb-tile"
                  onClick={() => onPin(tile.id)}
                  title={`Pin ${tile.nickname}`}
                >
                  {tile.local ? (
                    <canvas className="room-thumb-video" aria-hidden="true"/>
                  ) : (
                    <video
                      className="room-thumb-video"
                      autoPlay
                      playsInline
                      muted
                      ref={(el) => {
                        if (el && tile.stream && el.srcObject !== tile.stream) el.srcObject = tile.stream;
                      }}
                    />
                  )}
                  <span>{tile.nickname}</span>
                </button>
              ))}
            </div>
          )}
        </div>
        <aside className="room-live-rail">
          {sharing ? (
            <button type="button" className="room-share-btn is-live" disabled={shareBusy} onClick={onStopShare}>{copy.stopShareMine}</button>
          ) : (
            <button type="button" className="room-share-btn" disabled={shareBusy || !canShare} onClick={onShare}>{copy.shareMine}</button>
          )}
          <p className="room-rail-label">{copy.inTheRoom} · {people.length || 1}</p>
          {me ? (
            <div className="room-person is-you">
              <span>
                {me.master ? <span className="roster-crown" title="Master">♛</span> : null}
                {me.nickname}
                <small>{sharing ? copy.sharingYou : copy.you}</small>
              </span>
            </div>
          ) : (
            <div className="room-person is-you">
              <span>{copy.you}<small>{sharing ? copy.sharing : copy.inRoom}</small></span>
            </div>
          )}
          {others.length === 0 && <p className="roster-empty">{copy.onlyYou}</p>}
          {others.map((person) => {
            const on = watching.has(person.id);
            return (
              <div className="room-person" key={person.id}>
                <span>
                  {person.master ? <span className="roster-crown" title="Master">♛</span> : null}
                  {person.nickname}
                  <small>{person.share ? (on ? copy.liveWatching : copy.sharing) : copy.inRoom}</small>
                </span>
                <button
                  type="button"
                  className={on ? "room-watch is-on" : "room-watch"}
                  disabled={!person.share && !on}
                  onClick={() => (on ? onUnwatch(person.id) : onWatch(person.id))}
                >
                  {on ? copy.unwatchPerson : copy.watchPerson}
                </button>
              </div>
            );
          })}
        </aside>
      </div>
    </div>
  );
}
