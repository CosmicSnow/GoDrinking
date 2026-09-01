# IMPLEMENTATION — ordem para outra LLM

Não implementar Direct/Stunar em cima de um único `answer` slot. PRD-19 primeiro.

Ler `docs/connectivity/README.md` inteiro. Usar nomes de `CONTEXT.md`.

Verificação: cada fatia tem um “done when” no PRD. Não marcar feito sem isso.

---

## Fatia 0 — Licença (PRD-20)

Já há `/LICENSE`. Falta:

- `package.json` `"license": "PolyForm-Noncommercial-1.0.0"`
- `src-tauri/Cargo.toml` `license = "PolyForm-Noncommercial-1.0.0"`
- README: substituir “Ainda não.” por uma linha: source-available, PolyForm Noncommercial, uso comercial precisa de autorização.

---

## Fatia 1 — Vários Viewers no Host (PRD-19)

Hoje: `LanRoom.offer` / `.answer` únicos; `engine` um `PeerTransport`.

Fazer:

- `ViewerLink { id, nickname, state, peer: PeerTransport, … }`
- `MediaEngine` guarda `Vec`/`HashMap` até 8
- Cada Join cria offer **novo** (não reutilizar SDP)
- `snapshot().roster`
- `kick_viewer(id)` fecha aquele peer

LAN ainda é o único Join mode nesta fatia. TCP GOLIVE1 pode ficar até a fatia 2, mas a sala tem de aceitar N TCP.

Teste: dois processos Watch no mesmo Mac, mesmo código, os dois veem o ecrã.

---

## Fatia 2 — GOLIVE2 + Password + Nickname + Ignore list + Admission (PRD-15, 17 em LAN)

Protocolo TCP em `PROTOCOL.md` secção B.

- `HELLO GOLIVE2` / `AUTH` / `NICK`
- Ignore list no Host
- Admission toggle; `PENDING` / `OK` / `REJECT` / `KICK`
- UI: Nickname, Password, Require approval, Roster
- Comandos: `admit_viewer`, `reject_viewer`, `kick_viewer`, `update_session_credentials`

ICE ainda vazio (LAN).

Teste: Password errada → sem SDP. 5 erros → 15 min. Admission: Accept depois o vídeo. Kick corta um, o outro fica. Rotate Password: ligados ficam, novo precisa da nova.

---

## Fatia 3 — Seletor + Direct (PRD-13, 14)

- `join_mode` no Start/Join
- LAN: UDP 17424 como hoje, depois GOLIVE2 no TCP
- Direct: **sem** UDP. Viewer `TcpStream` ao IP:porta
- Snapshot `direct_addresses` + porta
- STUN só se `join_mode != lan` em `peer_transport.rs` (`RTCConfiguration.ice_servers`)
- UPnP/NAT-PMP/PCP best-effort, timeout 2s, nunca bloquear Start se falhar
- UI conforme `UI.md`

Não falar com o Rendezvous nesta fatia.

Teste: Join por `127.0.0.1` e pela IPv4 LAN, sem broadcast (podes desligar o UDP no Host para provar). IPv6 se o ambiente tiver.

---

## Fatia 4 — Rendezvous Node (PRD-16 servidor)

`rendezvous/server.mjs` como `SERVER.md`. README com Caddy e os 7 testes manuais.

Nada de app ainda. Provar com `curl` + um cliente WS mínimo (`websocat` ou script).

---

## Fatia 5 — Stunar no app (PRD-16 cliente)

- Host Rust: `open`, Heartbeat 30s, WS inbox, `decide`, `rotate`, `close`
- Viewer: `ask` + WS; offer/answer pelo mailbox
- URL em settings
- ICE com STUN (já da fatia 3)
- UI estados `Calling…` / `Live` / `Relay unreachable`

Teste: dois PCs (ou dois apps) **sem** LAN comum, Rendezvous no meio, Media P2P. Confirmar no servidor que não há RTP.

---

## Fatia 6 — Rotate ao vivo no Stunar + polish (PRD-18)

- `POST /v1/host/rotate`
- LAN já rodou na fatia 2
- Direct: só Password
- Não desligar Connected

---

## Ficheiros prováveis

| Ficheiro | Fatias |
|---|---|
| `LICENSE` | 0 (já) |
| `package.json`, `Cargo.toml`, `README.md` | 0 |
| `src-tauri/src/media/room.rs` | 1, 2, 3 |
| `src-tauri/src/media/engine.rs` | 1–6 |
| `src-tauri/src/media/peer_transport.rs` | 1, 3 |
| `src-tauri/src/media/types.rs` | 1–5 |
| `src-tauri/src/lib.rs` | comandos novos |
| `src/App.tsx`, `src/App.css` | 2, 3, 5 |
| `rendezvous/server.mjs` | 4 |
| `src-tauri/src/media/rendezvous.rs` (novo) | 5 |

Não criar SFU. Não adicionar TURN. Não mudar captura.

---

## Anti-padrões (a LLM não faz)

- `ice_servers` no LAN
- Logar Password, Token, SDP
- `{ error: "wrong password" }`
- Um único offer para todos os Viewers
- Fallback Direct→LAN sem o utilizador pedir
- SQLite no Rendezvous
- `npm` extra no Rendezvous além de `ws`
- Pedir Room code no Direct
- Chamar o Rendezvous de STUN no código ou na UI
