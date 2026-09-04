import { type CSSProperties, type RefObject } from "react";

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
  sizes: Record<string, number>;
  pinned: string | null;
  sharing: boolean;
  shareBusy: boolean;
  canShare: boolean;
  onWatch: (id: string) => void;
  onUnwatch: (id: string) => void;
  onPin: (id: string | null) => void;
  onSize: (id: string, delta: 1 | -1) => void;
  onShare: () => void;
  onStopShare: () => void;
  localCanvas?: RefObject<HTMLCanvasElement | null>;
};

export function RoomStage({
  tiles,
  people,
  selfId,
  watching,
  sizes,
  pinned,
  sharing,
  shareBusy,
  canShare,
  onWatch,
  onUnwatch,
  onPin,
  onSize,
  onShare,
  onStopShare,
  localCanvas,
}: Props) {
  const visible = pinned ? tiles.filter((tile) => tile.id === pinned) : tiles;
  const others = people.filter((person) => person.id !== selfId);
  return (
    <div className="room-stage">
      <div className={`room-grid ${pinned ? "is-pinned" : ""}`}>
        {visible.length === 0 && (
          <div className="room-empty">
            <strong>Nobody on screen</strong>
            <small>Watch someone who is sharing. You can stay off-air.</small>
          </div>
        )}
        {visible.map((tile) => {
          const span = Math.min(3, Math.max(1, sizes[tile.id] ?? 2));
          return (
            <div
              key={tile.id}
              className={`room-tile ${pinned === tile.id ? "is-pinned-tile" : ""}`}
              style={{ "--tile-span": span } as CSSProperties}
            >
              {tile.local ? (
                <canvas ref={localCanvas} className="room-video visible" aria-label="Your screen"/>
              ) : (
                <video
                  className="room-video visible"
                  autoPlay
                  playsInline
                  data-slot={tile.id}
                  ref={(el) => {
                    if (el && tile.stream && el.srcObject !== tile.stream) el.srcObject = tile.stream;
                  }}
                />
              )}
              <div className="room-tile-hud">
                <span>{tile.nickname}{tile.local ? " · you" : ""}</span>
                <span className="room-tile-actions">
                  {!tile.local && (
                    <>
                      <button type="button" onClick={() => onSize(tile.id, -1)} disabled={span <= 1} title="Smaller">−</button>
                      <button type="button" onClick={() => onSize(tile.id, 1)} disabled={span >= 3} title="Bigger">+</button>
                      <button type="button" onClick={() => onPin(pinned === tile.id ? null : tile.id)} title={pinned === tile.id ? "Unpin" : "Pin"}>
                        {pinned === tile.id ? "Unpin" : "Pin"}
                      </button>
                      <button type="button" onClick={() => onUnwatch(tile.id)} title="Disconnect this stream">Stop</button>
                    </>
                  )}
                </span>
              </div>
            </div>
          );
        })}
      </div>
      <div className="room-people">
        <div className="room-people-head">
          <span>In the room</span>
          {sharing ? (
            <button type="button" className="room-share-btn is-live" disabled={shareBusy} onClick={onStopShare}>Stop sharing</button>
          ) : (
            <button type="button" className="room-share-btn" disabled={shareBusy || !canShare} onClick={onShare}>Share my screen</button>
          )}
        </div>
        {others.length === 0 && <p className="roster-empty">Waiting for people. Share the code.</p>}
        {others.map((person) => {
          const on = watching.has(person.id);
          return (
            <div className="room-person" key={person.id}>
              <span>
                {person.master ? <span className="roster-crown" title="Master">♛</span> : null}
                {person.nickname}
                <small>{person.share ? "Sharing" : "In the room"}</small>
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
      </div>
    </div>
  );
}
