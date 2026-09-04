# PRD — Modo Sala

Data: 2026-09-04  
Produto: goDrinking  
Âmbito: segundo modo de sessão. O modo atual (1 Host, N Viewers) **permanece** e é o default. Media continua P2P. O Rendezvous só apresenta pessoas e reencaminha Signaling.

Implementar depois do pacote de qualidade, numa branch própria, release **beta**.

---

## PRD-R1 — Seletor de modo de sessão

**Problema:** só existe Broadcast (um Host). Amigos querem ver a tela uns dos outros na mesma reunião.

**Regras:**

- Antes do Start, o Host escolhe **Broadcast** ou **Sala**. Default: Broadcast.
- Broadcast = comportamento atual: um Host captura; Viewers só assistem.
- Sala = todos os membros podem capturar; todos podem assistir qualquer captura activa.
- Mudar o modo com a Session a correr não é permitido.

**Done when:** a UI mostra os dois modos; Broadcast não pede partilha ao Viewer; Sala mostra “partilhar a minha tela” a quem entrou.

---

## PRD-R2 — Abrir e entrar numa Sala

**Regras:**

- Quem abre a Sala é o primeiro **Master**. Define a Password (Stunar: obrigatória 4–64, como hoje).
- O Rendezvous gera o Room code (6 chars) e guarda metadados da Sala **enquanto houver pelo menos um membro com Heartbeat**.
- Entrar: Room code + Password + Nickname. Sem listagem pública.
- LAN/Direct também têm Sala: o código/endereço continua local; o Master vive no processo de quem abriu, até ceder a coroa.

**Done when:** um terceiro PC entra depois com o mesmo código/senha e vê quem já está; o Rendezvous não tem RTP.

---

## PRD-R3 — Master, coroa e encerramento

**Regras:**

- Master: altera Password, liga/desliga Admission, kicka.
- Se o Master sai, a coroa vai para quem **entrou a seguir** e ainda está dentro (ordem de admissão).
- Se o último membro sai, a Sala **acaba**. Código deixa de funcionar.
- Kick: o alvo fecha Media e Signaling; não reentra sem código+senha (e Admission, se houver).

**Done when:** testes de sucessão (A sai → B Master; A e B saem, C sozinho é Master; todos saem → Join falha).

---

## PRD-R4 — Todos Hosts e Viewers

**Regras:**

- Cada membro tem um **Share slot**: parado ou a capturar.
- Quem captura envia Media **P2P** para cada outro membro (fan-out no processo do capturador, como o Host faz hoje com N Viewers).
- Quem não captura só recebe. O Master também pode assistir a tela de outra pessoa.
- Vários Share slots ao mesmo tempo: a UI mostra grelha (um palco grande + thumbs). Quem assiste escolhe o palco.
- Áudio de sistema e exclusão por app continuam **por capturador**.

**Done when:** A assiste B e B assiste A ao mesmo tempo; o Rendezvous tcpdump não mostra RTP; Broadcast não quebrou.

---

## PRD-R5 — Signaling de malha

**Regras:**

- Rendezvous (Stunar) guarda o Roster da Sala e encaminha offer/answer **por par** (A→B, B→A). Zero Media.
- Cada par de membros tem até dois PeerTransports de vídeo (A envia, B envia) mais áudio se ligado.
- Sem TURN. NAT simétrico continua a falhar, como hoje. Documentado.
- Teto: 8 membros (já existe `MAX_ACCEPTED`). Acima disso, `busy`.

**Done when:** 3 membros, 2 a partilhar, 6 fluxos P2P no máximo (2 senders × 2 receivers + o contrário se ambos partilham… 2×2=4 fluxos de vídeo). Contas no SPEC.

---

## Fora deste pacote

Play Together, Benchmark (pode viver na mesma branch beta mas é PRD à parte). Não misturar com Broadcast além do seletor.
