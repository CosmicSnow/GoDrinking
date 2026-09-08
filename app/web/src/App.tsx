/**
 * Sala mínima: intenções explícitas + snapshots do dono + 2 eventos Tauri.
 *
 * - Comandos só via `api.ts`, chamados de handlers (clique) ou de callbacks
 *   de evento. NENHUM setInterval/setTimeout neste módulo.
 * - Snapshot via `get_snapshot` sob demanda (botão Atualizar, respostas de
 *   intent, chegada de evento). Roster rico via evento "roster".
 * - Erros do backend já vêm redigidos (sem senha/token/SDP); a UI nunca
 *   exibe de volta o conteúdo dos campos de senha.
 */

import { useEffect, useState } from "react";
import {
  createRoom,
  getE2ePlan,
  getMediaCounters,
  getSnapshot,
  joinRoom,
  leaveRoom,
  onMediaEvent,
  onSignalEvent,
  setQuality as setQualityCommand,
  setServer,
  startShare,
  stopShare,
  unwatchMember,
  watchMember,
  listSources,
  sourceCapabilities,
  type CapabilitySet,
  type E2ePlan,
  type EffectiveQuality,
  type LinkStats,
  type MediaEvent,
  type OwnerSnapshot,
  type RoomMember,
  type SignalEvent,
  type SourceInfo,
  type ViewerStats,
} from "./api";
import { runE2ePlan, type E2eReport } from "./e2e";
import {
  MOCK_SELF_ID,
  isTauriMissing,
  mockCaps,
  mockCounters,
  mockEffective,
  mockRoster,
  mockSnapshot,
  mockSources,
  randomMockCode,
} from "./mock";
import {
  HomeScreen,
  RoomScreen,
  resolveDesired,
  validateCode,
  validateNickname,
  validatePassword,
  validateSource,
  type QualitySel,
  type ResolutionSel,
} from "./views";

export const DEFAULT_SERVER = "http://127.0.0.1:18790";

const signalSummary = (event: SignalEvent): string => {
  switch (event.kind) {
    case "roster":
      return `roster (${event.entries.length} membro(s))`;
    case "watch":
      return `watch de ${event.from}`;
    case "unwatch":
      return `unwatch de ${event.from}`;
    case "signal":
      return `sinal ${event.type} de ${event.from}`;
    default:
      return event.kind;
  }
};

const mediaSummary = (event: MediaEvent): string => {
  switch (event.kind) {
    case "frame":
      return `frame (não-preto: ${event.non_black ? "sim" : "não"})`;
    case "stats":
      return `stats (${event.frames} frames)`;
    case "quality":
      return `qualidade (geração ${event.generation})`;
    default:
      return event.kind;
  }
};

export default function App() {
  const [screen, setScreen] = useState<"home" | "room">("home");
  const [tab, setTab] = useState<"create" | "join">("create");
  const [server, setServerBase] = useState(DEFAULT_SERVER);
  // Apelido interno (sem input visível na home fiel ao goDrinking2).
  const [nickname, setNickname] = useState("Você");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [roomCode, setRoomCode] = useState<string | null>(null);
  const [selfId, setSelfId] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<OwnerSnapshot | null>(null);
  const [roster, setRoster] = useState<RoomMember[]>([]);
  const [watching, setWatching] = useState<string[]>([]);
  const [source, setSource] = useState("synthetic");
  // Fontes de captura: capacidades (puro, sem SO) ao entrar na sala;
  // a lista real só em gesto explícito (pode pedir permissão ao SO).
  const [sources, setSources] = useState<SourceInfo[]>([]);
  const [sourcesError, setSourcesError] = useState<string | null>(null);
  const [caps, setCaps] = useState<CapabilitySet | null>(null);
  const [lastSignal, setLastSignal] = useState<string | null>(null);
  const [lastMedia, setLastMedia] = useState<string | null>(null);
  const [stats, setStats] = useState<ViewerStats | null>(null);
  // Contadores por link: mesma fonte do refresh + evento stats (que já pode
  // trazer `links` no lado viewer). Nenhum polling novo.
  const [linkStats, setLinkStats] = useState<LinkStats[] | null>(null);
  // Perfil de qualidade desejado (staged na UI; Aplicar chama `set_quality`
  // e o efetivo autoritativo volta no snapshot/evento).
  const [resolution, setResolution] = useState<ResolutionSel>("720p");
  const [customW, setCustomW] = useState("");
  const [customH, setCustomH] = useState("");
  const [quality, setQuality] = useState<QualitySel>("medium");
  const [customBitrate, setCustomBitrate] = useState("");
  const [customFps, setCustomFps] = useState("");
  // Efetivo autoritativo (snapshot + evento quality); aplicação em voo e o
  // último erro do comando (verbatim, já redatado no backend).
  const [effective, setEffective] = useState<EffectiveQuality | null>(null);
  // Selo do codificador (snapshot + evento stats; sem polling novo).
  const [backend, setBackend] = useState<string | null>(null);
  const [backendNote, setBackendNote] = useState<string | null>(null);
  const [applying, setApplying] = useState(false);
  const [applyError, setApplyError] = useState<string | null>(null);

  // Modo autodirigido test-only: só ativa com `--e2e-plan` (get_e2e_plan
  // devolve null no app normal e nada aqui executa). Guarda contra
  // mount duplo; o driver é headless e reporta via status + título.
  // Modo mock automático no navegador puro (sem Tauri): `isMock` congela na
  // montagem via `isTauriMissing()` (ausência de `window.__TAURI__` /
  // `window.__TAURI_INTERNALS__`). Quando true, nenhum `invoke`/`listen` é
  // chamado — tudo é estado local via `mock.ts`.
  const [isMock] = useState(() => isTauriMissing());
  // Share mock (fonte de verdade do snapshot mock; o backend real usa o
  // snapshot para isso, aqui o toggle local alimenta `mockSnapshot`).
  const [mockSharing, setMockSharing] = useState(true);
  const [e2ePlan, setE2ePlan] = useState<E2ePlan | null>(null);
  const [e2eReport, setE2eReport] = useState<E2eReport | null>(null);
  useEffect(() => {
    if (isMock) return; // mock: sem Tauri, sem e2e, sem invoke
    let live = true;
    let started = false;
    getE2ePlan()
      .then((plan) => {
        if (!live || !plan || started) return;
        started = true;
        setE2ePlan(plan);
        void runE2ePlan(plan, (report) => {
          if (live) {
            setE2eReport(report);
            document.title = `GoLive e2e ${report.role} ${report.phase}`;
          }
        }).catch((failure: unknown) => {
          if (live) {
            setE2eReport({
              role: plan.role,
              phase: "error",
              connected: false,
              frames: 0,
              keyframes: 0,
              keyframesSeen: false,
              presented: 0,
              qualityApplied: false,
              detail: typeof failure === "string" ? failure : failure instanceof Error ? failure.message : "e2e failed",
            });
          }
        });
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [isMock]);

  const messageOf = (failure: unknown, fallback: string): string => {
    if (failure instanceof Error) return failure.message;
    if (typeof failure === "string") return failure;
    return fallback;
  };

  /** Lê snapshot + contadores agora (botão, pós-intent, pós-evento). */
  const refresh = async (): Promise<void> => {
    if (isMock) {
      // mock: sem invoke — deriva tudo do estado local (share/watch/efetivo).
      const eff = effective ?? mockEffective();
      setSnapshot(mockSnapshot(mockSharing, watching));
      setRoster(mockRoster(nickname.trim() || "Você", mockSharing));
      const counters = mockCounters(watching, eff);
      setLinkStats(counters.links);
      setEffective(eff);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats((current) =>
        current ?? {
          frames: counters.frames,
          keyframes: counters.keyframes,
          ice: counters.connected,
          presented: counters.presented,
        },
      );
      return;
    }    try {
      setSnapshot(await getSnapshot());
    } catch (failure) {
      setError(messageOf(failure, "Não foi ler o estado da sala."));
      return;
    }
    try {
      const counters = await getMediaCounters();
      setLinkStats(counters.links);
      setEffective(counters.effective ?? null);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
    } catch {
      // Contadores são fallback observacional: sem eles, o painel de links
      // mostra o diagnóstico honesto em vez de número inventado.
      setLinkStats((current) => current);
    }
  };

  // Escuta única dos 2 eventos, só dentro da sala, com cleanup.
  // Cada evento atualiza o que é dele (roster/stats) e pede um snapshot
  // fresco — dirigido a evento, nunca a timer.
  useEffect(() => {
    if (screen !== "room") return;
    if (isMock) return; // mock: onSignal/onMedia viram no-op (dados já locais)
    let cancelled = false;
    const unlistens: Array<() => void> = [];
    void onSignalEvent((event) => {
      if (cancelled) return;
      if (event.kind === "roster") {
        setRoster(event.entries);
      } else if (event.kind === "kicked") {
        setError("Você foi removido da sala.");
      }
      setLastSignal(signalSummary(event));
      void refresh();
    }).then((off) => {
      if (cancelled) off();
      else unlistens.push(off);
    });
    void onMediaEvent((event) => {
      if (cancelled) return;
      if (event.kind === "stats") {
        setStats({ frames: event.frames, keyframes: event.keyframes, ice: event.ice, presented: event.presented ?? 0 });
        if (event.links) setLinkStats(event.links);
        if (event.backend !== undefined) setBackend(event.backend);
        if (event.backend_note !== undefined) setBackendNote(event.backend_note);
      } else if (event.kind === "quality") {
        setEffective({ profile: event.profile, generation: event.generation });
      }
      setLastMedia(mediaSummary(event));
      void refresh();
    }).then((off) => {
      if (cancelled) off();
      else unlistens.push(off);
    });
    return () => {
      cancelled = true;
      for (const off of unlistens) off();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [screen]);

  const runIntent = async (work: () => Promise<void>): Promise<void> => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await work();
      await refresh();
    } catch (failure) {
      setError(messageOf(failure, "A operação falhou."));
    } finally {
      setBusy(false);
    }
  };

  const handleCreate = (): void => {
    if (isMock) {
      // mock: pula TODAS as validações do caminho real (apelido/senha) —
      // o lobby fiel não tem campo de apelido e a senha é opcional aqui.
      // Entra sempre com sucesso.
      const effectiveNickname = nickname.trim() || "Você";
      const created = randomMockCode();
      const eff = mockEffective();
      const counters = mockCounters([], eff);
      setRoomCode(created);
      setNickname(effectiveNickname);
      setSelfId(MOCK_SELF_ID);
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, []));
      setRoster(mockRoster(effectiveNickname, true));
      setWatching([]);
      setEffective(eff);
      setLinkStats(counters.links);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats({
        frames: counters.frames,
        keyframes: counters.keyframes,
        ice: counters.connected,
        presented: counters.presented,
      });
      setCaps(mockCaps());
      setSources([]);
      setSourcesError(null);
      setLastSignal("roster (4 membro(s))");
      setLastMedia(null);
      setError(null);
      setScreen("room");
      return;
    }
    // Caminho Tauri real: validações mantidas como estão.
    const effectiveNickname = nickname.trim() || "Você";
    const nameError = validateNickname(effectiveNickname);
    if (nameError) {
      setError(nameError);
      return;
    }
    const passError = validatePassword(password);
    if (passError) {
      setError(passError);
      return;
    }
    const base = server.trim() || DEFAULT_SERVER;
    void runIntent(async () => {
      await setServer(base);
      const created = await createRoom(effectiveNickname, password);
      setRoomCode(created);
      setNickname(effectiveNickname);
      setSelfId(null); // create devolve código; nosso id chega no roster
      setScreen("room");
      // Capacidades são fatos de compilação (sem SO): seguro buscar ao entrar.
      // A lista real de fontes só em gesto explícito (pode pedir permissão).
      sourceCapabilities().then(setCaps, () => undefined);
    });
  };

  const handleJoin = (): void => {
    if (isMock) {
      // mock: pula TODAS as validações do caminho real (apelido/senha/código).
      // Usa o digitado ou gera um; entra sempre com sucesso.
      const effectiveNickname = nickname.trim() || "Você";
      const joined = code.trim().toUpperCase() || randomMockCode();
      const eff = mockEffective();
      const counters = mockCounters([], eff);
      setRoomCode(joined);
      setNickname(effectiveNickname);
      setSelfId(MOCK_SELF_ID);
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, []));
      setRoster(mockRoster(effectiveNickname, true));
      setWatching([]);
      setEffective(eff);
      setLinkStats(counters.links);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats({
        frames: counters.frames,
        keyframes: counters.keyframes,
        ice: counters.connected,
        presented: counters.presented,
      });
      setCaps(mockCaps());
      setSources([]);
      setSourcesError(null);
      setLastSignal("roster (4 membro(s))");
      setLastMedia(null);
      setError(null);
      setScreen("room");
      return;
    }
    // Caminho Tauri real: validações mantidas como estão.
    const effectiveNickname = nickname.trim() || "Você";
    const nameError = validateNickname(effectiveNickname);
    if (nameError) {
      setError(nameError);
      return;
    }
    const passError = validatePassword(password);
    if (passError) {
      setError(passError);
      return;
    }
    const codeError = validateCode(code);
    if (codeError) {
      setError(codeError);
      return;
    }
    const base = server.trim() || DEFAULT_SERVER;
    void runIntent(async () => {
      await setServer(base);
      const memberId = await joinRoom(code.trim().toUpperCase(), effectiveNickname, password);
      setRoomCode(code.trim().toUpperCase());
      setNickname(effectiveNickname);
      setSelfId(memberId);
      setScreen("room");
      sourceCapabilities().then(setCaps, () => undefined);
    });
  };

  const handleLeave = (): void => {
    if (isMock) {
      // mock: só limpa o estado local.
      setScreen("home");
      setSnapshot(null);
      setRoster([]);
      setWatching([]);
      setStats(null);
      setLinkStats(null);
      setEffective(null);
      setApplying(false);
      setApplyError(null);
      setSources([]);
      setSourcesError(null);
      setCaps(null);
      setLastSignal(null);
      setLastMedia(null);
      setRoomCode(null);
      setSelfId(null);
      setPassword("");
      setMockSharing(true);
      return;
    }
    void runIntent(async () => {
      await leaveRoom().catch(() => undefined);
      setScreen("home");
      setSnapshot(null);
      setRoster([]);
      setWatching([]);
      setStats(null);
      setLinkStats(null);
      setEffective(null);
      setApplying(false);
      setApplyError(null);
      setSources([]);
      setSourcesError(null);
      setCaps(null);
      setLastSignal(null);
      setLastMedia(null);
      setRoomCode(null);
      setSelfId(null);
      setPassword("");
    });
  };

  const handleListSources = (): void => {
    if (isMock) {
      // mock: lista local, sem pedir permissão ao SO.
      setSourcesError(null);
      setSources(mockSources());
      return;
    }
    setSourcesError(null);
    listSources().then(
      (listed) => {
        setSources(listed);
        if (listed.length === 0) {
          setSourcesError("Nenhuma fonte visível — provavelmente falta permissão de Gravação de Tela.");
        }
      },
      (failure: unknown) => {
        setSources([]);
        setSourcesError(messageOf(failure, "Não foi listar as fontes."));
      },
    );
  };

  const handleShare = (): void => {
    const sourceError = validateSource(source);
    if (sourceError) {
      setError(sourceError);
      return;
    }
    if (isMock) {
      // mock: só atualiza o estado local (snapshot/roster/efetivo).
      const eff = effective ?? mockEffective();
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, watching));
      setRoster(mockRoster(nickname.trim() || "Você", true));
      setEffective(eff);
      setLastMedia("frame (não-preto: sim)");
      setError(null);
      return;
    }
    void runIntent(async () => {
      await startShare(source.trim());
    });
  };

  const handleStopShare = (): void => {
    if (isMock) {
      setMockSharing(false);
      setSnapshot(mockSnapshot(false, watching));
      setRoster(mockRoster(nickname.trim() || "Você", false));
      setLastMedia("frame (não-preto: não)");
      return;
    }
    void runIntent(async () => {
      await stopShare();
    });
  };

  /**
   * Aplica o perfil desejado ao share vivo. Progresso e erro próprios do
   * painel (não usa o busy global): o comando responde o efetivo pré-bump
   * e a geração autoritativa chega pelo evento `quality`.
   */
  const handleApplyQuality = (): void => {
    if (applying || busy) return;
    const resolved = resolveDesired({
      resolution,
      customW,
      customH,
      quality,
      customBitrate,
      customFps,
      srcDims: selectedSourceDims(),
    });
    if (!("profile" in resolved)) {
      setApplyError(resolved.errors.join(" "));
      return;
    }
    if (isMock) {
      // mock: aplica localmente com bump de geração, sem comando.
      setEffective({
        profile: resolved.profile,
        generation: (effective?.generation ?? 0) + 1,
      });
      setApplying(false);
      setApplyError(null);
      setLastMedia("qualidade (geração mock)");
      return;
    }
    setApplying(true);
    setApplyError(null);
    const preset = quality === "custom" ? undefined : quality;
    setQualityCommand(resolved.profile, preset).then(
      (result) => {
        setEffective(result);
        setApplying(false);
        void refresh();
      },
      (failure: unknown) => {
        setApplyError(messageOf(failure, "Não foi aplicar a qualidade."));
        setApplying(false);
      },
    );
  };

  const handleWatch = (id: string): void => {
    if (isMock) {
      // mock: só atualiza o estado local (links/contadores derivam daqui).
      const next = watching.includes(id) ? watching : [...watching, id];
      const eff = effective ?? mockEffective();
      setWatching(next);
      setSnapshot(mockSnapshot(mockSharing, next));
      setLinkStats(mockCounters(next, eff).links);
      setLastSignal(`watch de ${id}`);
      return;
    }
    void runIntent(async () => {
      await watchMember(id);
      setWatching((current) => (current.includes(id) ? current : [...current, id]));
    });
  };

  const handleUnwatch = (id: string): void => {
    if (isMock) {
      const next = watching.filter((item) => item !== id);
      const eff = effective ?? mockEffective();
      setWatching(next);
      setSnapshot(mockSnapshot(mockSharing, next));
      setLinkStats(mockCounters(next, eff).links);
      setLastSignal(`unwatch de ${id}`);
      return;
    }
    void runIntent(async () => {
      await unwatchMember(id);
      setWatching((current) => current.filter((item) => item !== id));
    });
  };

  if (e2ePlan) {
    const phase = e2eReport?.phase ?? "boot";
    const summary =
      `e2e ${e2ePlan.role} ${phase} ` +
      `connected=${e2eReport?.connected ? "yes" : "no"} ` +
      `frames=${e2eReport?.frames ?? 0} ` +
      `keyframes=${e2eReport?.keyframes ?? 0} ` +
      `presented=${e2eReport?.presented ?? 0}` +
      (e2eReport?.code ? ` code=${e2eReport.code}` : "") +
      (e2eReport?.detail ? ` ${e2eReport.detail}` : "");
    return (
      <main data-testid="e2e-status">
        <h1>GoLive e2e {e2ePlan.role}</h1>
        <p>{summary}</p>
      </main>
    );
  }

  if (screen === "home") {
    return (
      <HomeScreen
        server={server}
        onServer={setServerBase}
        tab={tab}
        onTab={setTab}
        nickname={nickname}
        onNickname={setNickname}
        password={password}
        onPassword={setPassword}
        code={code}
        onCode={setCode}
        busy={busy}
        error={error}
        onCreate={handleCreate}
        onJoin={handleJoin}
        createdCode={roomCode}
        mock={isMock}
      />
    );
  }

  // Dimensões da fonte quando conhecida (display:/window: listado com w×h);
  // synthetic/movie não têm teto conhecido — o backend normaliza na borda.
  const selectedSourceDims = (): { w: number; h: number } | null => {
    const selected = sources.find((item) => `${item.kind}:${item.id}` === source.trim());
    return selected ? { w: selected.w, h: selected.h } : null;
  };
  const srcDims = selectedSourceDims();
  const shareLive = snapshot?.share.state === "live";

  return (
    <RoomScreen
      roomCode={roomCode}
      nickname={nickname.trim() || "você"}
      snapshot={snapshot}
      roster={roster}
      selfId={selfId}
      selfNickname={nickname.trim()}
      watching={watching}
      quality={{
        shareLive,
        busy,
        effective,
        backend,
        backendNote,
        applying,
        applyError,
        resolution,
        onResolution: setResolution,
        customW,
        onCustomW: setCustomW,
        customH,
        onCustomH: setCustomH,
        quality,
        onQuality: setQuality,
        customBitrate,
        onCustomBitrate: setCustomBitrate,
        customFps,
        onCustomFps: setCustomFps,
        srcDims,
        onApply: handleApplyQuality,
      }}
      linkStats={linkStats}
      source={source}
      onSource={setSource}
      sources={sources}
      sourcesError={sourcesError}
      caps={caps}
      onListSources={handleListSources}
      busy={busy}
      error={error}
      lastSignal={lastSignal}
      lastMedia={lastMedia}
      stats={stats}
      onRefresh={() => void refresh()}
      onLeave={handleLeave}
      onShare={handleShare}
      onStopShare={handleStopShare}
      onWatch={handleWatch}
      onUnwatch={handleUnwatch}
      mock={isMock}
    />
  );
}
