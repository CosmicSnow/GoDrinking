# Bugs — GoLive (revisar)

> Regra: bug 100% corrigido **e verificado** SAI desta lista (ver
> AGENTS.md, seção Bugs). Nunca marcar como feito no lugar — remover a
> linha. A lista contém só bugs abertos.

| ID      | Sintoma                                                        | Status         | Desde                    | Suspeita / notas |
|---------|----------------------------------------------------------------|----------------|--------------------------|------------------|
| BUG-001 | Compartilhamento de tela lento com hosts e viewers macOS/Windows | open — correção parcial, falta validar ao vivo | relatado após mudança PLI | Confirmado: gates de captura/ponte reiniciavam o intervalo a cada chegada, perdendo FPS com jitter. Regressão de 300 chegadas a 30 FPS com jitter de 1 ms: 151 encaminhadas antes, 300 após correção de cadência. Validar tela real e consumo de recursos antes de remover. `link_stats.bitrate_bps` mede RGBA apresentado, não bitrate H.264; não prova storm de IDR. |
| BUG-002 | Windows lento + GPU ~40% de RTX 3090 só assistindo             | open (windows) | build Windows pós-DXGI   | Lado Windows (LLM Windows): checar decode por software, present loop sem vsync, upload de textura por frame. |

Validação BUG-001 (2026-09-09): checks/testes de app, core, platform e
platform-macos passaram; web typecheck/test/build, testes do server e
`cargo xwin check --target x86_64-pc-windows-msvc` passaram. Build release
macOS concluído. Após conceder Gravação de Tela, captura de display e
apresentação no watcher local confirmadas; suavidade/FPS sustentado e CPU/GPU
ainda não verificados. Usuário segue observando ~3,7 FPS. Achado adicional:
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

Checks após ajuste: app `cargo check`/`cargo test` (57 + smoke), core
`cargo test` (54 + 3 integração, incluindo movie com fixture), web
`npm run typecheck`/`npm test` (67)/`npm run build`, server `npm test`,
Windows cross-check passaram (6 avisos preexistentes no stub VT).
Executável debug e helper recompilados com `tauri/custom-protocol`.
