import { type RefObject } from "react";

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

function TileVideo({
  tile,
  localCanvas,
  pinned,
  onPin,
  onUnwatch,
}: {
  tile: RoomTile;
  localCanvas?: RefObject<HTMLCanvasElement | null>;
  pinned: string | null;
  onPin: (id: string | null) => void;
  onUnwatch: (id: string) => void;
}) {
  return (
    <div
      className={`room-tile ${pinned === tile.id ? "is-pinned-tile" : ""}`}
      onDoubleClick={() => onPin(pinned === tile.id ? null : tile.id)}
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
              <button type="button" onClick={() => onPin(pinned === tile.id ? null : tile.id)}>
                {pinned === tile.id ? "Unpin" : "Pin"}
              </button>
              <button type="button" onClick={() => onUnwatch(tile.id)}>Stop</button>
            </>
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
  const stageTiles = pinned ? tiles.filter((tile) => tile.id === pinned) : tiles;
  const thumbs = pinned ? tiles.filter((tile) => tile.id !== pinned) : [];
  const shape = roomGridShape(Math.max(stageTiles.length, 1));
  const others = people.filter((person) => person.id !== selfId);
  const me = people.find((person) => person.id === selfId);

  return (
    <div className="room-live-shell">
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
            className={`room-discord-grid ${pinned ? "is-spotlight" : ""}`}
            style={{
              gridTemplateColumns: `repeat(${shape.cols}, minmax(0, 1fr))`,
              gridTemplateRows: `repeat(${shape.rows}, minmax(0, 1fr))`,
            }}
          >
            {stageTiles.length === 0 && (
              <div className="room-empty">
                <strong>Nobody on screen</strong>
                <small>Watch someone who is sharing. You can stay off-air.</small>
              </div>
            )}
            {stageTiles.map((tile) => (
              <TileVideo
                key={tile.id}
                tile={tile}
                localCanvas={localCanvas}
                pinned={pinned}
                onPin={onPin}
                onUnwatch={onUnwatch}
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
