/**
 * Autodireção test-only do E2E empacotado.
 *
 * Só executa quando `getE2ePlan()` devolve um plano (app lançado com
 * `--e2e-plan '<json>'`). Sem plano, nada aqui é importado em caminho quente
 * — a UI normal nunca toca este módulo além do boot check.
 *
 * Host: set_server → create_room → publica code → start_share (plan.share
 * ou "synthetic") → aguarda ice-connected + keyframe (eventos) → status
 * connected. Um share display: que o SO nega (sem consentimento de Gravação
 * de Tela) rejeita aqui e o boot registra phase error com o detalhe — o
 * veredito do harness segue FAIL gracioso, nunca fallback silencioso.
 * Viewer: aguarda code → set_server → join → roster acha quem compartilha →
 * watch → aguarda ice-connected + stats com frames>0 → status connected.
 *
 * Segredos nunca entram nos reports (só role/state/code/contagens).
 */

import {
  createRoom,
  e2eReadCode,
  e2eStatus,
  getMediaCounters,
  getSnapshot,
  joinRoom,
  onMediaEvent,
  onSignalEvent,
  setQuality,
  setServer,
  startShare,
  watchMember,
  type E2ePlan,
  type MediaEvent,
  type OwnerSnapshot,
  type RoomMember,
} from "./api";

export type E2ePhase =
  | "boot"
  | "room"
  | "sharing"
  | "joined"
  | "watching"
  | "connected"
  | "quality-applied"
  | "error";

export interface E2eReport {
  role: "host" | "viewer";
  phase: E2ePhase;
  code?: string;
  connected: boolean;
  /** Frames decodificados (viewer) ou 0 (host não decodifica). */
  frames: number;
  keyframes: number;
  /** Keyframes observados (host: encoder; viewer: decoder). */
  keyframesSeen: boolean;
  /** Frames apresentados em janelas nativas (acks; evidência de apresentação). */
  presented: number;
  /** set_quality aplicado mid-share pelo caminho real do invoke + geração bumpada. */
  qualityApplied: boolean;
  detail?: string;
  watchedMember?: string;
}

/** Acha quem está compartilhando (não somos nós). Puro e testável. */
export function pickSharer(entries: RoomMember[], selfId: string | null): string | null {
  const found = entries.find((e) => e.share && e.id !== selfId);
  return found ? found.id : null;
}

/** Algum link Connected no snapshot. Puro e testável. */
export function linkConnected(snapshot: OwnerSnapshot | null): boolean {
  if (!snapshot) return false;
  return snapshot.links.some((link) => link.state === "connected");
}

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

function messageOf(failure: unknown): string {
  if (failure instanceof Error) return failure.message;
  if (typeof failure === "string") return failure;
  try {
    return JSON.stringify(failure);
  } catch {
    return "unknown failure";
  }
}

async function publish(report: E2eReport): Promise<void> {
  const { role, phase, code, connected, frames, keyframes, keyframesSeen, presented, qualityApplied, detail } = report;
  await e2eStatus({ role, state: phase, code, connected, frames, keyframes, keyframesSeen, presented, qualityApplied, detail });
}

async function readCodeWithRetry(onReport: (r: E2eReport) => void): Promise<string> {
  const deadline = Date.now() + 60_000;
  for (;;) {
    try {
      return await e2eReadCode();
    } catch {
      if (Date.now() > deadline) {
        throw new Error("room code never published");
      }
      onReport({ role: "viewer", phase: "boot", connected: false, frames: 0, keyframes: 0, keyframesSeen: false, presented: 0, qualityApplied: false, detail: "waiting-room-code" });
      await sleep(500);
    }
  }
}

async function runHost(plan: E2ePlan, onReport: (r: E2eReport) => void): Promise<void> {
  const report: E2eReport = {
    role: "host",
    phase: "boot",
    connected: false,
    frames: 0,
    keyframes: 0,
    keyframesSeen: false,
    presented: 0,
    qualityApplied: false,
  };
  const emit = async (): Promise<void> => {
    onReport({ ...report });
    await publish(report);
  };
  await setServer(plan.server);
  const code = await createRoom(plan.nickname, plan.password);
  report.code = code;
  report.phase = "room";
  await emit();
  await startShare(plan.share ?? "synthetic");
  report.phase = "sharing";
  await emit();

  // Observation is poll-primary (backend counters), event-backed: push
  // events are best-effort in every environment, the counters are not.
  const offMedia = await onMediaEvent((event: MediaEvent) => {
    if (event.kind === "ice-connected") {
      report.connected = true;
      void emit();
    } else if (event.kind === "keyframe") {
      report.keyframesSeen = true;
      report.keyframes += 1;
      void emit();
    } else if (event.kind === "stats") {
      report.frames = Math.max(report.frames, event.frames);
    }
  }).catch((failure: unknown) => {
    throw new Error(`media listen failed: ${messageOf(failure)}`);
  });
  try {
    await waitFor(
      async () => {
        try {
          const counters = await getMediaCounters();
          if (counters.connected) report.connected = true;
          if (counters.keyframes_seen) report.keyframesSeen = true;
          report.keyframes = Math.max(report.keyframes, counters.keyframes);
        } catch {
          /* backend indisponível momentaneamente */
        }
        try {
          const snapshot = await getSnapshot();
          if (linkConnected(snapshot)) report.connected = true;
        } catch {
          /* melhor-esforço */
        }
        await emit();
        return report.connected && report.keyframesSeen;
      },
      100_000,
      "host flow timeout",
    );
    report.phase = "connected";
    await emit();
    // Prova do caminho real do aplicar-qualidade sobre fonte sintética:
    // o MESMO intent da UI (api.setQuality) → geração efetiva bumpa e o
    // stream segue (o viewer prova continuidade do outro lado).
    // Diagnóstico temporário: registra as chaves exatas enviadas.
    const qp = { w: 640, h: 360, bitrate_kbps: 1000, fps: 15 };
    report.detail = `sending set_quality keys: ${Object.keys(qp).join(",")}`;
    await emit();
    await setQuality(qp);
    await waitFor(
      async () => {
        try {
          const counters = await getMediaCounters();
          if ((counters.effective?.generation ?? 0) >= 1) {
            report.qualityApplied = true;
            await emit();
            return true;
          }
        } catch {
          /* tenta de novo até o deadline */
        }
        return false;
      },
      30_000,
      "quality generation bump timeout",
    );
    report.phase = "quality-applied";
    await emit();
  } finally {
    offMedia();
  }
}

/**
 * Poll test-only com deadline: avalia `cond` a cada 500ms até verdade ou
 * timeout (quando rejeita com `what`). Eventos push continuam atualizando o
 * report em paralelo; o poll é a fonte autoritativa.
 */
async function waitFor(
  cond: () => Promise<boolean>,
  timeoutMs: number,
  what: string,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await cond()) return;
    if (Date.now() > deadline) throw new Error(what);
    await sleep(500);
  }
}

async function runViewer(plan: E2ePlan, onReport: (r: E2eReport) => void): Promise<void> {
  const report: E2eReport = {
    role: "viewer",
    phase: "boot",
    connected: false,
    frames: 0,
    keyframes: 0,
    keyframesSeen: false,
    presented: 0,
    qualityApplied: false,
  };
  const emit = async (): Promise<void> => {
    onReport({ ...report });
    await publish(report);
  };
  const code = await readCodeWithRetry(onReport);
  report.code = code;
  await setServer(plan.server);
  const selfId = await joinRoom(code, plan.nickname, plan.password);
  report.phase = "joined";
  await emit();

  // Subscriptions up front: rejections fail loudly instead of hanging.
  let watching = false;
  const offSignal = await onSignalEvent((event) => {
    if (event.kind === "roster" && !watching) {
      const sharer = pickSharer(event.entries, selfId);
      if (sharer) {
        watching = true;
        report.phase = "watching";
        void emit();
        watchMember(sharer).then(() => { report.watchedMember = sharer; void emit(); }).catch(() => {
          watching = false;
        });
      }
    }
  }).catch((failure: unknown) => {
    throw new Error(`signal listen failed: ${messageOf(failure)}`);
  });
  const offMedia = await onMediaEvent((event: MediaEvent) => {
    if (event.kind === "ice-connected") {
      report.connected = true;
      void emit();
    } else if (event.kind === "stats") {
      report.frames = Math.max(report.frames, event.frames);
      report.keyframes = Math.max(report.keyframes, event.keyframes);
      void emit();
    } else if (event.kind === "keyframe") {
      report.keyframesSeen = true;
      report.keyframes += 1;
      void emit();
    } else if (event.kind === "frame" && event.non_black) {
      report.frames = Math.max(report.frames, 1);
      void emit();
    }
  }).catch((failure: unknown) => {
    throw new Error(`media listen failed: ${messageOf(failure)}`);
  });
  try {
    await waitFor(
      async () => {
        try {
          const counters = await getMediaCounters();
          if (counters.connected) report.connected = true;
          report.frames = Math.max(report.frames, counters.frames);
          report.keyframes = Math.max(report.keyframes, counters.keyframes);
          if (counters.keyframes_seen) report.keyframesSeen = true;
          report.presented = Math.max(report.presented, counters.presented);
        } catch {
          /* backend indisponível momentaneamente */
        }
        try {
          const snapshot = await getSnapshot();
          if (linkConnected(snapshot)) report.connected = true;
        } catch {
          /* melhor-esforço */
        }
        await emit();
        // Finish only after we actually started watching: counters alone
        // could (in theory) look alive before the roster even arrives.
        return report.connected && report.frames > 0 && report.presented > 0 && watching;
      },
      100_000,
      "viewer flow timeout",
    );
    report.phase = "connected";
    await emit();
  } finally {
    offSignal();
    offMedia();
  }
}

/**
 * Ponto de entrada do modo autodirigido. Rejeita em timeout/erro (o boot
 * registra o erro no status e no título da janela para o screenshot).
 */
export async function runE2ePlan(
  plan: E2ePlan,
  onReport: (r: E2eReport) => void,
): Promise<void> {
  if (typeof document !== "undefined") {
    document.title = `goDrinking e2e ${plan.role}`;
  }
  try {
    if (plan.role === "host") {
      await runHost(plan, onReport);
    } else {
      await runViewer(plan, onReport);
    }
  } catch (failure) {
    const detail =
      failure instanceof Error
        ? failure.message
        : typeof failure === "string"
          ? failure
          : "e2e failed";
    const report: E2eReport = {
      role: plan.role,
      phase: "error",
      connected: false,
      frames: 0,
      keyframes: 0,
      keyframesSeen: false,
      presented: 0,
      qualityApplied: false,
      detail,
    };
    onReport(report);
    try {
      await publish(report);
    } catch {
      /* arquivo pode nem existir; segue */
    }
    throw failure;
  }
}
