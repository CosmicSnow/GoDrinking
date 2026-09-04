# SPEC — Qualidade, UX e codecs

Data: 2026-09-04  
Ler o [PRD](./PRD.md) primeiro. Vocabulário em `/CONTEXT.md`.

## Seams (TDD)

| Seam | Onde | Observável |
|------|------|------------|
| Fit de captura | `types::fitted_even_size` | (w,h) pares, aspecto, orçamento de pixels |
| Cor | `access_unit::bgra_to_nv12` | vermelho/preto 709 limited |
| Piso | `types::resolve_floor` | 25% do alvo por omissão |
| Congestão | `pipeline::EncoderControl` | REMB sem perda não desce; com perda respeita piso |
| Codec de preset | `types::VideoCodec` + request | Low/Med/High → H.264 |
| Bitrate de preset | `pipeline::encoder_bitrate` | escala por pixels reais, teto = preset |

Não testar: threads SCK, VT FFI, MFT, WebView.

## Fit (PRD-Q1)

Substituir o letterbox `min(maxW/srcW, maxH/srcH)` por:

1. Sem upscale.
2. `scale = min(1, sqrt((maxW*maxH) / (srcW*srcH)))`.
3. `w,h` arredondados para par, para baixo.
4. Se `w*h` ainda exceder o orçamento, decrementar o eixo maior de 2 até caber.
5. H.264 Baseline 4.2: `ceil(w/16)*ceil(h/16) ≤ 8704`. Se falhar, reduzir na mesma regra.

Exemplos (High / 1920×1080 de orçamento):

| Fonte | Resultado esperado |
|-------|-------------------|
| 1920×1080 | 1920×1080 |
| 1280×720 | 1280×720 |
| 3440×1440 | ~2226×932 (não 1920×804) |
| 5120×1440 | ~2714×762 |
| 2560×1440 | 1920×1080 |

Windows `encode_ceiling` / `fit_within` e macOS `stream_configuration` usam a mesma função. OpenH264 deixa de ter teto-caixa 1920×1080 separado: usa o tamanho já fitted.

## Cor (PRD-Q2)

BT.709 limited, inteiros:

```
Y  = 16 + (47*R + 157*G + 16*B) >> 8     // clamp 16..235
Cb = 128 + ((-26*R - 87*G + 112*B) >> 8) // clamp 16..240
Cr = 128 + ((112*R - 102*G - 10*B) >> 8)
```

Sólidos (2×2):

- Preto (0,0,0): Y=16, UV=128
- Vermelho (255,0,0): Y ∈ [60,68], Cr > 220, Cb < 120

OpenH264: `VuiConfig` 709 **limited** (não full), se a API permitir; senão 709 e a conversão already limited.

VideoToolbox (`video_toolbox_encoder.swift`):

- `kVTCompressionPropertyKey_ColorPrimaries` = ITU_R_709_2
- `kVTCompressionPropertyKey_TransferFunction` = ITU_R_709_2
- `kVTCompressionPropertyKey_YCbCrMatrix` = ITU_R_709_2

SCK: `setColorSpaceName(kCGColorSpaceITUR_709)`. Feature `CGColorSpace` em `objc2-core-graphics`.

## Bitrate (PRD-Q3)

`DEFAULT_FLOOR_BPS` deixa de ser 1_000_000.

```
resolve_floor(target, override) =
  clamp(override or target/4, MIN_BITRATE, target)
```

`EncoderControl::set_congestion_bitrate`:

- Se `applied` ainda é o alvo e não há perda recente (`LOSS_WINDOW` + `LOSS_FREEZE_FRACTION`): **não aplicar** descida.
- Se há perda recente: `clamp(remb, floor, target)` como hoje.
- Probe inalterado (+25%, min +200 kbps, quiet 3 s).

UI: sliders só dentro de Customize. Vista simples manda `bitrate_bps: null`, `min_bitrate_bps: null`.

## Codecs (PRD-Q6)

`VideoCodec`: `H264`, `H264High`, `Hevc`, `Av1`.

Presets no frontend zeram codec para `h264` ao clicar Low/Medium/High.

SDP:

- H.264: `profile-level-id=42e02a` (preset) ou `640033` (High, nível 5.1 se 1440p+)
- HEVC: `video/H265` (já existe)
- AV1: `video/AV1` (`MIME_TYPE_AV1` no webrtc-rs 0.14). Encode só se `av1_encode_supported`. Packetizer: `TrackLocalStaticSample` como H.264.

Se `rejected_video_section`, notice: “O outro PC recusou este codec. Volta para H.264.”

## UI

- `qualityOpen` passa a chamar-se Customize e envolve resolução, fps, codec, encoder, bitrate, piso.
- Segmented Low/Medium/High fica sempre visível, com uma linha: `720p30 · 1.5 Mbps` / `1080p30 · 4 Mbps` / `1080p60 · 8 Mbps`.
- Topbar: LAN → “Na tua rede”. Direct/Stunar → “P2P · o servidor não vê o vídeo”.
- Copy em `src/copy.ts` (constantes). `App.tsx` não inventa frases soltas.

## Invariantes

- Media nunca no Rendezvous.
- Viewer continua JS `RTCPeerConnection`; Host `webrtc-rs`.
- Sem TURN.
- Sem mudança de identidade visual (cores, tipografia, layout de painéis).
