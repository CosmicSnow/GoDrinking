/**
 * Componentes puros (só props → markup). Nenhum invoke, nenhum listen,
 * nenhum timer aqui: toda decisão de produto chega pronta via props.
 * Idioma: só pt-BR neste passo (i18n é gate posterior, documentado).
 */

import type {
  CapabilitySet,
  LinkState,
  OwnerSnapshot,
  RoomMember,
  SalaState,
  ShareState,
  SourceInfo,
  ViewerStats,
} from "./api";

// ---------------------------------------------------------------------------
// Validação (espelha as regras do backend; mensagens sem segredos).
// ---------------------------------------------------------------------------

export function validateNickname(nickname: string): string | null {
  const name = nickname.trim();
  if (name.length < 2 || name.length > 24)
    return "Apelido: 2 a 24 caracteres.";
  if (!/^[A-Za-z0-9 _.-]+$/.test(name))
    return "Apelido: letras, números, espaço, _ - .";
  return null;
}

export function validatePassword(password: string): string | null {
  if (password.length < 4 || password.length > 64)
    return "Senha: 4 a 64 caracteres (obrigatória).";
  return null;
}

export function validateCode(code: string): string | null {
  if (code.trim().length < 4) return "Código: peça os 6 caracteres ao host.";
  return null;
}

export function validateSource(source: string): string | null {
  const raw = source.trim();
  if (raw === "synthetic") return null;
  if (raw.startsWith("movie:") && raw.length > "movie:".length) return null;
  if (raw.startsWith("display:") && raw.length > "display:".length) return null;
  if (raw.startsWith("window:") && raw.length > "window:".length) return null;
  return "Fonte: 'synthetic', 'movie:/caminho', 'display:<id>' ou 'window:<id>'.";
}

/** Tipo da fonte a partir do seletor opaco. Puro e testável. */
export type SourceKindSelect = "synthetic" | "movie" | "display" | "window";

export function sourceKindOf(source: string): SourceKindSelect {
  const raw = source.trim();
  if (raw.startsWith("movie:")) return "movie";
  if (raw.startsWith("display:")) return "display";
  if (raw.startsWith("window:")) return "window";
  return "synthetic";
}

// ---------------------------------------------------------------------------
// Rótulos de estado (espelham core/src/state.rs; minúsculas do backend).
// ---------------------------------------------------------------------------

const SALA_LABEL: Record<SalaState, string> = {
  closed: "Fechada",
  joining: "Entrando…",
  open: "Aberta",
  closing: "Fechando…",
};

const SHARE_LABEL: Record<ShareState, string> = {
  stopped: "Parado",
  starting: "Iniciando…",
  live: "No ar",
  stopping: "Parando…",
};

const LINK_LABEL: Record<LinkState, string> = {
  absent: "Ausente",
  negotiating: "Negociando",
  connected: "Conectado",
  closing: "Fechando",
};

export const salaLabel = (state: SalaState): string => SALA_LABEL[state] ?? state;
export const shareLabel = (state: ShareState): string => SHARE_LABEL[state] ?? state;
export const linkLabel = (state: LinkState): string => LINK_LABEL[state] ?? state;

// ---------------------------------------------------------------------------
// Tela inicial: criar / entrar.
// ---------------------------------------------------------------------------

export interface HomeProps {
  server: string;
  onServer: (value: string) => void;
  tab: "create" | "join";
  onTab: (tab: "create" | "join") => void;
  nickname: string;
  onNickname: (value: string) => void;
  password: string;
  onPassword: (value: string) => void;
  code: string;
  onCode: (value: string) => void;
  busy: boolean;
  error: string | null;
  onCreate: () => void;
  onJoin: () => void;
}

export function HomeScreen(props: HomeProps) {
  const {
    server, onServer, tab, onTab, nickname, onNickname, password, onPassword,
    code, onCode, busy, error, onCreate, onJoin,
  } = props;
  return (
    <div className="screen">
      <header className="brand">
        <h1>GoLive</h1>
        <p className="tagline">Tela direto de um PC para outro. O servidor só apresenta — nunca vê o vídeo.</p>
      </header>

      <section className="panel" aria-label="Servidor">
        <label htmlFor="server">Servidor (rendezvous)</label>
        <input
          id="server"
          value={server}
          onChange={(event) => onServer(event.target.value)}
          placeholder="http://127.0.0.1:18790"
          autoComplete="off"
          disabled={busy}
        />
      </section>

      <section className="panel" aria-label="Entrar na sala">
        <div className="tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "create"}
            className={tab === "create" ? "selected" : ""}
            onClick={() => onTab("create")}
            disabled={busy}
          >
            Criar sala
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "join"}
            className={tab === "join" ? "selected" : ""}
            onClick={() => onTab("join")}
            disabled={busy}
          >
            Entrar
          </button>
        </div>

        <label htmlFor="nickname">Apelido</label>
        <input
          id="nickname"
          value={nickname}
          onChange={(event) => onNickname(event.target.value)}
          placeholder="Seu nome na sala"
          maxLength={24}
          autoComplete="nickname"
          disabled={busy}
        />

        <label htmlFor="password">Senha da sala</label>
        <input
          id="password"
          type="password"
          value={password}
          onChange={(event) => onPassword(event.target.value)}
          placeholder="4 a 64 caracteres"
          maxLength={64}
          autoComplete={tab === "create" ? "new-password" : "current-password"}
          disabled={busy}
        />

        {tab === "join" && (
          <>
            <label htmlFor="code">Código da sala</label>
            <input
              id="code"
              value={code}
              onChange={(event) => onCode(event.target.value.toUpperCase())}
              placeholder="ABC123"
              maxLength={8}
              autoComplete="off"
              disabled={busy}
            />
          </>
        )}

        {error ? (
          <p className="error" role="alert">{error}</p>
        ) : null}

        {tab === "create" ? (
          <button type="button" className="primary" onClick={onCreate} disabled={busy}>
            {busy ? "Criando…" : "Criar sala"}
          </button>
        ) : (
          <button type="button" className="primary" onClick={onJoin} disabled={busy}>
            {busy ? "Entrando…" : "Entrar"}
          </button>
        )}
      </section>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sala: roster, share, watch, links, diagnóstico.
// ---------------------------------------------------------------------------

export interface RoomProps {
  /** Código da sala (nulo quando o backend ainda não o devolveu). */
  roomCode: string | null;
  nickname: string;
  snapshot: OwnerSnapshot | null;
  roster: RoomMember[];
  selfId: string | null;
  selfNickname: string;
  /** Member ids com intenção de watch ativa (eco local; verdade no snapshot). */
  watching: string[];
  source: string;
  onSource: (value: string) => void;
  /** Fontes listadas pelo backend (vazio até "Listar telas"). */
  sources: SourceInfo[];
  /** Erro tipado da última listagem (permissão, plataforma, …). */
  sourcesError: string | null;
  /** Capacidades desta build (desabilita com motivo). */
  caps: CapabilitySet | null;
  onListSources: () => void;
  busy: boolean;
  error: string | null;
  lastSignal: string | null;
  lastMedia: string | null;
  stats: ViewerStats | null;
  onRefresh: () => void;
  onLeave: () => void;
  onShare: () => void;
  onStopShare: () => void;
  onWatch: (id: string) => void;
  onUnwatch: (id: string) => void;
}

const isSelf = (member: RoomMember, selfId: string | null, selfNickname: string): boolean =>
  (selfId !== null && member.id === selfId) ||
  (selfId === null && member.nickname === selfNickname);

export function RoomScreen(props: RoomProps) {
  const {
    roomCode, nickname, snapshot, roster, selfId, selfNickname, watching,
    source, onSource, sources, sourcesError, caps, onListSources,
    busy, error, lastSignal, lastMedia, stats,
    onRefresh, onLeave, onShare, onStopShare, onWatch, onUnwatch,
  } = props;
  const watchingSet = new Set(watching);
  const shareState = snapshot?.share.state ?? null;
  const sharing = shareState === "live" || shareState === "starting";
  const links = snapshot?.links ?? [];
  const liveWatchers = snapshot?.watchers ?? [];

  return (
    <div className="screen">
      <header className="room-bar">
        <div>
          <span className="kicker">Sala</span>
          <strong data-testid="room-code">{roomCode ?? "sem código ainda"}</strong>
          <small>{nickname}</small>
        </div>
        <div className="room-bar-actions">
          <button type="button" onClick={onRefresh} disabled={busy} title="Lê o snapshot do backend agora">
            {busy ? "Atualizando…" : "Atualizar"}
          </button>
          <button type="button" className="danger" onClick={onLeave} disabled={busy}>
            Sair
          </button>
        </div>
      </header>

      {error ? (
        <p className="error" role="alert">{error}</p>
      ) : null}

      <section className="panel" aria-label="Compartilhar tela">
        <h2>Sua tela</h2>
        <label htmlFor="source-kind">Fonte</label>
        <select
          id="source-kind"
          value={sourceKindOf(source)}
          onChange={(event) => {
            const kind = event.target.value as SourceKindSelect;
            if (kind === "synthetic") onSource("synthetic");
            else if (kind === "movie") onSource("movie:");
            else if (kind === "display") onSource("display:");
            else onSource("window:");
          }}
          disabled={busy || sharing}
        >
          <option value="synthetic">Sintética (teste)</option>
          <option value="movie">Arquivo de vídeo</option>
          <option value="display" disabled={caps !== null && !caps.display.supported}>
            Tela {caps && !caps.display.supported ? `(${caps.display.reason})` : ""}
          </option>
          <option value="window" disabled={caps !== null && !caps.window.supported}>
            Janela {caps && !caps.window.supported ? `(${caps.window.reason})` : ""}
          </option>
        </select>
        {sourceKindOf(source) === "movie" ? (
          <>
            <label htmlFor="source">Arquivo</label>
            <input
              id="source"
              value={source}
              onChange={(event) => onSource(event.target.value)}
              placeholder="movie:/caminho/do/arquivo"
              autoComplete="off"
              disabled={busy || sharing}
            />
          </>
        ) : null}
        {sourceKindOf(source) === "display" || sourceKindOf(source) === "window" ? (
          <>
            <label htmlFor="source-pick">
              {sourceKindOf(source) === "display" ? "Tela" : "Janela"}
            </label>
            <select
              id="source-pick"
              value={source}
              onChange={(event) => onSource(event.target.value)}
              disabled={busy || sharing || sources.length === 0}
            >
              <option value={sourceKindOf(source) + ":"}>Escolha…</option>
              {sources
                .filter((item) => item.kind === sourceKindOf(source))
                .map((item) => (
                  <option key={`${item.kind}:${item.id}`} value={`${item.kind}:${item.id}`}>
                    {item.name}
                  </option>
                ))}
            </select>
            <div className="row">
              <button type="button" onClick={onListSources} disabled={busy || sharing}>
                Listar telas
              </button>
              {sourcesError ? (
                <span className="error" role="alert">{sourcesError}</span>
              ) : null}
            </div>
            <p className="hint">
              A primeira listagem pode pedir permissão ao sistema. Sem permissão,
              nada é capturado — o erro acima explica como autorizar.
            </p>
          </>
        ) : null}
        {sourceKindOf(source) === "synthetic" ? (
          <p className="hint"><code>synthetic</code> gera a bola de teste.</p>
        ) : null}
        <div className="row">
          {sharing ? (
            <button type="button" className="danger" onClick={onStopShare} disabled={busy}>
              {busy ? "Parando…" : "Parar de compartilhar"}
            </button>
          ) : (
            <button type="button" className="primary" onClick={onShare} disabled={busy}>
              {busy ? "Iniciando…" : "Compartilhar"}
            </button>
          )}
          <span className="state" data-testid="share-state">
            Share: {shareState ? shareLabel(shareState) : "desconhecido — toque Atualizar"}
          </span>
        </div>
      </section>

      <section className="panel" aria-label="Quem está na sala">
        <h2>Na sala · {roster.length}</h2>
        {roster.length === 0 ? (
          <p className="empty" role="status">
            Nenhum membro visível ainda. O roster chega pelo evento do servidor —
            aguarde um instante ou toque <strong>Atualizar</strong>.
          </p>
        ) : (
          <ul className="roster">
            {roster.map((member) => {
              const self = isSelf(member, selfId, selfNickname);
              const wantsWatch = watchingSet.has(member.id);
              return (
                <li key={member.id} className={self ? "is-self" : ""}>
                  <span className="member-name">
                    {member.master ? <span title="Master">♛ </span> : null}
                    {member.nickname}
                    {self ? <small> (você)</small> : null}
                  </span>
                  <span className="badges">
                    {member.share ? <span className="badge live">compartilhando</span> : null}
                    {wantsWatch ? <span className="badge">pedido de watch</span> : null}
                  </span>
                  {!self && member.share ? (
                    wantsWatch ? (
                      <button type="button" onClick={() => onUnwatch(member.id)} disabled={busy}>
                        Parar de ver
                      </button>
                    ) : (
                      <button type="button" onClick={() => onWatch(member.id)} disabled={busy}>
                        Assistir
                      </button>
                    )
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
      </section>

      <section className="panel" aria-label="Links de mídia">
        <h2>Links · {links.length}</h2>
        {links.length === 0 ? (
          <p className="empty" role="status">
            Nenhum link ativo no snapshot. Links aparecem quando alguém assiste
            ao seu share (ou quando seu watch vira link no backend).
          </p>
        ) : (
          <ul className="links">
            {links.map((link) => (
              <li key={link.id}>
                <code>{link.watcher}</code>
                <span className="state">{linkLabel(link.state)}</span>
              </li>
            ))}
          </ul>
        )}
        {liveWatchers.length > 0 ? (
          <p className="hint">Assistindo agora: {liveWatchers.join(", ")}</p>
        ) : null}
        <div className="video-placeholder" role="status">
          <strong>
            {links.some((link) => link.state === "connected")
              ? "Vídeo na janela nativa."
              : "Sem vídeo: nenhum link conectado."}
          </strong>
          <span>
            Cada watch abre uma janela nativa do sistema com o vídeo decodificado
            (título com o nome de quem compartilha). O que você vê aqui é o
            estado real do link — o vídeo nunca passa pela WebView.
          </span>
          {stats ? (
            <span data-testid="viewer-stats">
              Frames recebidos: {stats.frames} · keyframes: {stats.keyframes} · ICE:{" "}
              {stats.ice ? "conectado" : "negociando"} · apresentados: {stats.presented}
            </span>
          ) : (
            <span>Frames recebidos: ainda sem amostra do evento de mídia.</span>
          )}
        </div>
      </section>

      <section className="panel" aria-label="Diagnóstico">
        <h2>Diagnóstico</h2>
        <dl className="diag">
          <div>
            <dt>Sala</dt>
            <dd data-testid="sala-state">
              {snapshot ? salaLabel(snapshot.session.state) : "sem snapshot — toque Atualizar"}
            </dd>
          </div>
          <div>
            <dt>Share</dt>
            <dd>{snapshot ? shareLabel(snapshot.share.state) : "—"}</dd>
          </div>
          <div>
            <dt>Links</dt>
            <dd>{snapshot ? `${links.length} ativo(s)` : "—"}</dd>
          </div>
          <div>
            <dt>Último evento (sinal)</dt>
            <dd>{lastSignal ?? "nenhum ainda"}</dd>
          </div>
          <div>
            <dt>Último evento (mídia)</dt>
            <dd>{lastMedia ?? "nenhum ainda"}</dd>
          </div>
          <div>
            <dt>Último erro</dt>
            <dd className={error ? "is-error" : ""}>{error ?? "nenhum"}</dd>
          </div>
        </dl>
      </section>
    </div>
  );
}
