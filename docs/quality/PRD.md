# PRD — Qualidade, UX e codecs (defeitos)

Data: 2026-09-04  
Produto: goDrinking  
Âmbito: consertar a transmissão P2P para parecer Discord (cores, resolução, bitrate) sem passar Media pelo Rendezvous. Identidade visual (`App.css`) permanece.

Implementar na ordem. Não saltar PRD-Q5.

Seams de teste (TDD): `fitted_even_size`, `bgra_to_nv12`, `resolve_floor` / `EncoderControl`, catálogo de codecs, copy/presets na UI.

---

## PRD-Q1 — Ultrawide e resoluções acima de 1080p

**Problema:** Host com monitor ultrawide (3440×1440, 5120×1440) ou 1440p/4K falha ou entrega um letterbox estreito. O cap atual é uma *caixa* 1920×1080: 3440×1440 vira 1920×804 e desperdiça o orçamento H.264.

**Regras:**

- A saída **preserva aspecto** e **nunca faz upscale**.
- O cap de um preset é um **orçamento de pixels** (área do teto), não um retângulo 16:9 rígido. Ultrawide usa a área toda (ex. ~2.07 MP no High) mesmo que a largura passe de 1920.
- Presets Low / Medium / High ficam dentro do H.264 Baseline nível 4.2 (MaxFS 8704), para qualquer Viewer decodificar. High continua ~1080p de área, não 4K.
- Customize pode pedir 1440p ou 2160p. Aí o codec sobe de nível (H.264 High / HEVC / AV1) e a UI avisa incompatibilidade.
- Encoder software (OpenH264) usa o mesmo orçamento; não recebe o frame nativo de 29 MB.
- Dimensões finais são pares.

**Done when:** 3440×1440 no High cabe no orçamento ~1080p sem estourar o nível 4.2; 5120×1440 também; 1280×720 não é esticado; 1440p/4K só no Customize.

---

## PRD-Q2 — Espaço de cores

**Problema:** o Viewer vê cores lavadas ou diferentes do Host. NV12 usa BT.601; OpenH264 sinaliza BT.709 full; o Mac captura Display P3 sem tag.

**Regras:**

- Conversão BGRA→YUV é **BT.709 limited** (Y 16–235, UV 16–240).
- Encoders anunciam VUI / propriedades **BT.709**, range limitado.
- ScreenCaptureKit pede `kCGColorSpaceITUR_709` na stream.
- VideoToolbox: ColorPrimaries, TransferFunction e YCbCrMatrix = 709.
- Media Foundation NV12 usa a mesma conversão 709.

**Done when:** vermelho sólido dá luma ~63 (709), não ~81 (601); o Viewer no mesmo preset vê saturacão próxima do preview do Host.

---

## PRD-Q3 — Bitrate: o alvo é o alvo

**Problema:** dois sliders (teto e piso). O Viewer sente o piso (1 Mbps default). REMB pessimista do WebView derruba o encoder mesmo sem perda.

**Regras:**

- Presets: **um** número. Low 1.5 / Medium 4 / High 8 Mbps. Sem sliders na vista simples.
- O encoder **arranca no alvo**.
- REMB só **baixa** o encoder se houver perda recente (RTCP fraction-lost). Sem perda, ignora o chute baixo e continua no alvo (ou no probe para cima).
- Piso automático = 25% do alvo (mínimo 250 kbps), nunca um 1 Mbps absoluto. Customize pode sobrescrever.
- Probe sobe +25%/s em caminho limpo até o alvo.

**Done when:** sessão High sem perda fica perto de 8 Mbps no Status do Viewer; o piso não aparece na vista simples; um slider de piso na Customize não é o valor que a sessão “escolhe sozinha”.

---

## PRD-Q4 — Vista simples vs Customize

**Problema:** leigos não configuram; power users precisam de codec, 1440p, 120 fps, encoder.

**Regras:**

- Vista simples: fonte, Low/Medium/High, áudio de sistema + exclusão, Join mode, Start.
- **Customize** (fechado por omissão): resolução, fps, codec, encoder (Windows), bitrate, piso.
- Presets **sempre** forçam H.264 Baseline (digestível Mac↔Win, GTX 1050+ e M1+).
- Customize codecs: H.264, H.264 High, HEVC (macOS), AV1 (quando o Host consegue encode). Cada um tem aviso curto de quem consegue assistir.
- Codec e resolução acima de 1080p são fixos no Start (renegociar a meio rebenta o Peer).

**Done when:** um Host novo só vê três botões de qualidade; abrir Customize mostra o resto; Low/Medium/High nunca escolhem AV1/HEVC sozinhos.

---

## PRD-Q5 — Copy (sem AI slop)

**Problema:** títulos genéricos, mistos EN/PT, hints fora do sítio.

**Regras:**

- Manter Manrope + DM Mono, lima `#d8ff68`, painéis escuros.
- Tom do README: curto, concreto, sem “native media capability”.
- Labels no sítio: qualidade explica o preset; Customize explica o risco do codec.
- “Local only” no topbar some ou muda quando o Join mode é Stunar/Direct (não mentir).
- Versão no sidebar segue `package.json` (0.2.0 neste release).

**Done when:** nenhum hint de bitrate/piso na vista simples; headings dizem o que o ecrã faz; Stunar não diz “Local only”.

---

## PRD-Q6 — Codecs e hardware misto

**Problema:** AV1 Mac→NVIDIA antiga ou WebView Win pode falhar; H.264 Baseline passa em tudo o que suportamos.

**Regras:**

| Preset | Codec | Teto | fps | Alvo |
|--------|-------|------|-----|------|
| Low | H.264 Baseline 4.2 | orçamento 720p | 30 | 1.5 Mbps |
| Medium | H.264 Baseline 4.2 | orçamento 1080p | 30 | 4 Mbps |
| High | H.264 Baseline 4.2 | orçamento 1080p | 60 | 8 Mbps |

Customize:

- H.264 — default, todos.
- H.264 High — 1440p/4K; Viewer precisa de High profile.
- HEVC — Host macOS; Viewer com decode H.265.
- AV1 — Host com encode (VideoToolbox M3+); Viewer com decode AV1. Aviso: GTX 10-series / decode software pode engasgar.

Se o answer rejeitar o m=video, a UI diz para voltar a H.264. Media continua P2P.

**Done when:** presets não oferecem AV1/HEVC; Customize com AV1 no Mac que não tem encoder recusa com mensagem; Mac→Win em H.264 High/preset continua a ligar.

---

## Fora deste pacote

Salas, Benchmark, Play Together: `docs/rooms`, `docs/benchmark`, `docs/play-together`. Rendezvous continua sem Media (ADR 0003).
