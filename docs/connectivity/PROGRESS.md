# PROGRESS — connectivity

Outra LLM: lê isto primeiro, depois `README.md` nesta pasta. Não saltes fatias.

## Estado

| Fatia | PRD | Estado |
|---|---|---|
| 0 Licença | 20 | **feita** (2026-09-01) |
| 1 Vários Viewers | 19 | **feita** (código + `cargo test` room; falta prova com 2 Watch) |
| 2 GOLIVE2 + Password + Admission + Roster UI | 15, 17 | **feita** (código + testes; falta prova manual) |
| 3 Seletor + Direct | 13, 14 | **feita** (código + testes; falta prova manual) |
| 4 Rendezvous Node | 16 servidor | **feita** (código + testes manuais 1–7; falta prova com a app) |
| 5 Stunar no app | 16 cliente | **feita** (código + testes; falta prova manual com 2 apps) |
| 6 Rotate ao vivo Stunar | 18 | **feita** (código + testes; falta prova manual) |

## Fatia 0 — o que ficou

- `/LICENSE` PolyForm Noncommercial 1.0.0
- `package.json` `"license": "PolyForm-Noncommercial-1.0.0"`
- `src-tauri/Cargo.toml` `license = "PolyForm-Noncommercial-1.0.0"`
- `README.md` secção licença atualizada

## Fatia 1 — o que ficou

- `src-tauri/src/media/fanout.rs` — copia H.264/Opus para N peers
- `LanRoom` mint um offer **por** `GET_OFFER` (TCP por ligação em thread)
- `PeerSignal.id` (JSON opcional) para casar ANSWER com o Viewer
- `MediaEngine.viewers` até 8; `kick_viewer` / `kick_media_viewer`
- `snapshot.roster`
- Start **não** chama `create_media_peer_offer` (offer nasce no Join)
- Viewer ecoa `offer.id` no answer
- Testes: `media::room::tests::get_offer_mints_a_fresh_signal` ok
- `cargo check --tests` ok

Prova manual ainda em falta: dois processos Watch no mesmo Mac, mesmo código.

Não feito nesta fatia (é a 2): Password, GOLIVE2, Admission UI, Nickname.

## Fatia 2 — o que ficou

- `room.rs` — TCP GOLIVE2 (`HELLO GOLIVE2` / `AUTH` / `NICK`), linha a linha com `BufReader`. Primeira linha obrigatória; `GET_OFFER` (GOLIVE1) → `ERR PROTO` e close.
- `SessionGate` ligado ao `LanRoom` e ao `SessionRecord`: Password (constant-time via `passwords_match`), Admission, Ignore list (5 falhas/10 min → 15 min; Reject/Kick/FULL não contam), Pending com timeout 60s → `REJECT`.
- Fluxo Host: `OK <id>` + `OFFER` (Admission off) ou `PENDING <id>` → decisão → `OK` + `OFFER` / `REJECT` (Admission on). `ANSWER <json>` aceite na mesma ligação **ou** numa segunda ligação (fluxo do app: discover → answer).
- Fluxo Viewer: `discover_room(code, password, nickname)` faz UDP + handshake GOLIVE2; `submit_answer` continua na segunda ligação. Erros mapeados para as strings da UI (`Could not join.`, `This session is full.`, `The host declined.`).
- `engine.rs` — `admit_viewer`, `reject_viewer`, `update_session_credentials` (Password/Admission; não toca em Connected). `mint_viewer_offer(id, nickname)`; `refresh_native_state` remove Viewers com peer Failed. Snapshot: `roster` (pending + connected), `password_set`, `admission`, `join_mode: "lan"`.
- `types.rs` — `CreateMediaSessionRequest.password/nickname/admission` (serde default), `PeerTransportState::Pending` para o Roster, snapshot com `password_set`/`admission`/`join_mode`.
- `lib.rs` — comandos `admit_media_viewer`, `reject_media_viewer`, `update_media_session_credentials`; `discover_media_room`/`submit_media_room_answer` agora async (o handshake pode esperar 60s no Pending).
- `App.tsx` — Nickname persistido (`godrinking.nickname`, 2–24, validação), Password no Share e no Join, toggle `Require approval`, bloco `People` (Accept/Decline para pending, Disconnect para connected).
- Testes: `media::room` com GOLIVE2 (offer mintado, AUTH errada, GOLIVE1 recusado, Pending accept/reject), `media::session_gate` ok. `cargo check --tests` e `cargo test` verdes.

Prova manual ainda em falta (como na fatia 1): dois processos Watch no mesmo Mac, mesmo código — agora com Password errada → sem SDP; 5 erros → 15 min; Admission: Accept depois o vídeo; Kick corta um, o outro fica; Rotate Password: ligados ficam, novo precisa da nova.

Notas:
- LAN continua sem STUN (`ice_servers` vazio) e sem TURN, como antes.
- O Viewer não recebe `KICK` por TCP (a ligação de Signaling fecha depois do OFFER); o corte aparece como perda de ICE (`Connection lost.`).
- Um Pending cujo TCP morre fica no Roster até ao timeout de 60s; Accept nesse caso cria um Viewer sem answer — o Host pode usar Disconnect.
- `update_session_credentials` só roda Password/Admission; rotate de Room code é a fatia 6.

## Fatia 3 — o que ficou

- `types.rs` — `JoinMode` (`lan`/`direct`/`stunar`, default `lan`), `CreateMediaSessionRequest.join_mode` + `rendezvous_url` (serde default), `DirectAddress { ip, port, version, kind, copy }`, snapshot com `join_mode: JoinMode`, `direct_listen_port`, `direct_addresses`, `direct_mapping`.
- `peer_transport.rs` — `PeerTransport::new` recebe `join_mode`; LAN mantém `ice_servers` vazio, Direct/Stunar usam `stun:stun.l.google.com:19302`. Sem TURN.
- `room.rs` — `DirectRoom` (mesmo GOLIVE2 TCP do LAN, sem UDP, sem Room code): listener dual-stack `[::]:0` com probe IPv4 (fallback `0.0.0.0:0`), worker de endereços que recolhe LAN IPv4 (instantâneo), IPv6 global (routing table, sem link-local) e IPv4 público via STUN binding request best-effort (timeout 1.5s, no worker, nunca bloqueia o Start). UPnP/NAT-PMP/PCP é stub → `mapping: false` + log. `discover_direct` (TCP directo, sem broadcast) com erro `Could not reach that address.` para falha de ligação.
- `engine.rs` — `create_in_state` escolhe `LanRoom` ou `DirectRoom` por `join_mode`; Stunar → `MediaEngineError::UnsupportedJoinMode` antes de tocar no estado. Snapshot com campos Direct; `mint_viewer_offer` passa `join_mode` ao PeerTransport.
- `lib.rs` — `JoinRoomRequest` com `join_mode`/`host`/`port`; `discover_media_room` ramifica por modo; `parse_direct_host` aceita `1.2.3.4:41234`, `[2001:db8::1]:41234` e IPv6 nu com porta; nomes DNS → `Could not reach that address.`.
- `App.tsx` — seletor LAN | Direct | Stunar (persistido `godrinking.join_mode`, desativado enquanto Running, texto de ajuda por modo). Host Direct: lista de endereços copiáveis (LAN/Public/IPv6) + avisos (`No public IPv4…`, `Port mapping failed…`). Viewer Direct: campos Address + Port. Stunar selecionável mas Start/Join devolvem `Stunar is not yet available — use LAN or Direct.`
- Testes: 61 verdes (`stun_xor_mapped_address_parses`, `stunar_join_mode_is_rejected_before_starting` novos). `cargo check --tests` e `npx tsc --noEmit` ok.

Prova manual (como nas fatias 1-2): dois processos no mesmo Mac — Join Direct por `127.0.0.1:<porta>` e pela IPv4 LAN **sem broadcast** (prova que o Direct não usa UDP); IPv6 se o ambiente tiver; Password/Admission iguais ao LAN.

Notas:
- STUN/UPnP são best-effort sem crates novos: STUN binding request manual (1.5s), UPnP/NAT-PMP/PCP stub → `direct_mapping: false` e aviso na UI. A linha Public só aparece se o STUN respondeu; a porta TCP precisa de forward manual.
- Windows: o probe IPv4 deteta `[::]` v6-only e cai para `0.0.0.0:0` (sem IPv6 listado).
- O Rendezvous não é contactado nesta fatia. Stunar continua bloqueado no Start/Join.

## Fatia 4 — o que ficou

- `rendezvous/` — servidor Node 22 num ficheiro (`server.mjs`), RAM only, sem DB/Redis/disco. Dependência de produção: só `ws`. `node:crypto` para `randomBytes`/`scrypt`/`timingSafeEqual`.
- REST (PROTOCOL.md C1): `open` (256 rooms max, code `^[A-Z0-9]{6}$`, nickname 2–24, password 0 ou 4–64, scrypt N=16384 r=8 p=1, salt 16B), `heartbeat` (TTL 5 min, GC 15s), `rotate` (código com colisão → denied e mantém o antigo; password `""` remove; tokens accepted sobrevivem), `close`, `ask` (pending/accepted/denied/full; 8+8 por room; delay 50–80ms + scrypt dummy em sala inexistente para não distinguir timing), `decide` (accept/reject/kick).
- WS `/v1/ws?role=…&token=…` (C2): um socket por papel, reconnect ok; `pending`/`accepted`/`rejected`/`kicked`/`gone`/`roster`/`signal`; mailbox de 1 slot por Viewer (offer novo substitui não lido; não guarda SDP depois de entregar); só reencaminha signal se Token accepted; ping/pong 30s.
- Segurança (SECURITY.md): 404 `denied` estável para auth/sala inexistente; 429 `full`/`busy`; 400 `invalid`; 413 body >64 KiB; rate limits por IP (ask 10/min, open 5/min, heartbeat 6/min, WS 20/min, resto 60/min); Ignore list 5 falhas/10 min → 15 min (conta sala inexistente; não conta full/reject/kick); `X-Forwarded-For` só com `TRUST_PROXY=1`; timeouts headers 5s / request 15s; logs sem Password/SDP/Token.
- `rendezvous/README.md` — correr (PORT/BIND/TRUST_PROXY/HEARTBEAT_TTL_MS), Caddy, e os 7 testes manuais (curl + script WS).
- Testes manuais 1–7 verificados com curl/WS contra o servidor a correr (open/ask, password errada/certa, código inventado, admission+signal, GC sem heartbeat, rotate, ignore list). `node --check` ok. Rust intacto: `cargo check --tests` 0 erros, 61 testes verdes.

Prova manual ainda em falta: a app (Fatia 5) a falar com este servidor.

Notas:
- Bug encontrado e corrigido durante a prova: `isIgnored` apagava a entrada da Ignore list quando o ban não estava ativo, perdendo o histórico de falhas — agora só apaga quando o histórico expira.
- Rotate de código atualiza o `code` nos tokens dos Viewers accepted (senão o WS deles perdia a sala).
- A app ainda não contacta o Rendezvous (Stunar bloqueado no Start/Join até à fatia 5).

## Fatia 5 — o que ficou

- `src-tauri/src/media/rendezvous.rs` (novo) — cliente Stunar em Rust:
  - Host (`StunarHost`): `open` (POST /v1/host/open), worker com tokio current-thread que mantém o WS inbox (`/v1/ws?role=host`) com reconnect 2s e Heartbeat REST a cada 30s; estado `calling`/`live`/`unreachable`; roster sincronizado do Rendezvous (pending + accepted); mailbox de answers; `decide`/`kick`/`rotate`/`close` (POST); `send_signal` (offer) via canal para o worker.
  - Viewer (`discover_stunar_room`/`submit_stunar_answer`): `ask` → denied/full/pending/accepted; WS de viewer à espera de accepted + offer (timeout 65s, pending → `The host declined.`); answer enviado pelo mesmo WS (o `StunarViewer` guarda o runtime tokio — o reactor do stream tem de sobreviver entre comandos).
- `engine.rs` — `create_in_state` abre o StunarHost antes da captura (falha = abort limpo; sem URL → `Set the Stunar URL in settings.`); `SessionRecord.stunar`; `admit_viewer`/`reject_viewer`/`kick_viewer`/`update_session_credentials` ramificam para Stunar (decide/rotate no Rendezvous; Admission é fixa no open); `apply_stunar_accepts` (poll no snapshot) minta offer para accepted sem ViewerLink — cobre Admission off, em que o Rendezvous aceita sem passo pending; snapshot com `stunar_state` e roster pending do Stunar; `stop_in_state` faz `close` no Rendezvous.
- `lib.rs` — `discover_media_room` ramifica Stunar (usa o engine, guarda o `StunarViewer`); `submit_media_room_answer` com `join_mode` (Stunar → WS); comando `stunar_viewer_close` (Disconnect).
- `types.rs` — `StunarState` (`calling`/`live`/`unreachable`), snapshot `stunar_state: Option<StunarState>`.
- `Cargo.toml` — `reqwest` (rustls native roots), `tokio-tungstenite` (rustls), `futures-util`.
- `App.tsx` — campo Stunar URL persistido (`godrinking.rendezvous_url`) no Share e no Join quando o modo é Stunar; Start/Join recusam sem URL; Host Stunar mostra código copiável + chip `Calling…`/`Live`/`Relay unreachable` + Password/Admission/Roster; Viewer Stunar com código + Password, notice `Waiting for approval…`; erros `Stunar is unreachable.`/`Could not join.`/`This session is full.`/`The host declined.`; Disconnect fecha o WS.
- `rendezvous/server.mjs` — fix: o servidor agora envia `roster` ao Host também quando um `ask` é aceite imediatamente (Admission off), senão o Host nunca sabia do Viewer para minta o offer.
- Testes: 63 verdes. `stunar_integration_test` (gated: salta se o Rendezvous local não estiver a correr) prova o caminho de sinal completo contra `rendezvous/server.mjs` em `127.0.0.1:8787`: open → ask pending → decide accept → offer via WS → answer via WS → mailbox do Host; Admission off → accepted via roster → offer; close → sala some. `cargo check --tests` 0 erros, `npx tsc --noEmit` ok.

Prova manual ainda em falta: dois apps (ou dois processos) com o Rendezvous no meio — Host Stunar + Viewer Stunar, sem LAN comum. ICE usa STUN (já da fatia 3); Media é P2P, nunca passa pelo Rendezvous.

Notas:
- Escolha Host Rust / Viewer Rust (via invoke) — o Token do Viewer não vive no WebView; o `RTCPeerConnection` continua no browser.
- Heartbeat 30s; Pending 60s no Viewer; sem TURN; sem listagem de salas.
- `update_session_credentials` no Stunar roda Password (rotate); Admission não é mutável no Rendezvous a meio da Session.

## Fatia 6 — o que ficou

- `types.rs` — `UpdateCredentialsRequest { code?, password?, admission? }` (`None` = manter; `Some("")` = remover Password; `Some("ABC123")` = novo Room code).
- `room.rs` — `LanRoom.code` passou a `Arc<Mutex<String>>`; `rotate_code` valida `^[A-Z0-9]{6}$` (normaliza maiúsculas) e atualiza; o `udp_loop` lê o código atual a cada pedido (extraído para `udp_reply` testável). Depois do rotate, o broadcast só responde ao código novo.
- `rendezvous.rs` — `StunarHost::rotate(code?, password?)` faz `POST /v1/host/rotate`; 404 (colisão) → `That code is already in use.` e mantém o antigo; sucesso atualiza o código local. O servidor já repointa os tokens dos Viewers accepted; o WS não é tocado; o Heartbeat continua com o mesmo host_token.
- `engine.rs` — `update_session_credentials(request)` (extraído para `update_credentials_in_state`): valida o código antes de qualquer rede; LAN roda `rotate_code` local; Stunar chama o Rendezvous (sem segurar o state lock durante a rede); Direct só Password/Admission; gate atualizado para o novo AUTH. Viewers/fanout/pipeline/peers intocados; snapshot reflete o novo código/password_set imediatamente.
- `lib.rs` — comando `update_media_session_credentials` usa o `UpdateCredentialsRequest` partilhado.
- `App.tsx` — linha `New code` (input 6 chars + botão) no painel Connect para LAN/Stunar enquanto ativo; erros de formato/colisão mostrados e valores antigos mantidos; Password/Admission já eram live (fatia 2/5) e continuam.
- Testes: 68 verdes. Novos: `rotate_code_validates_and_updates`, `udp_discovery_answers_only_the_current_code` (room), `rotate_credentials_keeps_the_session_and_updates_the_code`, `rotate_credentials_rejects_invalid_codes_and_keeps_old_values` (engine), `stunar_rotate_code_and_password_live` (integração: rotate → código antigo deixa de resolver, password antiga rejeitada, nova funciona; colisão → erro e código antigo mantido).

Prova manual ainda em falta: sessão LAN com 2 Viewers ligados → rotate código/password → os dois continuam; um terceiro com o código/password antigos é recusado, com os novos entra. Stunar com Rendezvous local: rotate → o mesmo.

Notas:
- Rotate não reinicia captura/pipeline/fanout/peers — só o código/password mudam para pedidos novos.
- Direct não tem Room code; só a Password conta para AUTH novo.
- Colisão de código Stunar → erro `That code is already in use.` e o código antigo fica ativo.

## Como correr

Não usar `npm run tauri dev` para captura. `npm run macos:app`.
