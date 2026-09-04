# PRD — Play Together (futuro)

Data: 2026-09-04  
Produto: goDrinking  
Estado: **só documentação**. Não implementar nesta entrega.

Âmbito: o Host partilha o ecrã de um jogo e deixa Viewers ligarem um comando (tipo Xbox 360 / XInput) ao PC dele, pela internet, P2P. Parecido com Parsec. O Rendezvous **não** vê os bytes do comando além de um recado de “este membro tem pad” se for preciso no Roster — o fluxo de input é DataChannel P2P, não Media de vídeo.

---

## PRD-P1 — Consentimento do Host

**Problema:** um comando remoto é controlo da máquina. Sem opt-in explícito, é um exploit.

**Regras:**

- Play Together é **off** por omissão.
- Só o Host (Broadcast) ou o dono do Share slot (Sala) liga “aceitar comandos neste PC”.
- Cada Viewer pede autorização. O Host aceita por pessoa, revogável.
- Kick / Stop / desligar o toggle corta o DataChannel na hora.
- O Host vê, por membro do Roster, um ícone de comando se aquele Viewer tiver um pad pronto.

**Done when (futuro):** sem o toggle, nenhum evento XInput é injectado. Com o toggle off a meio, os pads remotos morrem.

---

## PRD-P2 — Pad no Viewer

**Regras:**

- Popup de Definições (ícone na sidebar) com aba **Comando**.
- Lista os pads que o OS vê. Preferência: XInput (Xbox 360 / Xbox One / clones). DualShock só se houver mapeamento XInput estável; senão, aviso “este jogo provavelmente quer Xbox”.
- Botão **Testar**: o Viewer vê A/B/X/Y, gatilhos, sticks, rumble (se houver).
- Enquanto testa, **não** envia nada ao Host.
- Quando o Host aceitou: o pad do Viewer vira um pad virtual no Host.

**Done when (futuro):** testar no Viewer não move o jogo do Host; depois de aceite, A no pad do Viewer é A no XInput do Host.

---

## PRD-P3 — Ícone e “piscadela”

**Regras:**

- Roster: ícone de comando ao lado do Nickname se aquele membro tem pad ligado **e** autorizado.
- Qualquer input (botão, stick fora da zona morta) faz o ícone **piscar** ~150 ms no Host e no próprio Viewer.
- Sem pad / recusado: sem ícone.

**Done when (futuro):** dois Viewers, só o autorizado pisca quando carrega.

---

## PRD-P4 — Transporte

**Regras:**

- Input via **WebRTC DataChannel** no mesmo PeerTransport da Session, fiável ou parcial (SPEC decide). Nunca HTTP, nunca o Rendezvous como relay de sticks.
- Vídeo continua o fluxo de ecrã. Play Together não exige Sala; funciona em Broadcast (Host joga, Viewers mandam pad).
- Latência-alvo: o mais baixo que o DataChannel permitir; sem buffer de 100 ms “para suavizar”.
- Perda: o Host aplica dead-man's — sticks a 0 e botões up se o canal calar > 200 ms.

**Done when (futuro):** tcpdump no Rendezvous sem frames de pad; o Host vê o pad virtual no painel de “Game controllers” do Windows / no I/O Kit do Mac.

---

## PRD-P5 — Plataformas

**Regras:**

- Host Windows 10/11: pad virtual XInput (ViGEmBus ou sucessor com maior compatibilidade em jogos). O utilizador pode ter de instalar o driver uma vez; o app explica.
- Host macOS: o mais compatível com jogos Mac no momento da implementação (Game Controller framework / virtual HID). Muitos jogos Mac nem XInput falam — documentar o limite.
- Viewer Win/Mac: ler o pad local (XInput / GCController).
- GTX 1050 até 40-series e M1+ são irrelevantes para o pad (CPU). O gargalo é o P2P e o driver virtual.

**Done when (futuro):** um jogo DirectInput/XInput comum no Win 11 vê o pad remoto como Xbox 360.

---

## Fora

- Teclado e rato remotos (Parsec completo): não neste PRD.
- Mais do que 4 pads virtuais: fora. Teto 4.
- Anti-cheat: o Host é responsável. O README dirá que injectar input pode levar ban. O projecto não se responsabiliza (já é a política do produto).
