# Bugs — GoLive (revisar)

> Regra: bug 100% corrigido **e verificado** SAI desta lista (ver
> AGENTS.md, seção Bugs). Nunca marcar como feito no lugar — remover a
> linha. A lista contém só bugs abertos.

| ID      | Sintoma                                                        | Status         | Desde                    | Suspeita / notas |
|---------|----------------------------------------------------------------|----------------|--------------------------|------------------|
| BUG-001 | Compartilhamento de tela lento com hosts e viewers macOS/Windows | open — Mac validado ao vivo, falta Windows + CPU/GPU | relatado após mudança PLI | Confirmado: gates de captura/ponte reiniciavam o intervalo a cada chegada, perdendo FPS com jitter. Regressão de 300 chegadas a 30 FPS com jitter de 1 ms: 151 encaminhadas antes, 300 após correção de cadência. Validado ao vivo no Mac (viewer fresh ~28,6/s, Display-3 PASS 23 s). Aberto: host Windows 1080p60 emite ~13fps/~2,5 Mbps (medido no viewer LHYSYV, path sem perdas — teto no emissor, trace do host pendente) + CPU/GPU sustentados. `link_stats.bitrate_bps` mede RGBA apresentado, não bitrate H.264; não prova storm de IDR. |
| BUG-002 | Windows lento + GPU ~40% de RTX 3090 só assistindo             | open (windows) | build Windows pós-DXGI   | Lado Windows (LLM Windows): checar decode por software, present loop sem vsync, upload de textura por frame. Evidência nova: viewer inocente (28fps local saudável, 0 repeats, path sem perdas); pipeline sem GPU em nenhum estágio (sem NVENC — 3090 não ajuda em nada hoje); host Windows emite ~13fps num alvo 60fps (ver BUG-001). |
| BUG-003 | Viewer repete `ice connected` a cada ~0,5–2 s a sessão toda    | open | log viewer do amigo (~150 linhas, sessão com watch+share) | `wire_ice_events` (media.rs) emite sem dedupe a cada transição Connected/Completed — connects succeeding = flap/retry loop, não causa do kick. Apurar gatilho (roster re-watch? ICE flap). |

Validação BUG-001 (2026-09-09): checks/testes de app, core, platform e
platform-macos passaram; web typecheck/test/build, testes do server e
`cargo xwin check --target x86_64-pc-windows-msvc` passaram. Build release
macOS concluído. Após conceder Gravação de Tela, captura de display e
apresentação no watcher local confirmadas; viewer local saudável
(~28,6 fresh fps, 0 repeats, medido 2026-09-09). Observação antiga de
~3,7 FPS superada no Mac; lentidão restante é host Windows (ver linha
BUG-001). CPU/GPU sustentados ainda não verificados. Achado adicional:
timeout de 250 ms no feeder reenvia o frame anterior e incrementa apresentados,
podendo aparentar ~4 FPS sem frames novos. Instrumentação opt-in descrita em
MEDIA_DEBUG.md separa captura, entrada, encode, RTP, decode e apresentações
repetidas; feeder corrigido para esperar frames novos após timeout. Regressão com helper
real: 1 frame gerava 3 apresentações; agora gera 1 e retoma com frame novo. Testes nativos
de platform-windows precisam de Windows.

Validação adicional BUG-001 (2026-09-09): trace da sessão lenta preservado em
`e2e-artifacts/slow-screen-share-repro/`: captura/encode ~29 FPS, watcher
1 frame novo em 30 s, 107/108 apresentações repetidas. Repro de vídeo 720p30
via WebRTC: debug padrão 79 frames novos/5 s (falha no mínimo 125); somente
OpenH264 otimizado 149/5 s; release 150/5 s. Conversão RGBA em
`H264Decoder::decode` estourava o orçamento no debug. OpenH264 agora usa
opt-level=3 nos perfis dev de app/core. Verificação ao vivo da nova build,
60 FPS e Windows ainda pendentes; não considerar o problema resolvido.

Checks após ajuste: app `cargo test --lib` (69) + smoke compila, core
`--lib` (56), platform (23), web vitest (68), `npm run build`,
server `npm test`, `analyze-trace.py --self-test` (14) — todos verdes
(medido 2026-09-09). `cargo xwin check` PENDENTE p/ RestartOrder
(platform-windows não compila no host macOS). Executável debug e helper
recompilados com `tauri/custom-protocol`.

Validação Display-3 + geração (2026-09-09): harness `E2E_SHARE=display:3`
(hook `--e2e-plan`, traces por instância + analyzer) verdict PASS em 23 s:
host `quality-applied` + `qualityApplied: true`, viewer conectado com frames
e presented. Root cause do "quality generation bump timeout": `set_quality`
gravava `generation` antiga por cima do bump do forward task durante o
restart SCK (~146 ms) — corrigido com max sob lock + teste de regressão
(`set_quality_never_writes_generation_backwards`, falha sem o fix, passa
com). Mídia do run: encode ~14,4/s → viewer fresh ~14,8/s (perfil
640x360@15 pós-switch), 0 repeats, 5 drops só no join-burst pré-keyframe
(PLI 0/0 — keyframe-wait, não perda), um gap de ~1 s na janela do switch
com recuperação total. Arquivos (não commitados): `app/src/lib.rs` (fix),
`core/src/trace.rs` + `media.rs` + `video/mod.rs` (pli/max_gap),
`scripts/e2e-packaged.sh` + `e2e.ts` (lane display + traces). Veredicto e
traces do run em `e2e-artifacts/` (verdict.json + traces/host-trace +
viewer-trace). Reconnect loop confirmado em log posterior e registrado
como BUG-003 (linha da tabela). NOTA: todos os fixes citados nesta
validação estão NÃO-COMMITADOS na árvore (~15 arquivos) — necessário
commit antes de distribuir builds (o branch `fresh/native-core` do amigo
não os contém).
