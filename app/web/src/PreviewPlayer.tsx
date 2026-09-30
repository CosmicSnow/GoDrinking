/**
 * Preview ao vivo de UMA fonte listada (modal Compartilhar).
 *
 * - Roda só com `active` (modal aberto + app em foco + fonte com id real).
 *   Sem Tauri (mock/navegador): placeholder honesto, nenhum invoke.
 * - Frames GLP2/format-0 chegam no Channel e desenham no canvas com o
 *   mesmo renderer do player (sem acks: latest-only, best-effort).
 * - Erro do backend (permissão, fonte sumida, câmera ocupada) vira nota
 *   via `onError`; o thumb estático do modal continua valendo.
 * - O token sobe via `onToken` para o dono parar antes de compartilhar
 *   (a mesma câmera não abre duas vezes).
 */

import { useEffect, useRef, useState } from "react";
import { deliverPlayerFrame } from "./playerFrameDelivery";
import {
  Channel,
  isTauri,
  previewStart,
  previewStop,
  type PreviewKind,
} from "./api";
import { createPlayerRenderer, parsePlayerFrame } from "./playerRenderer";

/**
 * App em foco agora? Puro e testável: foco da janela E visibilidade do
 * documento precisam ser verdade (blur ou aba oculta pausam o preview).
 */
export function appFocusNow(
  windowFocused: boolean,
  documentHidden: boolean,
): boolean {
  return windowFocused && !documentHidden;
}

/**
 * Foco da janela do app. Fora do Tauri (mock/testes/SSR) assume focado e
 * delega a visibilidade ao documento — nunca quebra o caminho real.
 */
export function useAppFocus(): boolean {
  const [focused, setFocused] = useState(true);
  useEffect(() => {
    if (!isTauri()) return;
    let off: (() => void) | undefined;
    let cancelled = false;
    void import("@tauri-apps/api/window")
      .then((win) => {
        if (cancelled) return;
        const current = win.getCurrentWindow();
        current.isFocused().then(
          (initial) => {
            if (!cancelled) setFocused(initial);
          },
          () => undefined,
        );
        current.onFocusChanged(({ payload }) => {
          if (!cancelled) setFocused(payload);
        }).then(
          (stop) => {
            off = stop;
          },
          () => undefined,
        );
      })
      .catch(() => undefined);
    const onVisibility = (): void => {
      if (document.hidden) setFocused(false);
      else {
        void import("@tauri-apps/api/window")
          .then((win) => win.getCurrentWindow().isFocused())
          .then(
            (initial) => {
              if (!cancelled) setFocused(initial);
            },
            () => undefined,
          );
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onVisibility);
      off?.();
    };
  }, []);
  return focused;
}

export interface PreviewPlayerProps {
  kind: PreviewKind;
  id: string;
  /** Modal aberto + app em foco + fonte válida (o dono decide). */
  active: boolean;
  onToken?: (token: string | null) => void;
  onError?: (message: string | null) => void;
}

export function PreviewPlayer({ kind, id, active, onToken, onError }: PreviewPlayerProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [frames, setFrames] = useState(0);
  const tokenCb = useRef(onToken);
  tokenCb.current = onToken;
  const errorCb = useRef(onError);
  errorCb.current = onError;

  useEffect(() => {
    if (!active || !isTauri()) return;
    let disposed = false;
    let token: string | null = null;
    let renderer: ReturnType<typeof createPlayerRenderer> | undefined;
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer) => {
      if (disposed) return;
      let frame: ReturnType<typeof parsePlayerFrame>;
      try {
        frame = parsePlayerFrame(buffer);
      } catch {
        return;
      }
      try {
        deliverPlayerFrame(
          () => {
            const target = canvas.current;
            if (!target) throw new Error("canvas unavailable");
            renderer ??= createPlayerRenderer(target);
            renderer.draw(frame);
            setFrames((n) => n + 1);
          },
          () => undefined,
        );
      } catch {
        // Desenho falhou (canvas sumiu): o próximo frame tenta de novo.
      }
    };
    void (async () => {
      try {
        token = await previewStart(kind, id, channel);
        if (disposed) {
          await previewStop(token).catch(() => undefined);
          return;
        }
        tokenCb.current?.(token);
        errorCb.current?.(null);
      } catch (failure) {
        if (!disposed) {
          errorCb.current?.(
            failure instanceof Error ? failure.message : "Preview indisponível.",
          );
        }
      }
    })();
    return () => {
      disposed = true;
      renderer?.dispose();
      tokenCb.current?.(null);
      if (token) void previewStop(token).catch(() => undefined);
    };
  }, [kind, id, active]);

  if (!isTauri()) {
    return (
      <p className="hint" data-testid="preview-mock">
        Preview ao vivo só no app (aqui vale o thumb estático).
      </p>
    );
  }
  if (!active) return null;
  return (
    <div className="preview-live" data-testid="preview-live">
      <canvas ref={canvas} aria-label={`Pré-visualização de ${kind}`} />
      {frames === 0 ? <span className="watch-note">Aguardando preview…</span> : null}
    </div>
  );
}
