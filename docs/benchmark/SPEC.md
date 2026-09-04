# SPEC — Benchmark local

Data: 2026-09-04  
[PRD](./PRD.md).

## Seams (TDD)

| Seam | Onde | Observável |
|------|------|------------|
| Classificador | `recommend_preset(probe)` puro | High/Medium/Low + nota |
| Limiares | constantes | 12 ms / 20 ms / drop |

Não testar: captura real, VT, NVENC. O probe devolve um DTO; o classificador é determinístico.

## DTO

```
ProbeSample {
  preset: low|medium|high,
  width, height, fps,
  encoder: software|hardware,
  mean_encode_ms,
  drop_ratio,          // 0.0..1.0
  codec_ok: { h264, hevc, av1 }
}

ProbeReport {
  samples: ProbeSample[],
  recommended: low|medium|high,
  note: String,        // uma linha, UI
  can_1440: bool,
  av1_encode: bool,
  hevc_encode: bool,
  at: unix_ms
}
```

## Classificador

```
if high.hardware && high.mean_encode_ms < 12 && high.drop_ratio < 0.02 → High
else if medium.mean_encode_ms < 20 && medium.drop_ratio < 0.05 → Medium
else → Low

can_1440 = hardware && a sample 2560×1440@30 (se corrida) mean < 16 ms
```

Se High nem chegou a correr (encoder recusou o tamanho): não é High.

## Comando Tauri

`run_media_benchmark() -> ProbeReport`

- `async` + `spawn_blocking`. Não bloquear o main (picker/TCC).
- Windows: frame sintético BGRA (padrão) se WGC for pesado demais para um probe; preferir uma captura real de 2 s do ecrã principal **sem** mostrar o picker, se a API deixar. Se precisar de picker, abortar com “precisa de permissão de captura” — não deadlocks.
- macOS: **não** chamar `getShareableContent` (TCC nag). Frame sintético 1920×1080 + 3440×1440 é suficiente para encode timing. Nota da UI: “Medi o encoder, não o teu monitor.”
- Nunca cria `LanRoom`, `PeerTransport`, nem HTTP ao Rendezvous.

## UI

- Botão na Customize: “Medir este PC”.
- Vista simples: link de 11px “Qual qualidade neste PC?”
- Resultado: `note` + selecciona o segmented.
- localStorage key `godrinking.benchmark`.

## Invariantes

- Zero Media na rede.
- Zero frames no Rendezvous.
- Não altera codec da sessão a correr (benchmark só com Session idle).

## Aceite

- [ ] “Medir este PC” na Customize e um link na vista simples.
- [ ] Com Session idle, devolve `recommended` + `note` numa linha e selecciona Low/Medium/High.
- [ ] Com Session a correr, recusa com mensagem clara (não mexe no codec).
- [ ] Não abre LAN/Stunar, não cria PeerTransport.
- [ ] Resultado em `localStorage` `godrinking.benchmark`.
