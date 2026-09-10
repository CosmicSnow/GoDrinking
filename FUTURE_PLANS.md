# Future plans — GoLive

Planos grandes, ainda **não implementados**. Não são bugs (`BUGS.md`).
Protocolos atuais (`server/PROTOCOL.md`, GLV1) continuam lei até uma
versão nova nascer nos **dois** lados ao mesmo tempo.

---

## 1. Viewer GPU: decode + present (salto tipo Parsec / Discord)

### 1.1 O problema que isto resolve

Hoje o **host Mac já comprime na GPU** (VideoToolbox). O engasgo e a
perda de nitidez no *viewer* vêm do caminho de **assistir**:

```text
RTP → OpenH264 na CPU → I420 → RGBA
    → socket GLV1 (cópia inteira do frame)
    → golive-video: scale bilinear CPU → softbuffer CPU → ack
```

Sessões locais medidas (Display 3, 720p60 / 1080p60) ficaram ~41–49 fps
com buracos de 0,5–2,6 s. O helper pinta na CPU; zoom/pan reescalavam o
quadro inteiro (cache de blit e pan tipo foto já mitigam *interação*,
não o present 60 fps).

Parsec/Discord não fazem esta cadeia. A placa gráfica:

1. descomprime o H.264
2. faz upsample de chroma (4:2:0 → ecrã) na GPU
3. desenha no ecrã (Metal / D3D)

O processador quase não vê pixels. Cores “iguais” e latência baixa saem
daí, não de um bitrate mágico.

**VideoToolbox não pinta o ecrã.** Só comprime/descomprime. “Usar VT no
present” é o nome errado. O plano certo é:

- **decode GPU** (VideoToolbox no Mac, D3D11 Video / Media Foundation /
  NVDEC no Windows)
- **present GPU** (Metal no Mac, DXGI/D3D11 no Windows)
- **zero cópia RGBA** no caminho quente (IOSurface / NT handle, não
  `u32 len + RGBA` por frame)

### 1.2 O que tem de permanecer intacto

- Sala wire v1 (`server/PROTOCOL.md`): signaling JSON, sem media, sem TURN,
  envelopes com fence, candidatos `typ relay` recusados.
- Trickle ICE bilateral (host → watcher e viewer → target).
- Erros redatados; títulos/pixels/SDP/tokens fora de logs.
- `golive-core` sem bindings de OS. Decode GPU vive atrás de um trait
  (espelho do `VideoEncoder` / `vt.rs`), stub que falha fechado fora do OS.
- Helper **continua a ser um processo** com o seu event loop: no macOS a
  thread principal é da Tauri; uma janela nativa por link watched não pode
  nascer no `golive-app`.
- Feeder + helper versionados juntos. Qualquer mudança de fio GPU é
  **GLV2** (ou flag no handshake), nunca um GLV1 “às vezes RGBA às vezes
  handle”.
- Browser mock (`app/web/src/mock.ts`) intacto até ordem explícita.
- Softbuffer/GLV1 fica como **fallback** (GPU indisponível, teste sem
  janela, `GOLIVE_DISABLE_HW`).

### 1.3 Duas arquitecturas (escolher uma na fase 0)

**A — Decode no core, handle no helper (preferida, mais Parsec)**

```text
read_loop: RTP → VT decompress → CVPixelBuffer/IOSurface
    → IPC: nome/handle do surface (não pixels)
    → helper Metal: textura a partir do IOSurface → drawable
```

- O core já tem o AU; o decode GPU substitui `H264Decoder` OpenH264 no
  viewer quando o probe passa.
- O helper deixa de receber megabytes RGBA; o socket só leva ids, dims,
  timestamp, “é keyframe?”.
- Present callback (`on_frame` / `PresentedFrame`) hoje é RGBA. Ou o
  present GPU **não** passa pelo callback de pixels (só stats: w/h/
  decoded++), ou `PresentedFrame` ganha um ramo `Gpu` opaco (tipo
  `GpuPixelBuffer` da captura, sem OS no core).

**B — Annex-B no helper, decode+present no helper**

```text
read_loop: RTP → AU Annex-B → GLV2 (NAL bytes, não RGBA)
    → helper: VT decode + Metal present
```

- O core viewer fica mais fino; o helper engorda (codec + GPU).
- Duplica política de PLI/stale-AU se o helper precisar pedir IDR.
- Empacotar VT/Media Foundation **dentro** do helper: dois sítios com
  decode, mais difícil de testar offline.

Recomendação: **A**, com fallback B só se o IPC de IOSurface/NT handle
se revelar indeployable (sandbox Tauri, codesign, helper sem entitlement).

### 1.4 Fio GLV2 (rascunho; versionar os dois bins)

Handshake actual GLV1: `GLV1` + u32 w + u32 h + u32 title_len + title,
depois `u32 len + RGBA`, ack `0x01`.

GLV2 (proposta, não implementar até a fase 0 fechar):

```text
magic:     b"GLV2"
hello:     u32 w + u32 h + u32 title_len + title + u8 present_mode
           present_mode: 0 = RGBA fallback (GLV1 frames)
                         1 = gpu-handle
frame RGBA (mode 0): igual ao GLV1 (len + bytes)
frame GPU  (mode 1): u32 w + u32 h + u32 handle_len + handle bytes
                     (Mach port name / IOSurface ID no Mac;
                      DXGI shared NT handle no Windows)
ack:       0x01 depois de present (ou depois de enfileirar, se
           latest-only no helper — ver 1.6)
EOF:       shutdown limpo, como hoje
```

Regras:

- Feeder **BINDA**, helper **CONNECTA** (igual).
- Dim change: hoje o shell respawna a janela (`present window respawn`).
  Com GPU, ou respawn (simples, já existe) ou `resize` no mesmo helper
  (menos flash). Primeira versão: **respawn**, não inventar resize GPU.
- Handle inválido / GPU down: uma linha de log kind-only, cair para
  RGBA nesse link (ou fechar a janela com erro `kind=error`). Nunca
  ecrã preto silencioso.
- Tamanho: handle é pequeno; o teto 256 MiB do RGBA deixa de aplicar
  ao ramo GPU.

### 1.5 Mac (VideoToolbox decode + Metal)

Probe (espelhar `vt.rs` encode):

- Sessão minúscula de **decompress** + um IDR sintético. Sucesso → GPU
  viewer. Falha → OpenH264 + GLV1, uma linha `backend`.
- `GOLIVE_DISABLE_HW=1` força fallback (testes).

Decode:

- `VTDecompressionSession` a partir do SPS/PPS do primeiro AU (já temos
  Annex-B no `read_loop`).
- Output `CVPixelBuffer` IOSurface-backed, 4:2:0 biplanar (NV12) ou
  4:2:2 se o decoder der — **não** converter para RGBA no CPU.
- Reorder: o produto é baixa latência; `kVTDecompressionPropertyKey_RealTime`
  / display immediately. Sem espera por B-frames no modo default
  (hoje o encode também tem `AllowFrameReordering = false`).
- Stale AU (`au_is_stale`) continua no `read_loop` **antes** de submit.

Present (helper):

- `CAMetalLayer` no `winit` (raw window handle) ou janela AppKit mínima
  só no helper — **não** no processo Tauri.
- Textura `MTLTexture` a partir do `IOSurface`; blit/fragment fullscreen
  com letterbox (contain) no shader, não bilinear CPU.
- Zoom/pan: uniforms no shader (crop do UV), **sem** reescalar um
  bitmap 4×. Isto é o que falta para zoom tipo foto sem travar.
- Vsync: `present` no drawable; se o blit atrasar, **latest-only** (o
  feeder já não deve bloquear no ack — ver 1.6).
- Entitlements: o helper precisa do mesmo sandbox/codesign que o app
  para receber IOSurface entre processos (Mach port). Validar no
  empacotado (`Contents/MacOS/golive-video` ao lado do bin, como hoje).

Cores:

- Anexar `kCVImageBufferYCbCrMatrixKey` / primaries / transfer no
  buffer (709 para sRGB). Shader faz YUV→RGB. Sem isto o “YouTube
  vermelho” continua sujo mesmo na GPU.
- 4:2:0 **não desaparece** (é H.264). A GPU só faz upsample decente.
  4:4:4 seria outro codec/perfil e outro plano.

### 1.6 Ack, latest-only, zoom (não desfazer o que já existe)

Já feito (não reverter):

- Feeder não bloqueia no ack (`drain_present_acks` nonblocking).
- Inbox do helper é latest-only (não fila de 2 blits).
- Pan tipo foto (`ox/oy` segue o cursor); cache do blit CPU no zoom.

GLV2 deve **manter** latest-only. Ack passa a significar “o helper
pegou/apresentou este generation”, não “o CPU acabou o bilinear”.
Contadores `presented` continuam a subir no ack; `decoded` no
`read_loop`. Não voltar a medir bitrate RGBA como se fosse H.264
(`bitrate_note` já avisa).

### 1.7 Windows (espelho, atrás do mesmo trait)

- Decode: `ID3D11VideoDecoder` ou Media Foundation; NVDEC se existir,
  senão software (OpenH264) com o mesmo fallback honesto.
- Present: D3D11 + `DXGISwapChain` no helper; shared texture via NT
  handle (`IDXGIResource1::CreateSharedHandle`).
- `cfg` só no ponto de selecção (`app/src/video/` + `core` trait),
  nunca espalhado. `platform-windows` **não** ganha RTP/WebRTC.
- Cross `cargo xwin check` no host Mac, como hoje.
- Sem GPU no viewer Windows, o sintoma BUG-002 (GPU alta *só a
  assistir*) hoje é o contrário: CPU 100% no blit. GPU present no
  viewer **usa** a 3090 para o que ela serve; encode no host Windows
  continua OpenH264 até haver NVENC (plano 2).

### 1.8 Fases (evidência em cada uma)

| Fase | Entrega | Como saber que está feito |
|------|---------|---------------------------|
| 0 | ADR curto neste ficheiro: A vs B; esboço GLV2; lista de entitlements | Decisão escrita; sem código GPU |
| 1 | Trait `GpuDecoder` no core + stub + teste de probe/fallback sem OS | `cargo test` core; `GOLIVE_DISABLE_HW` |
| 2 | VT decompress → `CVPixelBuffer` (ainda convertendo a RGBA para GLV1) | Viewer Mac: backend `videotoolbox-decode` no session log; imagem igual |
| 3 | GLV2 mode 1 + Metal present no helper; zoom/pan no shader | Sem RGBA no socket no caminho feliz; zoom 4× sem bilinear CPU |
| 4 | Fallback GLV1 se probe falhar; respawn de dims como hoje | Desligar GPU no SO → janela CPU, sem crash |
| 5 | Windows decode+present; `xwin check`; e2e skip honesto sem janela | Paridade de comportamento, não de fps |
| 6 | Medir: `scripts/debug-media.sh` + analyzer; session log sem spam `ice connected` | Display 3 local: present ~ captura, sem buracos de 2 s |

Não misturar fase 3 com mudança de perfil H.264 / B-frames / wire Sala.

### 1.9 Riscos

- **Sandbox / codesign:** IOSurface entre `golive-app` e `golive-video`
  pode falhar no `.app` empacotado e funcionar no `cargo tauri dev`.
  Testar os dois. Se falhar, plano B (Annex-B no helper) ou helper
  com XPC.
- **Thread principal:** Metal no helper, nunca no bin Tauri.
- **Dim change / 1:1 / resize de janela capturada:** o encode já pode
  mudar de tamanho; o viewer respawna. GPU present deve usar o mesmo
  critério (`window_fits` / `respawn_line`).
- **Dois viewers:** N helpers, N sessões VT. Limitar ou partilhar
  device Metal; não N `D3D11CreateDevice` sem teto.
- **Testes:** sem janela, skip honesto (já existe). Não exigir GPU nos
  unitários. Softbuffer permanece para `cargo test --bin golive-video`.

### 1.10 Explicitamente fora deste plano

- Mudar `server/PROTOCOL.md` ou ligar TURN.
- B-frames / modo “qualidade OBS” (é o plano 3, abaixo).
- NVENC no **host** Windows (plano 2).
- Present dentro da WebView Tauri (WebGL): viola a janela nativa por
  link e o event loop.
- Remover OpenH264: é o fallback e o Windows até NVENC.

---

## 2. Host Windows: encode GPU (NVENC / AMF)

Hoje o host Windows é OpenH264 CPU + readback DXGI. A 3090 no viewer
não ajuda o *envio*. Quando o plano 1 estiver estável no Mac:

- Encoder hardware atrás do mesmo `EngineKind::Auto` (probe → NVENC
  ou AMF, senão OpenH264).
- DXGI/WGC já têm textura D3D11: submit sem `copy_tight_bgra` no
  caminho feliz (espelho do IOSurface → VT no Mac).
- Sem isto, 1080p60 no host Windows continua o tecto ~13 fps medido
  (BUG-001 / BUG-002).

---

## 3. Modo latência vs qualidade (B-frames)

Pedido: interruptor “baixa latência” (hoje) vs “melhor qualidade”
(~0,3–0,8 s, B-frames, tipo OBS).

- Só faz sentido **depois** do present GPU. B-frames no caminho CPU
  actual pioram o delay e não tapam o blit.
- Encode Mac: `AllowFrameReordering = true` + 1–2 B-frames no VT;
  decode tem de reordenar (fase 1.5 já deve permitir).
- Default do produto **permanece** baixa latência (screen share).
- SDP continua nosso (os dois peers são GoLive); não é mudança Sala.
- OpenH264 no Windows ganha pouco; não vender o modo como “OBS” lá
  até o plano 2.

---

## 4. Higiene já identificada (pequena, pode entrar antes)

- **BUG-003 log:** `read_loop` manda `Stats { ice_connected: true }`
  a cada 30 frames; `apply_ice_connected` grava `ice connected` sempre.
  Dedupe no log (só transição). Não prova flap ICE real.
- Não spammar `quality applied` em tempestade de gerações no mesmo
  milissegundo (já se viu 4–6 applies ao mudar 720↔1080).
- Session log não substitui `GOLIVE_TRACE_DIR` + `analyze-trace.py`
  para fps/CPU.

---

## Ordem sugerida

1. Plano 1 fases 0–4 (viewer GPU Mac, fallback CPU).  
2. Higiene do session log (plano 4).  
3. Plano 1 fase 5 (Windows viewer GPU).  
4. Plano 2 (NVENC host).  
5. Plano 3 (interruptor B-frames), só se ainda fizer falta.
