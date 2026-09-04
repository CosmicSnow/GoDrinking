# SPEC — Modo Sala

Data: 2026-09-04  
[PRD](./PRD.md). Vocabulário: **Broadcast**, **Sala**, **Master**, **Share slot**, **Membro**.

## Seams (TDD)

| Seam | Onde | Observável |
|------|------|------------|
| Sucessão do Master | função pura `next_master(members, leaving_id)` | ordem de join |
| Encerrar Sala vazia | Rendezvous + testes Node | Join `denied` |
| Modo no open | `host/open` body `mode: broadcast\|room` | persistido |
| Pares de Signaling | ids (a,b) ordenados | um canal por direção de Media |

## Modelo

```
SessionMode = Broadcast | Room

Member = { id, nickname, joined_at, is_master, share: Idle | Live }

Room = { code, password_hash, mode, members[], heartbeat_by_member }
```

Broadcast hoje: um Host token, N viewer tokens. Sala: N member tokens, um `master_id`.

## Sucessão

```
next_master(members, leaving):
  rest = members.filter(m => m.id != leaving).sort_by(joined_at)
  return rest[0] or None  // None => close room
```

Empate de `joined_at`: desempate por `id` lexicográfico.

## Rendezvous

Novos (ou alargados) endpoints, todos JSON, sem Media:

- `POST /v1/host/open` ganha `mode`. Default `broadcast` (compat).
- `POST /v1/member/heartbeat` (Sala: cada membro; Broadcast: só o Host como hoje).
- `POST /v1/member/leave`
- `POST /v1/master/kick` { target_id } — só Master token.
- `POST /v1/master/rotate` — Password, como `host/rotate`.
- WS inbox por membro. Recados: `offer`, `answer`, `share-start`, `share-stop`, `roster`, `you-are-master`, `kicked`.

Payloads de Signaling: SDP + `from` + `to` + `media: video|audio`. Teto de bytes igual ao atual. Qualquer outro frame → drop.

Heartbeat: 30 s. Sem Heartbeat 5 min → membro removido; se era Master, sucessão; se Roster vazio, Sala apagada.

## Media (P2P)

Capturador = caminho nativo atual (`webrtc-rs` send).  
Receptor = JS `RTCPeerConnection` receive, um `<video>` por Share slot remoto.

Quando C começa a partilhar numa Sala com membros {A,B,C}:

1. C `share-start` via Rendezvous. **Não** minta offers a toda a gente.
2. A (ou B) clica Watch em C: `watch` → C. C minta offer só para quem pediu.
3. Quem pediu responde. ICE P2P. RTP não toca o servidor.
4. `unwatch` fecha o PeerTransport nesse par (poupa CPU e rede).

Abrir a Sala **não** começa captura. Host e Watcher partilham quando quiserem (`share_on_start: false`).

Quando D entra a meio: o Roster diz quem está a partilhar; D clica Watch em quem quiser. Sem auto-subscribe.

## UI

- Seletor Broadcast | Sala no painel de Start (vista simples).
- Em Sala: botão “Partilhar a minha tela” / “Stop sharing”, mesmo para quem não é Master. Abrir a Sala não captura.
- Palco: grelha das streams a que te ligaste. Cada tile: maior/menor, pin (só essa), Stop (desliga o P2P). Lista de pessoas com Watch/Stop. Host e Watcher iguais.
- Coroa (ícone lima) ao lado do Master no Roster.
- Kick só visível para o Master.

Identidade visual: mesmos painéis, lima, mono. Não inventar um layout “meeting app”.

## LAN / Direct

Sala em LAN: o processo do Master corre o `LanRoom` alargado (Roster de membros + fan-out de SDP). Sem Rendezvous. Se o Master cai, o sucessor faz bind da descoberta (porta 17424) — documentar corrida; se falhar, a Sala LAN acaba (não há servidor a guardar o código).

Direct: o endereço que se partilha é o do Master atual; se a coroa muda, o novo Master mostra endereços novos. Quem já estava ligado não precisa de re-Join (já tem P2P).

## Contas de fluxos

N membros, S a partilhar: `S * (N-1)` fluxos de vídeo. N=8, S=8 → 56. Pesado. UI avisa acima de 4 partilhas simultâneas. Teto duro: 8 membros.

## Invariantes

- Rendezvous não vê, não guarda, não reencaminha Media.
- Broadcast não muda o fio actual além do campo `mode` default.
- Sem TURN.

## Aceite (beta)

- [ ] Abrir em Broadcast continua 1 Host / N Viewers; o campo `mode` default é `broadcast`.
- [ ] Abrir em Sala devolve `mode: room` e `member_id` do Master.
- [ ] Dois membros entram; o Master sai; o segundo `joinedAt` passa a Master (`you-are-master`); o código da Sala continua válido.
- [ ] O último membro sai; um Join com o mesmo código recebe `denied`.
- [ ] Master kicka um membro; esse WS recebe `kicked` e não volta a receber SDP.
- [ ] Membro C clica “Share my screen”: o Roster marca share; A e B **não** recebem vídeo até clicarem Watch.
- [ ] A (Master) e um Watcher vêem a mesma grelha, ligam/desligam cada stream, redimensionam tiles e alfinetam uma.
- [ ] Host abre a Sala sem transmitir. Pode partilhar mais tarde, ou nunca.
- [ ] Coroa lima no Nickname do Master. Kick só no Master.
- [ ] Mais de 4 Share slots Live: aviso na UI. 8 membros: `full`.
- [ ] LAN/Direct em Sala: se o Master cai, a Sala acaba (sem Rendezvous a guardar o código). Documentado na UI.
