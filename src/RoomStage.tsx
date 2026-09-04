import { useEffect, useState, type RefObject } from "react";

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

type TileCtl = { zoom: number; volume: number; muted: boolean };

const defaultCtl = (): TileCtl => ({ zoom: 1, volume: 80, muted: false });

function TileVideo({
  tile,
  localCanvas,
  pinned,
  cinema,
  ctl,
  onPin,
  onUnwatch,
  onZoom,
  onVolume,
  onMute,
  onCinema,
}: {
  tile: RoomTile;
  localCanvas?: RefObject<HTMLCanvasElement | null>;
  pinned: string | null;
  cinema: boolean;
  ctl: TileCtl;
  onPin: (id: string | null) => void;
  onUnwatch: (id: string) => void;
  onZoom: (id: string, zoom: number) => void;
  onVolume: (id: string, volume: number) => void;
  onMute: (id: string, muted: boolean) => void;
  onCinema: (id: string | null) => void;
}) {
  const zoomOut = Math.max(1, Math.round((ctl.zoom - 0.25) * 100) / 100);
  const zoomIn = Math.min(3, Math.round((ctl.zoom + 0.25) * 100) / 100);
  return (
    <div
      className={`room-tile ${pinned === tile.id ? "is-pinned-tile" : ""} ${cinema ? "is-cinema-tile" : ""}`}
      onDoubleClick={() => onPin(pinned === tile.id ? null : tile.id)}
    >
      {tile.local ? (
        <canvas ref={localCanvas} className="room-video visible" aria-label="Your screen" style={{ transform: `scale(${ctl.zoom})` }}/>
      ) : (
        <video
          className="room-video visible"
          autoPlay
          playsInline
          data-slot={tile.id}
          style={{ transform: `scale(${ctl.zoom})` }}
          ref={(el) => {
            if (!el) return;
            if (tile.stream && el.srcObject !== tile.stream) el.srcObject = tile.stream;
            el.muted = ctl.muted;
            el.volume = ctl.muted ? 0 : ctl.volume / 100;
          }}
        />
      )}
      <div className="room-tile-hud">
        <span>{tile.nickname}{tile.local ? " · you" : ""}</span>
        <span className="room-tile-actions" onClick={(event) => event.stopPropagation()}>
          <button type="button" onClick={() => onZoom(tile.id, zoomOut)} disabled={ctl.zoom <= 1} title="Zoom out">−</button>
          <button type="button" onClick={() => onZoom(tile.id, 1)} title="Reset zoom">{Math.round(ctl.zoom * 100)}%</button>
          <button type="button" onClick={() => onZoom(tile.id, zoomIn)} disabled={ctl.zoom >= 3} title="Zoom in">+</button>
          {!tile.local && (
            <>
              <button type="button" onClick={() => onMute(tile.id, !ctl.muted)} title={ctl.muted ? "Unmute" : "Mute"}>{ctl.muted ? "Muted" : "Vol"}</button>
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
          <button type="button" onClick={() => onCinema(cinema ? null : tile.id)} title={cinema ? "Exit video only" : "Video only"}>{cinema ? "Exit" : "Video only"}</button>
          <button type="button" onClick={() => onPin(pinned === tile.id ? null : tile.id)}>{pinned === tile.id ? "Unpin" : "Pin"}</button>
          {!tile.local && (
            <button type="button" onClick={() => onUnwatch(tile.id)}>Stop</button>
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
}: Props) {
  const visible = liveRoomTiles(tiles, watching);
  const [ctl, setCtl] = useState<Record<string, TileCtl>>({});
  const [cinemaId, setCinemaId] = useState<string | null>(null);
  const ctlOf = (id: string) => ctl[id] ?? defaultCtl();
  const patchCtl = (id: string, next: Partial<TileCtl>) => {
    setCtl((current) => ({ ...current, [id]: { ...defaultCtl(), ...current[id], ...next } }));
  };
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
          <span className="room-live-kicker">Sala</span>
          <strong>{roomLabel || "Room"}</strong>
        </div>
        <div className="room-live-bar-actions">
          <button type="button" className="room-bar-btn" onClick={onSettings}>Settings</button>
          <button type="button" className="room-bar-btn is-leave" onClick={onLeave}>Leave</button>
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
                <strong>Nobody on screen</strong>
                <small>Watch someone who is sharing. Empty tiles stay off the grid.</small>
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
                onPin={onPin}
                onUnwatch={onUnwatch}
                onZoom={(id, zoom) => patchCtl(id, { zoom })}
                onVolume={(id, volume) => patchCtl(id, { volume })}
                onMute={(id, muted) => patchCtl(id, { muted })}
                onCinema={setCinemaId}
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
            <button type="button" className="room-share-btn is-live" disabled={shareBusy} onClick={onStopShare}>Stop sharing</button>
          ) : (
            <button type="button" className="room-share-btn" disabled={shareBusy || !canShare} onClick={onShare}>Share my screen</button>
          )}
          <p className="room-rail-label">In the room</p>
          {me && (
            <div className="room-person is-you">
              <span>
                {me.master ? <span className="roster-crown" title="Master">♛</span> : null}
                {me.nickname}
                <small>{sharing ? "Sharing · you" : "You"}</small>
              </span>
            </div>
          )}
          {others.length === 0 && <p className="roster-empty">Waiting for people.</p>}
          {others.map((person) => {
            const on = watching.has(person.id);
            return (
              <div className="room-person" key={person.id}>
                <span>
                  {person.master ? <span className="roster-crown" title="Master">♛</span> : null}
                  {person.nickname}
                  <small>{person.share ? (on ? "Live · watching" : "Sharing") : "In the room"}</small>
                </span>
                <button
                  type="button"
                  className={on ? "room-watch is-on" : "room-watch"}
                  disabled={!person.share && !on}
                  onClick={() => (on ? onUnwatch(person.id) : onWatch(person.id))}
                >
                  {on ? "Stop" : "Watch"}
                </button>
              </div>
            );
          })}
        </aside>
      </div>
    </div>
  );
}
