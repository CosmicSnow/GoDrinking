# Bugs — GoLive (revisar)

> Regra: bug 100% corrigido **e verificado** SAI desta lista (ver
> AGENTS.md, seção Bugs). Nunca marcar como feito no lugar — remover a
> linha. A lista contém só bugs abertos.

| ID      | Sintoma                                                        | Status         | Desde                    | Suspeita / notas |
|---------|----------------------------------------------------------------|----------------|--------------------------|------------------|
| BUG-001 | Vídeo em slow motion após o PLI (Mac→Mac e Mac→Win)            | open           | pós-PLI (`core` PLI + `force_intra`) | Suspeita inicial: flag `force_intra` presa (IDR em todo frame explode bitrate/CPU) ou storm de PLI (debounce insuficiente). Confirmar via `link_stats`: bitrate alto + `keyframes_decoded` alto durante a lentidão. |
| BUG-002 | Windows lento + GPU ~40% de RTX 3090 só assistindo             | open (windows) | build Windows pós-DXGI   | Lado Windows (LLM Windows): checar decode por software, present loop sem vsync, upload de textura por frame. |
