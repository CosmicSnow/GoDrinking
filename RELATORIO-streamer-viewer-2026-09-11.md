# Relatório — streamer + viewer (resolução, NVENC, áudio, Mac)

Data (UTC): 2026-09-11. Máquina: Windows, build já executado, `golive-app.exe` transmitindo durante o diagnóstico (PID 6468 observado; nenhum processo foi encerrado, nenhum rebuild foi rodado para não derrubar a live).
Branch: `dev` tracking `origin/dev`. Head: `d7704e5 refactor: configure H.264 profile, disable B-frames and CABAC for NVENC encoder`.
Histórico recente relevante: `17e682c fix: improve NVENC hardware fallback reliability...`, `bd47a63 feat: implement Windows hardware encoder support with NvencEncoder...`, `b52e121 feat: implement auto-unwatch... cap OpenH264 encoding to 1080p`, `2144331 fix: join abort, watch a/v, roster ghosts, 5120x1440`.

Pedido: rodar os testes + verificar tudo do streamer e viewer para 4 sintomas: (1) resolução do monitor, (2) áudio não aparece com o vídeo, (3) usuário Mac não vê a tela, (4) resoluções não pegam no NVENC, só as menores.

Nada neste relatório altera comportamento. São indícios + provas lidas no código + reparos indicados com ponto de verificação.

## 1. Testes executados (provas)

Todos rodados nesta máquina em 2026-09-11, com a live no ar:

| Suíte | Comando | Resultado |
|---|---|---|
| Web | `npm test` em `app/web/` | PASS: `Test Files 3 passed (3)`, `Tests 76 passed (76)`, ~1,06 s |
| Server | `npm test` em `server/` | PASS: `rooms ok, auth ok, admit ok, signaling ok, limits ok, kick ok, succession ok, heartbeat ok, disconnect ok, ratelimit ok, nomedia ok` |
| Core | `cargo test --manifest-path ../core/Cargo.toml --lib` em `app/` | PASS: `67 passed; 0 failed` |
| Platform | `cargo test --manifest-path ../platform/Cargo.toml --lib` | PASS: `25 passed; 0 failed` |
| Platform-windows | `cargo test --manifest-path ../platform-windows/Cargo.toml --lib` | PASS: `10 passed; 0 failed` |
| App lib | `cargo test --lib` em `app/` | FALHA DE LOAD, não é falha de assert: compilou (~1m17s, só warnings), mas o binário de teste abortou com `exit code 0xc0000139, STATUS_ENTRYPOINT_NOT_FOUND`. Nenhum teste do `app` executou. Ver `core/src/vt.rs:838` warning `unused imports VtEncoder/probe_hardware` e `platform-windows/src/wgc.rs:343` warning `mut pool` — cosméticos, não são a causa do load. |

Interpretação: o contrato testável (perfis, encode/decode software, bridge, trickle, signaling) está verde. O que NÃO está coberto por esses testes: NVENC real (probe exige MFT de hardware + `GOLIVE_DISABLE_HW` ausente), WASAPI real, DXGI/WGC real, e o harnes `app --lib` quebrado neste host (impede validar `screen.rs`/`pump.rs`/`lib.rs` aqui — prioridade de infra).

## 2. Indícios e provas por sintoma

### 2.1 NVENC só pega as menores — causa raiz localizada (prova no código)

- `core/src/nvenc.rs:656` (dentro de `video_type()`): `MF_MT_MPEG2_LEVEL` é fixo em `eAVEncH264VLevel4_1`, independente de `w/h/fps`.
- `core/src/nvenc.rs:581-599` (`configure_types`): tenta `ConstrainedBase` e depois `Base`, mas AMBOS passam pelo mesmo `video_type()` com Level 4.1. Ou seja, a segunda tentativa não salva resoluções altas.
- `core/src/nvenc.rs:185-188` (`NvencEncoder::new`): aceita até `MAX_DIM` (8192) e dims pares — então o erro não aparece na validação, aparece no `SetOutputType` do MFT, que rejeita o nível.
- Por que bate com o sintoma: Level 4.1 cobre 720p/1080p30; 1080p60 já pede 4.2, 1440p pede 5.0, 5120×1440 pede 5.1+. Resultado: perfis pequenos passam no NVENC, grandes falham em TODOS os MFTs enumerados (`nvenc`→`qsv`→`amf`, rank em `core/src/nvenc.rs:35-42`, enumeração em `510-552`) e o `VideoEncoder::new` Auto (`core/src/media.rs:722-752`) cai no fallback software com o log `probe failed (...); software fallback`.
- O fallback software tem teto próprio: `core/src/media.rs:834-845` (`fit_openh264_dims`) limita o lado longo a 1920 e o curto a 1080. Então resolução alta pedida vira 1080p software — o usuário vê "só as menores pegam no NVENC, as grandes viram outra coisa".
- Agravante: a UI oferece `5120×1440` (`app/web/src/views.tsx:117-122`) e teto 8192 (`views.tsx:124`, `core/src/media.rs:116`), mas NVENC H.264 tem teto prático ~4096 de largura. 5120 de largura falha mesmo com o nível corrigido. Hoje nada avisa isso na borda — o fallback é silencioso fora do session log + badge.
- SDP agrava o diagnóstico cruzado: `core/src/media.rs:1073-1101` (`video_codec()`) anuncia `profile-level-id=42e01f` (Baseline 3.1), enquanto o NVENC emite SPS de nível 4.1. `level-asymmetry-allowed=1` atenua, mas o par anuncia 3.1 e entrega 4.1 — decodificadores estritos (VideoToolbox no Mac) são os primeiros a reclamar. O software usa `level_for()` (`core/src/media.rs:849-860`: 3.1 até 720p, 4.1 até 1080p, 5.0/5.1 acima) — o NVENC deveria usar a mesma tabela.

### 2.2 Resolução do monitor (streamer)

- Cadeia de escala tem 3 estágios, todos redutores, nenhum faz upscale (`scale.min(1.0)` em `core/src/media.rs:183-191` `normalize_dims`):
  1. `app/src/screen.rs:132-139` (`profile_config`): fonte (`info.w/h` do enumerate) fit no perfil.
  2. `app/src/screen.rs:395` (`pump_bridge`): BGRA capturado fit no target de novo + `scale_bgra_bilinear` + `bgra_to_i420` no tamanho do target (economia correta, mas amolece texto).
  3. `core/src/media.rs:1775-1809` (`encode_target_for_frame` + `try_retarget_encoder`): retarget por frame + rebuild do encoder + `force_intra` + bump de geração.
- Fonte das dims: `platform-windows/src/dxgi.rs:19-61` (`enumerate_displays`) usa `DesktopCoordinates` do DXGI (coordenadas lógicas). Com DPI scaling ≠ 100% ou layout multi-monitor com escala por monitor, o `w×h` listado pode não ser o pixel físico — o usuário compara com "resolução do meu monitor" (física) e estranha. Não confirmado sem o `list_sources` da máquina dele; fica como hipótese nº 1 a checar.
- Ultrawide/ retrato: `normalize_dims` preserva aspecto, então fonte 21:9 dentro de perfil 16:9 (1920×1080) vira `1920×~800`, não 1080p cheio — parece "resolução errada", mas é o fit correto. O `1:1 nativo` (`views.tsx:233-240`) resolve pedindo o nativo, mas cai no teto do NVENC/software acima.
- `set_quality` em display é `StopFirst` (`platform-windows/src/lib.rs:152-165`, teste `restart_order_is_stop_first_for_dxgi_displays`): há um buraco breve (Duplication precisa fechar antes de reabrir). Em window é `NewFirst` (sem glitch). Trocar resolução no ar pisca em display por desenho, não é bug.

### 2.3 Áudio não aparece com o vídeo

Caminho completo lido (nenhum erro fatal visível, mas 3 pontos cegos que explicam "tem vídeo, não tem áudio"):

- Host cria o tap sempre para Display/Window: `app/src/lib.rs:621-625` (`ShareAudio::start(Vec::new()).ok()` + `subscribe()`).
- `app/src/audio.rs:42-59`: `ShareAudio::start` SEMPRE retorna `Ok`, mesmo com tap falho (`tap.ok()`); `live()` (`audio.rs:88-91`) denuncia. O `audio_rx` é inscrito mesmo sem tap — o SDP ganha `m=audio` (ver `core/src/media.rs:1237-1249`) mas nenhum pacote chega. Superfície: share parece "com áudio" no SDP e é mudo na prática.
- Tap Windows: `platform-windows/src/audio.rs:95-123` — sem exclusões vai para loopback do render device; `192-235` pede 48 kHz estéreo float; `245-297` codifica Opus 20 ms (`960*2` amostras) e `try_send` (cheio = descarta, correto). Falhas típicas silenciosas aqui: dispositivo render inexistente/desabilitado, modo exclusivo do driver, `initialize_mta` tardio, ou.springframework — todos viram `PlatformError::Internal` engolido pelo `.ok()` do `start_share`.
- Transporte: `core/src/media.rs:1284-1304` (thread `spawn_blocking` + `runtime.block_on(write_sample)`). Funciona, mas depende do `Handle::current()` capturado no `start` — se o `start_share` um dia sair do runtime, o áudio morre sem derrubar o vídeo.
- Viewer nativo: `app/src/pump.rs:1033-1037` cria `ViewerPlayback::start()` e só passa `on_audio` se o output local abriu. Se `cpal` falhar no viewer (sem output default, formato não F32/I16), `on_audio=None` e `core/src/media.rs:1980-1984` descarta a trilha de áudio sem erro — vídeo segue, silêncio total. `ViewerPlayback::push` (`app/src/audio.rs:203-212`) limpa o buffer se > 96k floats (estouro = corte, não chiado).
- O que falta para fechar o diagnóstico (não coletado porque a live estava no ar): linha do session log `share start kind=display profile=... audio=0/1` (`app/src/lib.rs:721-724`), badge `backend`/`backend_note` do `get_media_counters`, e `list_audio_apps` no share vivo. Se `audio=0`, o problema é captura (WASAPI); se `audio=1` e o viewer não ouve, é transporte/playback.

### 2.4 Mac não vê a tela do Windows

Hipóteses ordenadas por força (não há trace das duas pontas, então nenhuma está provada):

1. **Builds divergentes (forte, tem prova documental).** `BUGS.md` final: fixes da validação Display-3 estão NÃO-COMMITADOS e "o branch `fresh/native-core` do amigo não os contém". Se o Mac roda esse branch, faltam: fence de geração (`set_quality_never_writes_generation_backwards`), PLI/gap handling, `pli/max_gap` no trace, lane display do e2e. Sintoma esperado: conecta, recebe SPS de dims que não entende ou geração antiga, tela preta/congela. Checar: versão/commit do Mac vs `d7704e5` daqui.
2. **SDP 3.1 × SPS 4.1 (forte, prova no código).** Ver §2.1. O viewer nativo decodifica com OpenH264 (`core/src/media.rs:2084-2086`, `926-930`), que tolera; mas se o Mac assiste pelo path VideoToolbox/browser, o mismatch 42e01f × 4.1 é candidato a rejeição. O fix do nível dinâmico (§3.1) resolve os dois lados de uma vez.
3. **Rede sem TURN (média, por desenho).** `AGENTS.md` §6-7 + `core/src/media.rs:1052-1061` (`rtc_config` com STUN Google) e `media.rs:339-343` (rejeita `typ relay`). mDNS desligado dos dois lados (`media.rs:1007-1008`) = candidatos com IP literal (bom para debug, ruim para privacidade, irrelevante aqui). Se o Mac está fora da LAN/ZeroTier do host, hole-punching UDP pode falhar e não há TURN para salvar — ICE nunca conecta. Checar: census (`host/srflx`), `IceConnected` no viewer, e se ambos estão na mesma ZeroTier/rede.
4. **Áudio quebrando viewer antigo (fraca).** Oferta atual tem `m=audio` + `m=video` (`media.rs:2534-2564` testa os dois). Viewer antigo sem trilha de áudio ainda responde com `m=video` válido no path atual, mas build antiga pode rejeitar a m-line. Checar a answer do Mac (`m=video 0` = rejeitada).

### 2.5 Streamer + viewer ponta a ponta (o que foi verificado sem derrubar a live)

- Sinalização: server verde (11/11 scripts). Contrato em `server/PROTOCOL.md` (não re-lido aqui; sem erro de signaling nos testes).
- Encode→RTP→decode→present: coberto no core (67) e no bridge (fps clamp, downscale-antes-de-converter, GPU retained sem conversão em `app/src/screen.rs:848-888`).
- Trickle bilateral + `ice-complete`: código em `app/src/pump.rs` + `core/src/media.rs:1019-1050` (`add_ice_candidate_all_mlines`); testes de envelope passam no `app`? Não executaram aqui por causa do load 0xc0000139 — lacuna.
- Telemetria redatada: `MediaStats`/`CandidateCensus` sem SDP/candidatos/IPs (`media.rs:312-363`, `2194-2258`). Sessão logada sem segredos (`app/src/session_log.rs`, `lib.rs:395-399`).
- Janela de vídeo: abre no PRIMEIRO frame apresentado, nunca preta ociosa (`pump.rs:1020-1032`); `get_media_counters.presented` soma acks dos helpers (`lib.rs:1349-1362`). Sem frames apresentados no Mac = problema antes do helper (oferta/ICE/decode), não no helper.

## 3. Reparos indicados (ordem sugerida, cada um com verificação)

1. **NVENC: nível H.264 dinâmico por dims+fps (causa raiz do §2.1).** Arquivo: `core/src/nvenc.rs`, função `video_type()` (`656`) + `set_types()` (`601-616`). Mapear como `level_for()` (`media.rs:849-860`): ≤720p→3.1, ≤1080p→4.1 (4.2 se fps>30), ≤1440p→5.0, acima→5.1/5.2 conforme `windows::Win32::Media::MediaFoundation::eAVEncH264VLevel*` disponível no windows 0.62. Manter `ConstrainedBase`, GOP `fps*2`, `MeanBitRate`, `LowLatency`, sem CABAC/B-frames (head `d7704e5`/`17e682c` já nessa direção). Verificação: teste unit que constrói `video_type` por faixa e assert no `MF_MT_MPEG2_LEVEL`; e2e NVENC 1080p60 + 2560×1440 passando em `backend=nvenc` (hoje cai para `openh264`). Critério de aceite: `get_media_counters.backend` = `nvenc|qsv|amf` nas resoluções altas, não mais `openh264`.
2. **Honestidade para 5120×1440 no H.264.** Ou cap documentado (NVENC H.264 ≤4096 de largura: fitar para 3840×1080 com aviso no `backend_note`/UI) ou mover o preset para HEVC quando houver. Hoje `views.tsx:121` promete o que o MF não entrega. Verificação: `set_quality 5120×1440` retorna erro tipado OU aplica fit com `generation` bump + badge explicando — nunca fallback mudo.
3. **Áudio: falhar alto + expor no diagnóstico.** `app/src/lib.rs:622` trocar `.ok()` por log + `backend_note`-like para áudio; `get_media_counters` (ou `effective`) expor `audio_live` (já existe no session log `audio=0/1`, mas não no snapshot). `app/src/pump.rs:1033` logar `ViewerPlayback::start()=None` (hoje silêncio). Verificação: teste `ShareAudio::start` com tap falho reporta `live()=false`; viewer sem output loga e segue com vídeo.
4. **SDP: alinhar `profile-level-id` com o nível real ou fixar `42e01f` nos dois lados.** Opção A (preferida): encoder (software NVENC VT) emite o nível que o SDP anuncia por faixa. Opção B: anunciar o teto (4.1/5.x) com `level-asymmetry-allowed=1` mantido. Verificação: teste `offer_sdp_*` (`media.rs:2511-2564`) assert por perfil, e Mac decodificando oferta Windows.
5. **Mac: alinhar builds antes de qualquer outro debug.** Trazer o Mac para o mesmo commit (`d7704e5`+) ou commitar os fixes pendentes citados em `BUGS.md` e redistribuir. Depois, coletar do Mac: answer SDP (`m=video 0`?), `IceConnected`, `frames_decoded/keyframes_decoded`, `presented`, e census. Sem isso, qualquer mexida no host é tiro no escuro.
6. **Infra de teste: ressuscitar `cargo test --lib` no `app` no Windows.** `0xc0000139` = import faltando no PATH do harness (suspeitos: `openh264` DLL, `webrtc` nativo, `cpal/wasapi`, `windows` runtime). Sem isso, `screen.rs`/`pump.rs`/`lib.rs` seguem sem cobertura local. Verificação: `cargo test -p golive-app --lib` verde + `tests/smoke.rs` compilando (AGENTS.md §5).
7. **DPI/logical vs físico (hipótese §2.2).** Logar `info.w/h` do `enumerate` + dims do `profile_config` + target do bridge no session log (só números). Se divergir do painel do Windows, o fix é usar retângulo físico/DPI-aware no `dxgi.rs:48-50`.

## 4. O que NÃO foi mexido + próximos passos

- Nenhum arquivo de código foi editado; `git status --short` antes deste relatório: `M app/Cargo.toml`, `M app/gen/schemas/desktop-schema.json`, `M app/gen/schemas/windows-schema.json`, `M app/web/package-lock.json` (pré-existentes, intocados). Este relatório é o único arquivo novo.
- Para fechar §2.3/§2.4 na próxima janela (sem live no ar ou com segunda sala de teste): pedir ao host `get_media_counters` (backend, backend_note, effective, links), a linha `share start ... audio=` do session log, `list_audio_apps`; pedir ao Mac commit/versão, answer SDP (só `m=` lines + `a=rtcp-fb`, nunca candidatos), `IceConnected?`, `frames_decoded/presented`.
- Referências cruzadas: `AGENTS.md` §2 (protocolos são lei: `server/PROTOCOL.md`, GLV1), §5 (matriz de testes), §7 (`get_roster` pull, trickle bilateral, sem TURN); `BUGS.md` BUG-001/002/003 seguem abertos e compatíveis com este diagnóstico (emissor Windows ~13 fps, GPU ociosa sem NVENC, flap ICE).
