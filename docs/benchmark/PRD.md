# PRD — Benchmark local

Data: 2026-09-04  
Produto: goDrinking  
Âmbito: no PC do Host, medir o que aquele hardware aguenta e **recomendar** Low / Medium / High (e avisar Customize). Não envia frames para a rede. Não usa o Rendezvous.

Branch beta, depois dos defeitos, pode ir junto com Salas.

---

## PRD-B1 — Correr o probe

**Regras:**

- Botão “Medir este PC” na Customize (e um atalho discreto na vista simples: “Não sabes qual usar? Mede.”).
- Dura ≤ 8 s. Captura um ecrã (ou um frame sintético se o picker for demais) e encode local nos três orçamentos.
- Mede: tempo médio de encode, frames dropped no cap, se hardware encoder levantou, se AV1/HEVC existem.
- Não abre Session, não publica Room code, não fala com Viewer.

**Done when:** clicar Medir não inicia Stunar/LAN; no fim há uma recomendação.

---

## PRD-B2 — Recomendação

**Regras:**

| Resultado | Recomenda |
|-----------|-----------|
| Encode High 1080p60 hardware < 12 ms/frame, drop ≈ 0 | High |
| Encode Medium 1080p30 < 20 ms, High dropa | Medium |
| Só 720p30 estável, ou só software lento | Low |
| 1440p/4K hardware folgado | High + nota “Customize aguenta 1440p” |

- Mostra o porquê numa linha: “NVENC ok, 1080p60 a 6 ms. High.”
- Integrada: GTX 1050 / iGPU → provavelmente Low/Medium. M1+ VT → Medium/High. RTX 30/40 → High.
- Nunca escolhe HEVC/AV1 para o utilizador. Só menciona: “AV1 encode existe neste Mac; usa no Customize se o Viewer também tiver AV1.”

**Done when:** a recomendação preenche o segmented Low/Med/High; Customize continua intacto.

---

## PRD-B3 — Persistência

**Regras:**

- Guarda o último resultado em localStorage (timestamp, preset, encoder, notas).
- Não é lei: o Host pode ignorar.
- Voltar a medir substitui.

**Done when:** reabrir o app mostra “Última medição: High · há 2 dias” até haver outra.
