# PRD — Join modes, Roster, Rendezvous

Data: 2026-09-01  
Produto: goDrinking  
Âmbito: documentação para implementação. Não inclui captura, encode, nem qualidade de vídeo.

Implementar na ordem dos PRDs. Não saltar PRD-20: o Roster precisa de mais do que um Viewer.

---

## PRD-13 — Seletor de Join mode

**Problema:** Só existe LAN com Room code. Direct e Stunar não têm onde viver na UI.

**Regras:**

- Host e Viewer escolhem um Join mode: **LAN**, **Direct**, **Stunar**. Default: LAN.
- Os dois lados da mesma Session usam o mesmo modo. Não há fallback silencioso (Direct não tenta LAN por baixo).
- O seletor está visível **antes** de Start (Host) e **antes** de Join (Viewer).
- Mudar de modo com a Session a correr não é permitido. Stop primeiro.

**Done when:** A UI mostra os três modos, o modo escolhido vai no comando de Start/Join, e LAN continua a entrar só com Room code.

---

## PRD-14 — Direct

**Problema:** Fora da LAN, o Room code não atravessa a internet. O Host quer mandar um endereço, não um código.

**Regras:**

- Host Start em Direct **não** pede Room code ao Viewer.
- Depois do Start, o Host mostra, copiáveis:
  - IPv4 local (se existir)
  - IPv4 público (STUN e/ou mapeamento UPnP/NAT-PMP/PCP, se existir)
  - IPv6 global (se existir)
  - porta TCP de Signaling
- “UDP automático”: dual-stack quando o OS deixar; STUN para srflx IPv4; UPnP/NAT-PMP/PCP best-effort para a porta de Signaling; ICE com STUN para Media.
- Viewer Join: campo de endereço (IPv4 ou IPv6), porta, Password (se o Host tiver), Nickname. Sem Room code.
- IPv6 no campo: forma com parênteses se precisar de porta, ex. `[2001:db8::1]:41234`.
- Se UPnP falhar, a UI **não mente**. Mostra os endereços que tem e um aviso de que o Direct pela internet pode falhar sem porta aberta.

**Done when:** Dois PCs na mesma LAN ligam por IP:porta sem broadcast. Dois PCs com IPv6 global ligam por IPv6. O Host nunca mostra só “código”.

---

## PRD-15 — Password e Ignore list no Host

**Problema:** Um endereço público + porta aberta é isco para scanners.

**Regras:**

- Password é **opcional** (LAN/Direct). Vazia = qualquer um que saiba o endereço (Direct) ou o Room code (LAN) pode pedir para entrar. **Stunar: obrigatória** (4–64) — o Rendezvous recusa `open` sem Password.
- Se preenchida: 4–64 caracteres. Nunca logada. Nunca no SDP.
- Signaling Direct/LAN **não** envia offer antes de AUTH ok.
- Ignore list no processo do Host: falhas do mesmo IP em 10 minutos escalam a duração — **5** → 15 min, **10** → 1 h, **15** → 6 h, **20+** → 24 h. Ligação TCP fecha na hora. Sem mensagem rica (`ERR BANNED` chega).
- Falhas de Admission (Host recusou) **não** contam para a Ignore list.
- A Ignore list é memória do processo. Morre com o Stop.

**Done when:** Password errada nunca recebe SDP. Falhas do mesmo IP escalam a Ignore list (5→15 min, 10→1 h, 15→6 h, 20+→24 h). Password certa de outro IP entra.

---

## PRD-16 — Stunar (Rendezvous)

**Problema:** Código curto na internet precisa de um sítio que mapeie código → Host, sem ver o ecrã.

**Regras:**

- URL HTTPS do Rendezvous é configuração do app (default documentado, editável). Sem URL, Stunar recusa Start/Join com erro claro.
- Host em Stunar: Nickname + Password (**obrigatória**, 4–64) + Admission. O Room code é **gerado pelo servidor** no `open` (6 chars A–Z0–9) e devolvido na resposta com o `host_token`; o Host não o escolhe. “Telefona” `open` e depois Heartbeat.
- Heartbeat a cada **30s**. Sem Heartbeat **5 minutos** → a sala deixa de existir. Pedidos a essa sala = `denied` genérico.
- Viewer: mesmo Room code, Password, Nickname.
- O Rendezvous **não** devolve IP, SDP, Roster nem existência da sala a quem falhe Password.
- Depois de aceite: reencaminha Signaling (offer/answer) entre aquele Host e aquele Viewer. Zero Media.
- Sem listagem de salas. Sem pesquisa. Sem contas.

**Done when:** Host e Viewer em redes diferentes, com o Rendezvous no meio, trocam SDP e a Media corre P2P. O código que o Viewer escreve é o que o servidor devolveu no `open`. Matar o Heartbeat 5+ min faz o Join falhar. tcpdump no Rendezvous não mostra RTP.

---

## PRD-17 — Nickname, Roster, Kick, Admission

**Problema:** O Host não sabe quem está a pedir entrada nem consegue mandar embora.

**Regras:**

- Nickname obrigatório nos dois lados, 2–24 caracteres, imprimíveis, trim. Não é único. Colisões mostram o mesmo nome + id curto no Roster.
- Roster no Host, **os três modos**:
  - Pending (Admission ligada)
  - Connected
- Admission é um toggle do Host, default **desligado**. Ligado: cada Viewer fica Pending até Accept ou Reject. Desligado: Password+código/endereço bastam.
- Accept dispara Signaling. Reject fecha o pedido. Kick desliga um Connected; o WebRTC dessa pessoa fecha; ela não reentra com o Token antigo.
- Viewer vê: à espera / recusado / ligado / kick. Sem ver o Roster dos outros.

**Done when:** Host com Admission vê o Nickname, aceita, o ecrã chega. Kick corta a Media desse Viewer e os outros continuam.

---

## PRD-18 — Rodar Password ou Room code ao vivo

**Problema:** O Host quer trocar o segredo sem deitar abaixo quem já está dentro.

**Regras:**

- Host pode editar Password (os três modos) com a Session a correr. Room code: só LAN — no Stunar o código é do servidor e não roda.
- Quem **já está Connected** permanece. Tokens e PeerConnections atuais continuam válidos.
- Pedidos **novos** usam só os valores novos.
- No Stunar, `rotate` só aceita Password nova (e Admission); o código não muda.
- No LAN, o broadcast passa a responder só ao código novo.
- Direct não tem Room code; só a Password nova vale para AUTH novo.

**Done when:** Dois Viewers ligados. Host muda a Password. Eles continuam a ver. Um terceiro com a Password antiga é `denied`. Com a nova, entra (ou fica Pending).

---

## PRD-19 — Vários Viewers

**Problema:** O código de hoje guarda um offer e um answer. Roster e Kick não fazem sentido com um único lugar.

**Regras:**

- Máximo **8** Viewers Connected por Session.
- Cada Viewer tem o seu `PeerTransport` no Host (offer próprio).
- Cheio → Join recusa `full`. Não derruba os que já estão.

**Done when:** 2 Viewers assistem a mesma Session ao mesmo tempo. Kick num não afeta o outro.

---

## PRD-20 — Licença no repo

**Done when:** `/LICENSE` é PolyForm Noncommercial 1.0.0 com `Required Notice: Copyright CosmicSnow (goDrinking)`. `package.json` e `src-tauri/Cargo.toml` levam `license = "PolyForm-Noncommercial-1.0.0"`. README deixa de dizer “Ainda não”.
