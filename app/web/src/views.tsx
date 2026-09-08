/**
 * Componentes puros (só props → markup). Nenhum invoke, nenhum listen,
 * nenhum timer aqui: toda decisão de produto chega pronta via props.
 * Idioma: só pt-BR neste passo (i18n é gate posterior, documentado).
 */

import type {
  CapabilitySet,
  EffectiveQuality,
  LinkState,
  LinkStats,
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
// Qualidade (espelha core/src/media.rs `QualityProfile`; aplicada via
// comando `set_quality`, efetivo autoritativo no snapshot/evento).
// ---------------------------------------------------------------------------

export type ResolutionSel = "480p" | "720p" | "1080p" | "custom";
export type QualitySel = "low" | "medium" | "high" | "custom";

export interface DesiredProfile {
  w: number;
  h: number;
  bitrate_kbps: number;
  fps: number;
}

export const RESOLUTION_DIMS: Record<Exclude<ResolutionSel, "custom">, { w: number; h: number }> = {
  "480p": { w: 854, h: 480 },
  "720p": { w: 1280, h: 720 },
  "1080p": { w: 1920, h: 1080 },
};

export const QUALITY_PRESETS: Record<
  Exclude<QualitySel, "custom">,
  DesiredProfile & { label: string }
> = {
  low: { w: 854, h: 480, bitrate_kbps: 800, fps: 15, label: "LOW · 800 kbps · 15 fps" },
  medium: { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30, label: "MEDIUM · 2000 kbps · 30 fps" },
  high: { w: 1920, h: 1080, bitrate_kbps: 10000, fps: 30, label: "HIGH · 10000 kbps · 30 fps" },
};

/** Formata o efetivo autoritativo (perfil + geração do fence). */
export function formatEffective(effective: EffectiveQuality): string {
  const { profile, generation } = effective;
  return `${profile.w}×${profile.h} @ ${profile.bitrate_kbps} kbps · ${profile.fps} fps · geração ${generation}`;
}

/** Selo do codificador (espelha `counters.backend`/`backend_note`).
 * Valores vêm do backend; qualquer outro nome cai no "sem leitura"
 * honesto em vez de rótulo inventado.
 */
export function formatBackend(backend: string | null, note: string | null): string {
  if (backend === "videotoolbox") return "Codificador: VideoToolbox (hardware)";
  if (backend === "openh264") {
    return note
      ? `Codificador: OpenH264 (software) — ${note}`
      : "Codificador: OpenH264 (software)";
  }
  return "Codificador: ainda sem leitura";
}

const parseUint = (raw: string): number | null => {
  const text = raw.trim();
  if (!/^\d+$/.test(text)) return null;
  const value = Number(text);
  return Number.isSafeInteger(value) ? value : null;
};

export function validateCustomDim(raw: string, axis: "largura" | "altura"): string | null {
  const value = parseUint(raw);
  if (value === null) return `${axis}: número inteiro.`;
  if (value < 2) return `${axis}: pelo menos 2 px.`;
  if (value > 4096) return `${axis}: máximo 4096 px.`;
  if (value % 2 !== 0) return `${axis}: use valor par (o encoder exige dimensão par).`;
  return null;
}

export function validateBitrate(raw: string): string | null {
  const value = parseUint(raw);
  if (value === null) return "bitrate: número inteiro em kbps.";
  if (value < 100 || value > 20000) return "bitrate: 100 a 20000 kbps.";
  return null;
}

export function validateFps(raw: string): string | null {
  const value = parseUint(raw);
  if (value === null) return "fps: número inteiro.";
  if (value < 1 || value > 60) return "fps: 1 a 60.";
  return null;
}

export interface QualitySelection {
  resolution: ResolutionSel;
  customW: string;
  customH: string;
  quality: QualitySel;
  customBitrate: string;
  customFps: string;
  /** Dimensões da fonte quando conhecida (display/window listada). */
  srcDims: { w: number; h: number } | null;
}

/**
 * Resolve o perfil desejado ou lista os erros (custom inválido/incompleto).
 * Com fonte conhecida, barra upscale além dela; sem fonte conhecida, vale o
 * teto do backend (4096) e o próprio backend normaliza na borda.
 */
export function resolveDesired(
  selection: QualitySelection,
): { profile: DesiredProfile } | { errors: string[] } {
  const errors: string[] = [];
  let w = 0;
  let h = 0;
  if (selection.resolution === "custom") {
    const wError = validateCustomDim(selection.customW, "largura");
    const hError = validateCustomDim(selection.customH, "altura");
    if (wError) errors.push(wError);
    if (hError) errors.push(hError);
    w = parseUint(selection.customW) ?? 0;
    h = parseUint(selection.customH) ?? 0;
    if (!wError && !hError && selection.srcDims) {
      const { w: srcW, h: srcH } = selection.srcDims;
      if (w > srcW || h > srcH)
        errors.push(`sem upscale além da fonte (${srcW}×${srcH}).`);
    }
  } else {
    ({ w, h } = RESOLUTION_DIMS[selection.resolution]);
  }
  let bitrate_kbps = 0;
  let fps = 0;
  if (selection.quality === "custom") {
    const bitrateError = validateBitrate(selection.customBitrate);
    const fpsError = validateFps(selection.customFps);
    if (bitrateError) errors.push(bitrateError);
    if (fpsError) errors.push(fpsError);
    bitrate_kbps = parseUint(selection.customBitrate) ?? 0;
    fps = parseUint(selection.customFps) ?? 0;
  } else {
    ({ bitrate_kbps, fps } = QUALITY_PRESETS[selection.quality]);
  }
  if (errors.length > 0) return { errors };
  return { profile: { w, h, bitrate_kbps, fps } };
}

// ---------------------------------------------------------------------------
// Formatação de contadores (pt-BR; "—" honesto quando ausente).
// ---------------------------------------------------------------------------

export function formatBps(bps: number): string {
  if (!Number.isFinite(bps) || bps < 0) return "—";
  if (bps < 1000) return `${bps} bps`;
  if (bps < 1_000_000)
    return `${(bps / 1000).toLocaleString("pt-BR", { maximumFractionDigits: 1 })} kbps`;
  return `${(bps / 1_000_000).toLocaleString("pt-BR", { maximumFractionDigits: 1 })} Mbps`;
}

export function formatFps(fps: number): string {
  if (!Number.isFinite(fps) || fps < 0) return "—";
  return `${fps.toFixed(1)} fps`;
}

export function formatDelayMs(delayMs: number | null): string {
  if (delayMs === null) return "—";
  return `${delayMs} ms`;
}

// ---------------------------------------------------------------------------
// Painel de qualidade do share (desejo da UI; efetivo fixo no backend).
// ---------------------------------------------------------------------------

export interface QualityPanelProps extends QualitySelection {
  /** Só com share no ar os controles valem (perfil do share vivo). */
  shareLive: boolean;
  busy: boolean;
  /** Efetivo autoritativo do backend (null = nenhuma leitura ainda). */
  effective: EffectiveQuality | null;
  /** Codificador vivo (`videotoolbox`/`openh264`; null = sem leitura). */
  backend: string | null;
  /** Motivo do fallback software (null no hardware). */
  backendNote: string | null;
  /** Aplicação em voo (progresso do botão Aplicar). */
  applying: boolean;
  /** Último erro do comando, verbatim (já redatado no backend). */
  applyError: string | null;
  onResolution: (value: ResolutionSel) => void;
  onCustomW: (value: string) => void;
  onCustomH: (value: string) => void;
  onQuality: (value: QualitySel) => void;
  onCustomBitrate: (value: string) => void;
  onCustomFps: (value: string) => void;
  onApply: () => void;
}

export const QUALITY_DISABLED_REASON = "Inicie o Compartilhar para ajustar a qualidade.";

export function QualityPanel(props: QualityPanelProps) {
  const {
    shareLive, busy, effective, backend, backendNote, applying, applyError,
    resolution, onResolution, customW, onCustomW, customH, onCustomH,
    quality, onQuality, customBitrate, onCustomBitrate, customFps, onCustomFps, srcDims,
    onApply,
  } = props;
  const resolved = resolveDesired({
    resolution, customW, customH, quality, customBitrate, customFps, srcDims,
  });
  const valid = "profile" in resolved;
  const disabled = !shareLive || busy || applying || !valid;
  return (
    <div className="quality">
      {!shareLive ? (
        <p className="empty" role="status">{QUALITY_DISABLED_REASON}</p>
      ) : null}
      <fieldset disabled={disabled} aria-label="Perfil de qualidade">
        <div className="row">
          <div className="field">
            <label htmlFor="resolution">Resolução</label>
            <select
              id="resolution"
              value={resolution}
              onChange={(event) => onResolution(event.target.value as ResolutionSel)}
            >
              <option value="480p">480p · 854×480</option>
              <option value="720p">720p · 1280×720</option>
              <option value="1080p">1080p · 1920×1080</option>
              <option value="custom">Custom…</option>
            </select>
          </div>
          <div className="field">
            <label htmlFor="quality">Qualidade</label>
            <select
              id="quality"
              value={quality}
              onChange={(event) => onQuality(event.target.value as QualitySel)}
            >
              <option value="low">{QUALITY_PRESETS.low.label}</option>
              <option value="medium">{QUALITY_PRESETS.medium.label}</option>
              <option value="high">{QUALITY_PRESETS.high.label}</option>
              <option value="custom">CUSTOM…</option>
            </select>
          </div>
        </div>
        {resolution === "custom" ? (
          <div className="row">
            <div className="field">
              <label htmlFor="custom-w">Largura (px, par)</label>
              <input
                id="custom-w"
                value={customW}
                onChange={(event) => onCustomW(event.target.value)}
                placeholder="1280"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
            <div className="field">
              <label htmlFor="custom-h">Altura (px, par)</label>
              <input
                id="custom-h"
                value={customH}
                onChange={(event) => onCustomH(event.target.value)}
                placeholder="720"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
          </div>
        ) : null}
        {quality === "custom" ? (
          <div className="row">
            <div className="field">
              <label htmlFor="custom-bitrate">Bitrate (kbps)</label>
              <input
                id="custom-bitrate"
                value={customBitrate}
                onChange={(event) => onCustomBitrate(event.target.value)}
                placeholder="2000"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
            <div className="field">
              <label htmlFor="custom-fps">FPS</label>
              <input
                id="custom-fps"
                value={customFps}
                onChange={(event) => onCustomFps(event.target.value)}
                placeholder="30"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
          </div>
        ) : null}
      </fieldset>
      {valid ? (
        <p className="hint" data-testid="desired-profile">
          Desejado: {resolved.profile.w}×{resolved.profile.h} @{" "}
          {resolved.profile.bitrate_kbps} kbps · {resolved.profile.fps} fps
          {srcDims ? ` (fonte ${srcDims.w}×${srcDims.h})` : " (fonte desconhecida: sem teto de upscale)"}
        </p>
      ) : (
        <ul className="errors" role="alert">
          {resolved.errors.map((message) => (
            <li key={message}>{message}</li>
          ))}
        </ul>
      )}
      <div className="row">
        <button
          type="button"
          className="primary"
          onClick={onApply}
          disabled={disabled}
          title={
            !shareLive
              ? QUALITY_DISABLED_REASON
              : !valid
                ? "Corrija os erros do perfil custom."
                : "Aplica o perfil ao share vivo"
          }
        >
          {applying ? "Aplicando…" : "Aplicar qualidade"}
        </button>
      </div>
      {applyError ? (
        <p className="error" role="alert">{applyError}</p>
      ) : null}
      <p className="hint" data-testid="effective-profile">
        {effective
          ? `Efetivo no backend: ${formatEffective(effective)}`
          : "Efetivo no backend: ainda sem leitura — toque Atualizar."}
      </p>
      <p className="hint" data-testid="encode-backend">
        {formatBackend(backend, backendNote)}
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Painel do viewer por link (só contadores do backend; sem vídeo na web).
// ---------------------------------------------------------------------------

export interface ViewerLinksProps {
  /** Links com contadores (null = nenhuma amostra ainda). */
  links: LinkStats[] | null;
  /** Member ids com intenção de watch (eco local). */
  watching: string[];
}

export const WINDOW_HINT =
  "Na janela do vídeo: roda = zoom · arrastar = pan · F ou duplo-clique = tela cheia · Esc sai.";

export function ViewerLinksPanel({ links, watching }: ViewerLinksProps) {
  if (links === null) {
    return (
      <p className="empty" role="status">
        Sem amostra de contadores ainda — toque <strong>Atualizar</strong>.
      </p>
    );
  }
  if (links.length === 0) {
    return (
      <p className="empty" role="status">
        {watching.length > 0
          ? "Watch pedido, aguardando o link do backend — toque Atualizar."
          : "Nenhum link assistido. Use Assistir em um membro com share."}
      </p>
    );
  }
  return (
    <div className="viewer-links">
      {links.map((link) => (
        <article key={link.member} className="link-card" aria-label={`Link de ${link.member}`}>
          <header>
            <strong>{link.title || link.member}</strong>
            <span className="state">{link.codec}</span>
          </header>
          <dl className="diag link-stats">
            <div>
              <dt>FPS (apresentados/s)</dt>
              <dd>{formatFps(link.render_fps)}</dd>
            </div>
            <div>
              <dt>Delay</dt>
              <dd title={link.delay_note}>{formatDelayMs(link.delay_estimate_ms)}</dd>
            </div>
            <div>
              <dt>Bitrate medido</dt>
              <dd title={link.bitrate_note}>{formatBps(link.bitrate_bps)}</dd>
            </div>
            <div>
              <dt>Resolução atual</dt>
              <dd>{link.width}×{link.height}</dd>
            </div>
            <div>
              <dt>Decodificados</dt>
              <dd>{link.decoded}</dd>
            </div>
            <div>
              <dt>Apresentados</dt>
              <dd>{link.presented}</dd>
            </div>
            <div>
              <dt>Descartados</dt>
              <dd title={link.dropped_note}>{link.dropped}</dd>
            </div>
          </dl>
        </article>
      ))}
      <p className="hint">{WINDOW_HINT}</p>
    </div>
  );
}

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
  /** Controles de qualidade (desejo + efetivo + Aplicar; ver QualityPanel). */
  quality: QualityPanelProps;
  /** Contadores por link (null = nenhuma amostra ainda). */
  linkStats: LinkStats[] | null;
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
    busy, error, lastSignal, lastMedia, stats, quality, linkStats,
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

      <section className="panel" aria-label="Qualidade do share">
        <h2>Qualidade</h2>
        <QualityPanel {...quality} />
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
        <ViewerLinksPanel links={linkStats} watching={watching} />
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
