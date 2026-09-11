/**
 * Componentes puros (só props → markup) com o visual goDrinking2.
 * Estados puramente visuais (pin, mute-all, zoom/pan, modais, toast,
 * busca/filtro de áudio) são useState locais. Dados reais (roster,
 * snapshot, links, sources) chegam via props do App.tsx — lista vazia é
 * estado honesto, nunca mock. Idioma: só pt-BR.
 */

import { useEffect, useRef, useState } from "react";
import type {
  AudioApp,
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

export function watchingStillLive(watching: string[], entries: RoomMember[]): string[] {
  const live = new Set(entries.filter((entry) => entry.share).map((entry) => entry.id));
  return watching.filter((id) => live.has(id));
}

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

export function tileIsLive(iceConnected: boolean, presented: number): boolean {
  return iceConnected && presented > 0;
}

// ---------------------------------------------------------------------------
// Qualidade (espelha core/src/media.rs `QualityProfile`; aplicada via
// comando `set_quality`, efetivo autoritativo no snapshot/evento).
// ---------------------------------------------------------------------------

export type ResolutionSel = "480p" | "720p" | "1080p" | "5120x1440" | "1:1" | "custom";
export type QualitySel = "low" | "medium" | "high" | "custom";

export interface DesiredProfile {
  w: number;
  h: number;
  bitrate_kbps: number;
  fps: number;
}

export const RESOLUTION_DIMS: Record<Exclude<ResolutionSel, "custom" | "1:1">, { w: number; h: number }> = {
  "480p": { w: 854, h: 480 },
  "720p": { w: 1280, h: 720 },
  "1080p": { w: 1920, h: 1080 },
  "5120x1440": { w: 5120, h: 1440 },
};

const NATIVE_CAP = { w: 8192, h: 8192 };

function evenDim(value: number): number {
  return Math.max(2, value - (value % 2));
}

function usableSrcDims(dims: { w: number; h: number } | null): { w: number; h: number } | null {
  if (!dims || dims.w < 2 || dims.h < 2) return null;
  return dims;
}

export const QUALITY_PRESETS: Record<
  Exclude<QualitySel, "custom">,
  DesiredProfile & { label: string }
> = {
  low: { w: 854, h: 480, bitrate_kbps: 800, fps: 15, label: "LOW · 800 kbps · 15 fps" },
  medium: { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30, label: "MEDIUM · 2000 kbps · 30 fps" },
  high: { w: 1920, h: 1080, bitrate_kbps: 10000, fps: 60, label: "HIGH · 10000 kbps · 60 fps" },
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
  if (backend === "nvenc") return "Codificador: NVENC (hardware)";
  if (backend === "qsv") return "Codificador: Intel Quick Sync (hardware)";
  if (backend === "amf") return "Codificador: AMD AMF (hardware)";
  if (backend === "mfhw") return "Codificador: GPU (hardware)";
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
  if (value > 8192) return `${axis}: máximo 8192 px.`;
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
 * teto do backend (8192) e o próprio backend normaliza na borda.
 */
export function resolveDesired(
  selection: QualitySelection,
): { profile: DesiredProfile } | { errors: string[] } {
  const errors: string[] = [];
  let w = 0;
  let h = 0;
  const srcDims = usableSrcDims(selection.srcDims);
  if (selection.resolution === "custom") {
    const wError = validateCustomDim(selection.customW, "largura");
    const hError = validateCustomDim(selection.customH, "altura");
    if (wError) errors.push(wError);
    if (hError) errors.push(hError);
    w = parseUint(selection.customW) ?? 0;
    h = parseUint(selection.customH) ?? 0;
    if (!wError && !hError && srcDims) {
      const { w: srcW, h: srcH } = srcDims;
      if (w > srcW || h > srcH)
        errors.push(`sem upscale além da fonte (${srcW}×${srcH}).`);
    }
  } else if (selection.resolution === "1:1") {
    if (srcDims) {
      w = evenDim(srcDims.w);
      h = evenDim(srcDims.h);
    } else {
      w = NATIVE_CAP.w;
      h = NATIVE_CAP.h;
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
  /** Prefixo de ids (dois painéis no DOM: TX-config e modal Compartilhar). */
  idPrefix?: string;
  /** Staging no popup de fonte: edita o desejo sem share vivo; Aplicar some. */
  staging?: boolean;
  busy: boolean;
  /** Efetivo autoritativo do backend (null = nenhuma leitura ainda). */
  effective: EffectiveQuality | null;
  /** Codificador vivo (`videotoolbox`/`nvenc`/`openh264`; null = sem leitura). */
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
    onApply, staging = false, idPrefix = "",
  } = props;
  const resolved = resolveDesired({
    resolution, customW, customH, quality, customBitrate, customFps, srcDims,
  });
  const valid = "profile" in resolved;
  const fid = (name: string) => `${idPrefix}${name}`;
  // Staging (popup de fonte) libera o desejo sem share vivo. Live: só com
  // share. Sem `!valid` no fieldset: CUSTOM vazio precisa poder ser digitado.
  const fieldDisabled = (!staging && !shareLive) || busy || applying;
  const disabled = fieldDisabled || !valid;
  return (
    <div className="quality">
      {!shareLive && !staging ? (
        <p className="empty" role="status">{QUALITY_DISABLED_REASON}</p>
      ) : null}
      <fieldset disabled={fieldDisabled} aria-label="Perfil de qualidade">
        <div className="row">
          <div className="field">
            <label htmlFor={fid("resolution")}>Resolução</label>
            <select
              id={fid("resolution")}
              value={resolution}
              onChange={(event) => onResolution(event.target.value as ResolutionSel)}
            >
              <option value="480p">480p · 854×480</option>
              <option value="720p">720p · 1280×720</option>
              <option value="1080p">1080p · 1920×1080</option>
              <option value="5120x1440">5120×1440</option>
              <option value="1:1">1:1 · nativo da fonte</option>
              <option value="custom">Custom…</option>
            </select>
          </div>
          <div className="field">
            <label htmlFor={fid("quality")}>Qualidade</label>
            <select
              id={fid("quality")}
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
              <label htmlFor={fid("custom-w")}>Largura (px, par)</label>
              <input
                id={fid("custom-w")}
                value={customW}
                onChange={(event) => onCustomW(event.target.value.replace(/\D/g, ""))}
                placeholder="1280"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
            <div className="field">
              <label htmlFor={fid("custom-h")}>Altura (px, par)</label>
              <input
                id={fid("custom-h")}
                value={customH}
                onChange={(event) => onCustomH(event.target.value.replace(/\D/g, ""))}
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
              <label htmlFor={fid("custom-bitrate")}>Bitrate (kbps)</label>
              <input
                id={fid("custom-bitrate")}
                value={customBitrate}
                onChange={(event) => onCustomBitrate(event.target.value.replace(/\D/g, ""))}
                placeholder="2000"
                inputMode="numeric"
                autoComplete="off"
              />
            </div>
            <div className="field">
              <label htmlFor={fid("custom-fps")}>FPS</label>
              <input
                id={fid("custom-fps")}
                value={customFps}
                onChange={(event) => onCustomFps(event.target.value.replace(/\D/g, ""))}
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
      ) : null}
      {staging ? (
        <p className="hint">Este perfil entra junto com Compartilhar.</p>
      ) : (
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
      )}
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
  "Na janela do vídeo: arraste as bordas para redimensionar · roda = zoom · arrastar = pan · F ou duplo-clique = tela cheia · Esc sai.";

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
// Utilidades visuais locais.
// ---------------------------------------------------------------------------

const hueOfNickname = (nickname: string): number => {
  let h = 0;
  for (let i = 0; i < nickname.length; i++) h = (h * 31 + nickname.charCodeAt(i)) % 360;
  return h;
};

const initialOf = (nickname: string): string =>
  nickname.trim().charAt(0).toUpperCase() || "?";

function useToast(): { toast: string | null; show: (message: string) => void } {
  const [toast, setToast] = useState<string | null>(null);
  useEffect(() => {
    if (toast === null) return;
    const timer = setTimeout(() => setToast(null), 2200);
    return () => clearTimeout(timer);
  }, [toast]);
  return { toast, show: setToast };
}

// ---------------------------------------------------------------------------
// Tela inicial: criar / entrar (lobby 2 panes).
// ---------------------------------------------------------------------------

export interface HomeProps {
  /** Legado (mantido p/ compat): servidor rendezvous, hoje sempre DEFAULT_SERVER. */
  server?: string;
  onServer?: (value: string) => void;
  /** Legado (mantido p/ compat): a home fiel não tem tabs. */
  tab?: "create" | "join";
  onTab?: (tab: "create" | "join") => void;
  /** Apelido exibido no card "Seu Nick" da home; vazio cai no default. */
  nickname?: string;
  onNickname?: (value: string) => void;  password: string;
  onPassword: (value: string) => void;
  /** Código digitado no pane Entrar (6 letras/números). */
  code: string;
  onCode: (value: string) => void;
  busy: boolean;
  error: string | null;
  onCreate: () => void;
  onJoin: () => void;
  /** Código devolvido pelo backend (ignorado na home; só aparece na sala). */
  createdCode?: string | null;
  /** Selo visual discreto do modo navegador (mock, sem Tauri). Só visual. */
  mock?: boolean;
}

export function HomeScreen(props: HomeProps) {
  const {
    server = "", onServer,
    nickname = "", onNickname,
    password, onPassword, code, onCode, busy, error, onCreate, onJoin,
    mock = false,
  } = props;
  const { toast } = useToast();
  return (
    <div className="app" data-state="lobby" data-hook="room-shell">
      <header className="topbar">
        <div className="brand">
          <img className="brand-logo" src="/logo.png" alt="goDrinking" width={22} height={22} />
          <strong>goDrinking</strong>
          <span className="room-pill" data-hook="room-presence">
            <span className="pulse" aria-hidden="true" />
            <span>Nenhuma sala</span>
          </span>
          {mock ? (
            <span
              className="mock-tag"
              data-testid="mock-tag"
              title="Sem Tauri: dados locais de demonstração"
              style={{
                fontSize: 11,
                opacity: 0.75,
                border: "1px solid currentColor",
                borderRadius: 999,
                padding: "1px 8px",
                marginLeft: 8,
                whiteSpace: "nowrap",
              }}
            >
              MODO NAVEGADOR (mock)
            </span>
          ) : null}
        </div>
      </header>

      <div className="layout">
        <main className="stage-wrap">
          <div className="lobby" data-hook="room-join">
            <div className="lobby-card lobby-card--wide">
              <h2>Crie ou entre em uma Sala</h2>
              <p>Salas com código + senha. O compartilhamento só aparece dentro da sala.</p>

              <section className="lobby-pane lobby-name" aria-label="Servidor">
                <label className="field" htmlFor="server">
                  <span>Servidor</span>
                  <input
                    className="input"
                    id="server"
                    type="text"
                    value={server}
                    onChange={(event) => onServer?.(event.target.value)}
                    placeholder="http://127.0.0.1:18790"
                    autoComplete="off"
                    spellCheck={false}
                    disabled={busy}
                  />
                </label>
              </section>

              <section className="lobby-pane lobby-name" aria-label="Seu Nick (só pessoas na sala conseguem ver)">
                <label className="field" htmlFor="nickname">
                  <span>Seu Nick (só pessoas na sala conseguem ver)</span>
                  <input
                    className="input"
                    id="nickname"
                    type="text"
                    value={nickname}
                    onChange={(event) => onNickname?.(event.target.value)}
                    maxLength={24}
                    placeholder="Como te chamam na sala"
                    autoComplete="nickname"
                    disabled={busy}
                  />
                </label>
              </section>

              <div className="lobby-cols">
                <section className="lobby-pane" aria-label="Criar sala">
                  <h3>Criar sala</h3>
                  <p className="pane-desc">Defina a senha e receba o código para convidar.</p>
                  <label className="field" htmlFor="createPassword">
                    <span>Senha da sala</span>
                    <input
                      className="input"
                      id="createPassword"
                      type="password"
                      value={password}
                      onChange={(event) => onPassword(event.target.value)}
                      autoComplete="new-password"
                      aria-label="Senha da sala para criar"
                      maxLength={128}
                      placeholder="Ex.: café-com-leite"
                      disabled={busy}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") onCreate();
                      }}
                    />
                  </label>
                  <button
                    type="button"
                    className="btn primary big"
                    id="createBtn"
                    onClick={onCreate}
                    disabled={busy}
                  >
                    {busy ? "Criando…" : "Criar sala"}
                  </button>
                </section>
                <section className="lobby-pane" aria-label="Entrar na sala">
                  <h3>Entrar</h3>
                  <p className="pane-desc">Digite o código + senha para entrar.</p>
                  <label className="field" htmlFor="joinCode">
                    <span>Código (6 letras/números)</span>
                    <input
                      className="input code"
                      id="joinCode"
                      type="text"
                      value={code}
                      onChange={(event) => onCode(event.target.value.toUpperCase())}
                      autoComplete="one-time-code"
                      aria-label="Código da sala"
                      maxLength={6}
                      spellCheck={false}
                      placeholder="Ex.: K7Q9XA"
                      disabled={busy}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") onJoin();
                      }}
                    />
                  </label>
                  <label className="field" htmlFor="joinPassword">
                    <span>Senha da sala</span>
                    <input
                      className="input"
                      id="joinPassword"
                      type="password"
                      value={password}
                      onChange={(event) => onPassword(event.target.value)}
                      autoComplete="current-password"
                      aria-label="Senha da sala para entrar"
                      maxLength={128}
                      placeholder="Senha combinada"
                      disabled={busy}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") onJoin();
                      }}
                    />
                  </label>
                  <button
                    type="button"
                    className="btn primary big"
                    id="joinBtn"
                    onClick={onJoin}
                    disabled={busy}
                  >
                    {busy ? "Entrando…" : "Entrar"}
                  </button>
                </section>
              </div>
              {error ? (
                <p className="lobby-error" id="lobbyError" role="alert">{error}</p>
              ) : null}
              <small id="lobbyNote">Tela direto de um PC para outro. O servidor só apresenta, nunca vê o vídeo. <span style={{ opacity: 0.6, fontSize: 11 }}>· v0.7.4</span></small>
            </div>
          </div>
        </main>
      </div>

      <div className={`toast${toast ? " show" : ""}`} role="status">{toast}</div>
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
  /** Permissão de captura negada: bloco honesto com o caminho manual. */
  sourcesDenied?: boolean;
  /** Capacidades desta build (desabilita com motivo). */
  caps: CapabilitySet | null;
  onListSources: () => void;
  /** Thumbs PNG (data URL) por "kind:id", cache lazy do App. Ausente = gradiente. */
  previews?: Record<string, string>;
  /** Pede thumbs da aba visível (App busca lazy com cache; mock ignora). */
  onPreviewsVisible?: (items: SourceInfo[]) => void;
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
  audioApps?: AudioApp[];
  audioExcluded?: string[];
  onToggleAudioExclude?: (id: string) => void;
  /** Selo visual discreto do modo navegador (mock, sem Tauri). Só visual. */
  mock?: boolean;
}

const isSelf = (member: RoomMember, selfId: string | null, selfNickname: string): boolean =>
  (selfId !== null && member.id === selfId) ||
  (selfId === null && member.nickname === selfNickname);

interface TileProps {
  member: RoomMember;
  self: boolean;
  wantsWatch: boolean;
  connected: boolean;
  pinned: boolean;
  muted: boolean;
  busy: boolean;
  onWatch: () => void;
  onUnwatch: () => void;
  onPin: () => void;
  onToggleMute: () => void;
  showToast: (message: string) => void;
}

function Tile(props: TileProps) {
  const { member, self, wantsWatch, connected, pinned, muted, busy, onWatch, onUnwatch, onPin, onToggleMute, showToast } = props;
  const [zoom, setZoom] = useState({ s: 1, x: 0, y: 0 });
  const hue = hueOfNickname(member.nickname);
  const toggleFull = (host: HTMLElement | null): void => {
    try {
      if (document.fullscreenElement) {
        void document.exitFullscreen();
      } else {
        void host?.requestFullscreen?.();
      }
    } catch {
      showToast("Tela cheia indisponível aqui.");
    }
  };
  return (
    <article
      className="tile"
      data-hook="tile-stream"
      tabIndex={0}
      aria-label={`Transmissão de ${member.nickname}`}
    >
      <div
        className="viewport"
        onWheel={(event) => {
          const next = Math.min(3, Math.max(1, zoom.s + (event.deltaY < 0 ? 0.15 : -0.15)));
          const s = Math.round(next * 100) / 100;
          setZoom(s === 1 ? { s: 1, x: 0, y: 0 } : { ...zoom, s });
        }}
        onDoubleClick={() => {
          setZoom({ s: 1, x: 0, y: 0 });
          showToast("Zoom resetado.");
        }}
      >
        <div
          className="tile-art"
          style={{
            background: `linear-gradient(135deg, hsl(${hue} 45% 32%), hsl(${hue} 45% 12%))`,
            transform: `translate(${zoom.x}px, ${zoom.y}px) scale(${zoom.s})`,
          }}
          aria-hidden="true"
        >
          {initialOf(member.nickname)}
        </div>
        {!member.share ? (
          <div className="watch-center">
            <span className="watch-note">Sem transmissão</span>
          </div>
        ) : connected ? null : (
          <div className="watch-center">
            <button
              type="button"
              className="watch-btn"
              data-testid={`tile-view-${member.id}`}
              onClick={(event) => {
                event.stopPropagation();
                if (wantsWatch) onUnwatch();
                else onWatch();
              }}
              disabled={busy}
            >
              {wantsWatch ? "Parar de ver" : "Ver"}
            </button>
            <span className="watch-note">
              {wantsWatch ? "Conectando…" : "Scroll = zoom · Duplo-clique = resetar"}
            </span>
          </div>
        )}
      </div>
      {connected ? (
        <div className="badge-live"><i />AO VIVO</div>
      ) : null}
      <div className="tile-top">
        <span className="name-tag">
          {member.nickname}
          {member.master ? <span className="leader">LÍDER</span> : null}
          {self ? <span className="leader">VOCÊ</span> : null}
        </span>
        {connected ? <span className="live-stats">conectado</span> : null}
      </div>
      <div className="tile-controls">
        <button type="button" className="tbtn pin" onClick={onPin} title="Fixar vídeo">
          {pinned ? "Desafixar" : "Fixar"}
        </button>
        <button
          type="button"
          className="tbtn full"
          title="Tela cheia"
          onClick={(event) => toggleFull(event.currentTarget.closest(".tile"))}
        >
          Ampliar
        </button>
        <button
          type="button"
          className={`tbtn mute warn${muted ? " on" : ""}`}
          title="Mudo"
          onClick={onToggleMute}
        >
          {muted ? "Mudo" : "Som"}
        </button>
      </div>
    </article>
  );
}

export function RoomScreen(props: RoomProps) {
  const {
    roomCode, snapshot, roster, selfId, selfNickname, watching,
    source, onSource, sources, sourcesError, sourcesDenied = false, caps, onListSources,
    previews, onPreviewsVisible,
    busy, error, lastSignal, lastMedia, quality, linkStats,
    onRefresh, onLeave, onShare, onStopShare, onWatch, onUnwatch,
    audioApps = [], audioExcluded = [], onToggleAudioExclude,
    mock = false,
  } = props;
  const watchingSet = new Set(watching);
  const shareState = snapshot?.share.state ?? null;
  const sharing = shareState === "live" || shareState === "starting";
  const links = snapshot?.links ?? [];
  const liveWatchers = snapshot?.watchers ?? [];
  const connectedIds = new Set(
    links.filter((link) => link.state === "connected").map((link) => link.watcher),
  );
  const presentedByMember = new Map(
    (linkStats ?? []).map((link) => [link.member, link.presented] as const),
  );
  const liveFor = (member: RoomMember): boolean => {
    const ice = connectedIds.has(member.nickname) || connectedIds.has(member.id);
    const presented =
      presentedByMember.get(member.id) ?? presentedByMember.get(member.nickname) ?? 0;
    return tileIsLive(ice, presented);
  };

  // Estados puramente visuais.
  const [pinnedId, setPinnedId] = useState<string | null>(null);
  const [muteAll, setMuteAll] = useState(false);
  const [mutedIds, setMutedIds] = useState<Set<string>>(new Set());
  const [shareOpen, setShareOpen] = useState(false);
  const [txOpen, setTxOpen] = useState(false);
  const [shareTab, setShareTab] = useState<"screens" | "apps">("screens");
  const [audioQuery, setAudioQuery] = useState("");
  const [audioSoundOnly, setAudioSoundOnly] = useState(false);
  const { toast, show } = useToast();

  const sharingMembers = roster.filter((member) => member.share);
  // Palco mostra SOMENTE quem compartilha; sidebar continua com todo mundo.
  const pinned = pinnedId ? sharingMembers.find((member) => member.id === pinnedId) ?? null : null;
  const gridMembers = pinned ? [pinned] : sharingMembers;
  const stripMembers = pinned ? sharingMembers.filter((member) => member.id !== pinned.id) : [];

  const say = (message: string): void => show(message);

  const openShare = (): void => {
    onListSources();
    setShareOpen(true);
  };
  const closeShare = (): void => setShareOpen(false);

  const handleShareMain = (): void => {
    if (sharing) {
      onStopShare();
      say("Compartilhamento parado.");
    } else {
      openShare();
    }
  };

  // Selecionar-confirmar: o clique só seleciona (highlight .sel via `source`);
  // só o botão Compartilhar inicia o share e fecha o modal.
  const pickSource = (kind: "display" | "window", id: string, name: string): void => {
    onSource(`${kind}:${id}`);
    say(`Fonte escolhida: ${name}. Toque Compartilhar para ir ao ar.`);
  };

  const copyCode = (): void => {
    if (!roomCode) {
      say("Nenhum código para copiar ainda.");
      return;
    }
    try {
      void navigator.clipboard?.writeText(roomCode);
      say(`Código copiado: ${roomCode}`);
    } catch {
      say(`Código da sala: ${roomCode}`);
    }
  };

  const toggleMuteId = (id: string, name: string): void => {
    setMutedIds((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
    say(`${name} ${mutedIds.has(id) ? "com som." : "silenciado."}`);
  };

  const visibleSources = sources.filter((item) =>
    shareTab === "screens" ? item.kind === "display" : item.kind === "window",
  );
  // Previews lazy do modal: ao abrir ou trocar de aba/lista, pede os thumbs
  // da aba visível com debounce (o App cacheia por kind:id; sem thumb, o
  // gradiente continua). Callback via ref para não refogar o debounce.
  const previewsCb = useRef(onPreviewsVisible);
  previewsCb.current = onPreviewsVisible;
  useEffect(() => {
    if (!shareOpen) return;
    const timer = setTimeout(() => {
      previewsCb.current?.(visibleSources);
    }, 180);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shareOpen, shareTab, sources]);
  const audioFiltered = audioApps.filter((item) => {
    if (audioSoundOnly && !item.emitting_audio) return false;
    if (audioQuery && !item.name.toLowerCase().includes(audioQuery.toLowerCase())) return false;
    return true;
  });

  return (
    <div className="app" data-state="inside" data-hook="room-shell">
      <header className="topbar">
        <div className="brand">
          <img className="brand-logo" src="/logo.png" alt="goDrinking" width={22} height={22} />
          <strong>goDrinking</strong>
          <span className="room-pill" data-hook="room-presence">
            <span className="pulse" aria-hidden="true" />
            <span>Sala {roomCode ?? "sem código ainda"}</span>
            <em>· {roster.length} pessoa(s)</em>
          </span>
          {roomCode ? (
            <button
              type="button"
              className="btn ghost small"
              onClick={copyCode}
              title="Copia o código da sala"
            >
              Copiar
            </button>
          ) : null}
          {mock ? (
            <span
              className="mock-tag"
              data-testid="mock-tag"
              title="Sem Tauri: dados locais de demonstração"
              style={{
                fontSize: 11,
                opacity: 0.75,
                border: "1px solid currentColor",
                borderRadius: 999,
                padding: "1px 8px",
                marginLeft: 8,
                whiteSpace: "nowrap",
              }}
            >
              MODO NAVEGADOR (mock)
            </span>
          ) : null}
        </div>
        <div className="top-actions">
          <button type="button" className="btn danger-b" onClick={onLeave} disabled={busy}>
            Sair da sala
          </button>
        </div>
      </header>

      <div className="layout">
        <main className="stage-wrap">
          <div className="stage-head">
            <div>
              <p>
                {sharingMembers.length} compartilhando · {watching.length} assistindo
              </p>
            </div>
            <div className="stage-head-actions">
              <div className="stage-hint">
                Scroll = zoom · Arrastar = mover · Duplo-clique = resetar
              </div>
              <button
                type="button"
                className="btn ghost small"
                onClick={() => {
                  onRefresh();
                  say("Sala atualizada.");
                }}
                disabled={busy}
                title="Lê o snapshot do backend agora"
              >
                {busy ? "Atualizando…" : "Atualizar"}
              </button>
            </div>
          </div>

          {error ? (
            <p className="lobby-error" role="alert">{error}</p>
          ) : null}

          <section className="stage" aria-label="Transmissões da sala">
            {sharingMembers.length === 0 ? (
              <div className="empty-stage">Sem transmissões</div>
            ) : (
              <>
                <div className={`tiles${pinned ? " solo" : ""}`} data-hook="tile-grid">
                  {gridMembers.map((member) => {
                    const self = isSelf(member, selfId, selfNickname);
                    return (
                      <Tile
                        key={member.id}
                        member={member}
                        self={self}
                        wantsWatch={watchingSet.has(member.id)}
                        connected={liveFor(member)}
                        pinned={pinnedId === member.id}
                        muted={muteAll || mutedIds.has(member.id)}
                        busy={busy}
                        onWatch={() => {
                          onWatch(member.id);
                          say(`Pedindo para assistir ${member.nickname}…`);
                        }}
                        onUnwatch={() => {
                          onUnwatch(member.id);
                          say(`Parou de ver ${member.nickname}.`);
                        }}
                        onPin={() => {
                          setPinnedId((current) => (current === member.id ? null : member.id));
                          say(pinnedId === member.id
                            ? "Vídeo desafixado — volta ao grid."
                            : `${member.nickname} fixado na área principal.`);
                        }}
                        onToggleMute={() => toggleMuteId(member.id, member.nickname)}
                        showToast={say}
                      />
                    );
                  })}
                </div>
                {pinned ? (
                  <>
                    <p className="strip-label">Na sala agora — clique em desafixar para voltar ao grid</p>
                    <div className="strip">
                      {stripMembers.map((member) => {
                        const self = isSelf(member, selfId, selfNickname);
                        return (
                          <Tile
                            key={member.id}
                            member={member}
                            self={self}
                            wantsWatch={watchingSet.has(member.id)}
                            connected={liveFor(member)}
                            pinned={false}
                            muted={muteAll || mutedIds.has(member.id)}
                            busy={busy}
                            onWatch={() => onWatch(member.id)}
                            onUnwatch={() => onUnwatch(member.id)}
                            onPin={() => setPinnedId(member.id)}
                            onToggleMute={() => toggleMuteId(member.id, member.nickname)}
                            showToast={say}
                          />
                        );
                      })}
                    </div>
                  </>
                ) : null}
              </>
            )}
          </section>

          <footer className="controls" data-hook="room-controls">
            <button
              type="button"
              className={`ctl${muteAll ? " muted" : ""}`}
              onClick={() => {
                setMuteAll((current) => !current);
                say(muteAll ? "Áudio geral ativado." : "Tudo silenciado.");
              }}
              title="Silenciar tudo"
            >
              <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2"><path d="M11 5 6 9H2v6h4l5 4V5z" /><path d="M22 9l-6 6M16 9l6 6" /></svg>
              <span>Silenciar tudo</span>
            </button>
            <button
              type="button"
              className={`ctl accent${sharing ? " danger" : ""}`}
              data-hook="share-open"
              onClick={handleShareMain}
              disabled={busy}
            >
              <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2"><rect x="2" y="4" width="20" height="13" rx="2" /><path d="M8 21h8M12 17v4" /></svg>
              <span>{sharing ? (busy ? "Parando…" : "Parar de compartilhar") : (busy ? "Iniciando…" : "Compartilhar")}</span>
            </button>
            <button
              type="button"
              className="ctl"
              data-hook="share-config"
              onClick={() => setTxOpen(true)}
              title="Configurar transmissão"
            >
              <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2"><path d="M4 8h10M18 8h2M4 16h4M12 16h8" /><circle cx="16" cy="8" r="2" /><circle cx="10" cy="16" r="2" /></svg>
              <span>Configurar Transmissão</span>
            </button>
          </footer>

          <section className="card" aria-label="Links de mídia">
            <div className="card-head">
              <h2>Links · {links.length}</h2>
            </div>
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
          </section>

          <section className="card" aria-label="Diagnóstico">
            <div className="card-head">
              <h2>Diagnóstico</h2>
            </div>
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
                <dt>Codificador</dt>
                <dd data-testid="diag-backend">{formatBackend(quality.backend, quality.backendNote)}</dd>
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
        </main>

        <aside className="side">
          <section className="card" data-hook="room-presence">
            <div className="card-head">
              <h2>Pessoas <span className="count">{roster.length}</span></h2>
              <span className="mini">{sharingMembers.length} compartilhando</span>
            </div>
            {roster.length <= 1 ? (
              <p className="card-note">Só você aqui.</p>
            ) : null}
            {roster.length === 0 ? (
              <ul className="people roster">
                <li className="person ghost">
                  <span className="who">
                    <small>Nenhum membro visível ainda. O roster chega pelo evento do servidor — aguarde um instante ou toque <strong>Atualizar</strong>.</small>
                  </span>
                </li>
              </ul>
            ) : (
              <ul className="people roster">
                {roster.map((member) => {
                  const self = isSelf(member, selfId, selfNickname);
                  const wantsWatch = watchingSet.has(member.id);
                  const hue = hueOfNickname(member.nickname);
                  return (
                    <li key={member.id} className={`person${self ? " is-self" : ""}`}>
                      <span className="avatar" style={{ ["--h" as string]: hue }}>
                        {initialOf(member.nickname)}
                      </span>
                      <span className="who">
                        <b>
                          <span className="member-name">
                            {member.master ? <span title="Master">♛ </span> : null}
                            {member.nickname}
                            {self ? <small> (você)</small> : null}
                          </span>
                          {member.master && !self ? <span className="leader-tag">LÍDER</span> : null}
                        </b>
                        <small>{member.share ? "Compartilhando" : wantsWatch ? "Assistindo" : "Na sala"}</small>
                      </span>
                      <span className="badges">
                        {member.share ? <span className="badge live">compartilhando</span> : null}
                        {wantsWatch ? <span className="badge">pedido de watch</span> : null}
                      </span>
                      <span className={`dot ${member.share ? "live" : "watch"}`} aria-hidden="true" />
                      {!self && member.share ? (
                        wantsWatch ? (
                          <button
                            type="button"
                            data-testid="roster-watch"
                            onClick={() => {
                              onUnwatch(member.id);
                              say(`Parou de ver ${member.nickname}.`);
                            }}
                            disabled={busy}
                          >
                            Parar de ver
                          </button>
                        ) : (
                          <button
                            type="button"
                            data-testid="roster-watch"
                            onClick={() => {
                              onWatch(member.id);
                              say(`Pedindo para assistir ${member.nickname}…`);
                            }}
                            disabled={busy}
                          >
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

          <section className="card audio-card" data-hook="audio-policy">
            <div className="card-head">
              <h2>Ignorar Áudio de Apps</h2>
            </div>
            <div className="audio-tools">
              <input
                className="input small"
                type="search"
                autoComplete="off"
                aria-label="Buscar aplicativo"
                placeholder="Buscar app…"
                value={audioQuery}
                onChange={(event) => setAudioQuery(event.target.value)}
                disabled={!sharing}
              />
              <button
                type="button"
                className="btn ghost small"
                aria-pressed={audioSoundOnly}
                disabled={!sharing}
                onClick={() => setAudioSoundOnly((value) => !value)}
              >
                App com Som
              </button>
            </div>
            {!sharing ? (
              <p className="card-desc">Disponível durante o compartilhamento de tela.</p>
            ) : caps && !caps.app_audio.supported ? (
              <p className="card-desc">{caps.app_audio.reason}</p>
            ) : (
              <p className="card-desc">
                Quem assiste não ouve os apps marcados. Ex.: ignore o Discord para o grupo não se ouvir.
              </p>
            )}
            <div className="audio-list">
              {sharing && audioFiltered.length === 0 ? (
                <p className="empty-sources">Nenhum app encontrado.</p>
              ) : null}
              {sharing ? (
                <ul className="apps">
                  {audioFiltered.map((app) => {
                    const excluded = audioExcluded.includes(app.id);
                    return (
                      <li key={`${app.pid}-${app.id}`}>
                        <button
                          type="button"
                          className={`app-row${excluded ? " sel" : ""}`}
                          aria-pressed={excluded}
                          onClick={() => onToggleAudioExclude?.(app.id)}
                        >
                          <span className="app-ico" aria-hidden="true">
                            {app.name.slice(0, 1).toUpperCase()}
                          </span>
                          <span className="who">
                            <b>{app.name}</b>
                            {app.emitting_audio ? <small>com som</small> : null}
                          </span>
                          <span className={`snd${app.emitting_audio ? " on" : " off"}`} aria-hidden="true">
                            <i /><i /><i />
                          </span>
                        </button>
                      </li>
                    );
                  })}
                </ul>
              ) : null}
            </div>
          </section>

          <p className="foot-note">
            Cada watch abre uma janela nativa com o vídeo. O estado acima é o real do backend.
          </p>
        </aside>
      </div>

      {/* Modal Compartilhar — sempre no DOM (hidden quando fechado) */}
      <div className="modal-back" hidden={!shareOpen} data-hook="share-enumeration">
        <div className="modal" role="dialog" aria-modal="true" aria-label="Compartilhar tela">
          <div className="modal-head">
            <h2>Compartilhar</h2>
            <button type="button" className="icon-btn" aria-label="Fechar" onClick={closeShare}>
              ✕
            </button>
          </div>
          <div className="tabs">
            <button
              type="button"
              className={shareTab === "screens" ? "tab active" : "tab"}
              onClick={() => setShareTab("screens")}
            >
              Telas
            </button>
            <button
              type="button"
              className={shareTab === "apps" ? "tab active" : "tab"}
              onClick={() => setShareTab("apps")}
            >
              Aplicativos
            </button>
          </div>
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
                className="input"
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
                <option value={`${sourceKindOf(source)}:`}>Escolha…</option>
                {sources
                  .filter((item) => item.kind === sourceKindOf(source))
                  .map((item) => (
                    <option key={`${item.kind}:${item.id}`} value={`${item.kind}:${item.id}`}>
                      {item.name}
                    </option>
                  ))}
              </select>
              <div className="row">
                <button type="button" className="btn ghost small" onClick={onListSources} disabled={busy || sharing}>
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
          <div className="source-grid" style={{ marginTop: 12 }}>
            {visibleSources.length === 0 ? (
              <p className="empty-sources">
                Sem fontes ainda.
                <br />
                <button
                  type="button"
                  className="btn ghost small"
                  style={{ marginTop: 8 }}
                  onClick={onListSources}
                  disabled={busy || sharing}
                >
                  Listar telas
                </button>
                <span className="hint" style={{ display: "block", marginTop: 8 }}>
                  A primeira listagem pode pedir permissão ao sistema.
                </span>
                {sourcesDenied ? (
                  <span className="error" role="alert" style={{ display: "block", marginTop: 8 }}>
                    {sourcesError ?? "Sem permissão de Gravação de Tela."}
                    <span className="hint" style={{ display: "block", marginTop: 4 }}>
                      Caminho manual: Ajustes → Privacidade e Segurança →
                      Gravação de Tela (ative o GoLive) e toque Listar telas
                      de novo.
                    </span>
                  </span>
                ) : sourcesError ? (
                  <span className="error" role="alert" style={{ display: "block", marginTop: 8 }}>
                    {sourcesError}
                  </span>
                ) : null}
              </p>
            ) : (
              visibleSources.map((item) => (
                <button
                  key={`${item.kind}:${item.id}`}
                  type="button"
                  className={`source${source === `${item.kind}:${item.id}` ? " sel" : ""}`}
                  onClick={() => pickSource(item.kind, item.id, item.name)}
                >
                  {(() => {
                    const thumb =
                      previews?.[`${item.kind}:${item.id}`] ?? item.thumbnail ?? null;
                    return thumb ? (
                      <span className="thumb" aria-hidden="true">
                        <img
                          src={thumb}
                          alt=""
                          aria-hidden="true"
                          draggable={false}
                          style={{
                            width: "100%",
                            height: "100%",
                            objectFit: "cover",
                            display: "block",
                          }}
                        />
                      </span>
                    ) : (
                      <span
                        className="thumb"
                        style={{
                          background: `linear-gradient(135deg, hsl(${hueOfNickname(item.id)} 45% 35%), hsl(${hueOfNickname(item.id)} 45% 12%))`,
                        }}
                      >
                        FONTE
                      </span>
                    );
                  })()}
                  <b>{item.name}</b>
                  <small>{item.w} × {item.h}</small>
                </button>
              ))
            )}
          </div>
          <p className="modal-note">
            A enumeração vem do backend (<code>list_sources</code>); sem
            permissão, a listagem erra honesto acima — nunca volta vazia
            silenciosa.
          </p>
          <QualityPanel {...quality} staging idPrefix="share-" />
          <div className="modal-foot">
            <button type="button" className="btn ghost" onClick={closeShare}>
              Cancelar
            </button>
            <button
              type="button"
              className="btn primary"
              onClick={() => {
                onShare();
                setShareOpen(false);
                say("Iniciando compartilhamento…");
              }}
              disabled={busy}
            >
              {busy ? "Iniciando…" : "Compartilhar"}
            </button>
          </div>
        </div>
      </div>

      {/* Modal TX Config — sempre no DOM (hidden quando fechado) */}
      <div className="modal-back" hidden={!txOpen} data-hook="share-config">
        <div className="modal small" role="dialog" aria-modal="true" aria-label="Configurar transmissão">
          <div className="modal-head">
            <h2>Configurar Transmissão</h2>
            <button type="button" className="icon-btn" aria-label="Fechar" onClick={() => setTxOpen(false)}>
              ✕
            </button>
          </div>
          <p className="modal-note top">
            Padrão automático: resolução máxima da fonte com bitrate saudável.
          </p>
          <QualityPanel {...quality} />
          <div className="modal-foot">
            <button type="button" className="btn ghost" onClick={() => setTxOpen(false)}>
              Fechar
            </button>
          </div>
        </div>
      </div>

      <div className={`toast${toast ? " show" : ""}`} role="status">{toast}</div>
    </div>
  );
}
