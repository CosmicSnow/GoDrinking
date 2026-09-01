# SPEC — como isto entra no goDrinking

## 1. O que não muda

| Peça | Continua |
|---|---|
| Captura Host | ScreenCaptureKit / Windows capture |
| Encode | VideoToolbox / OpenH264 + Opus |
| Host WebRTC | `webrtc-rs`, `TrackLocalStaticSample` |
| Viewer WebRTC | `RTCPeerConnection` no WebView |
| Frames | nunca atravessam IPC Tauri |
| LAN discovery | UDP `GOLIVE1 FIND` porta **17424** quando Join mode = `lan` |

## 2. O que muda

Hoje: 1 `LanRoom` + 1 `PeerTransport` + `ice_servers: []`.

Passa a:

```
MediaEngine
  NativePipeline          (igual)
  SessionGate             (ex-LanRoom, agora por Join mode)
  Vec<ViewerLink>         (até 8)
      PeerTransport       (ICE conforme o modo)
      nickname, id, state
  Roster + Admission + Password + Ignore list
```

`create_media_session` recebe:

```
join_mode: "lan" | "direct" | "stunar"
password: string          // "" = sem Password (LAN/Direct); Stunar obrigatória 4–64
nickname: string
admission: bool
rendezvous_url?: string   // só stunar
```

## 3. ICE por modo

| Modo | `ice_servers` | Discovery |
|---|---|---|
| LAN | `[]` | UDP broadcast + TCP |
| Direct | STUN configurável (default `stun:stun.l.google.com:19302`) | TCP no IP:porta que o Viewer escreveu |
| Stunar | mesmo STUN | HTTPS Rendezvous, depois P2P |

Sem TURN. Sem `iceTransportPolicy: relay`.

STUN default pode ser substituído por lista vazia numa config avançada; Direct/Stunar sem STUN degradam a “só host/IPv6”.

## 4. Endereços Direct (“UDP automático”)

No Start Direct, o Host:

1. Abre `TcpListener` dual-stack se possível (`[::]:0` com `IPV6_V6ONLY=0`, senão `0.0.0.0:0` + IPv6 à parte).
2. Recolhe IPv4/IPv6 **não** link-local, **não** loopback, **não** ULA a não ser que não haja mais nada (ULA só como extra, etiquetada).
3. STUN binding: guarda o srflx IPv4.
4. Tenta NAT-PMP, depois PCP, depois UPnP IGD, mapeando a porta TCP de Signaling (UDP Media fica a cargo do ICE). Timeout total ≤ 2s. Falha = silêncio + flag `mapping: false`.
5. Publica na snapshot:

```
direct_listen_port: u16
direct_addresses: [
  { ip, version: 4|6, kind: "lan" | "public" | "mapped" | "ipv6", copy: "1.2.3.4:port" }
]
```

A UI mostra cada linha. Não inventa IPv6 se o OS não tem.

## 5. Viewer Direct

`join_direct { host, port, password, nickname }`:

- `host` é IPv4 ou IPv6 (sem DNS lookup no v1 — se parecer nome, erro `invalid_address`).
- TCP connect timeout 8s.
- Protocolo `PROTOCOL.md` (GOLIVE2).
- Depois: mesmo fluxo WebRTC do Join LAN (setRemoteDescription → answer → send ANSWER).

## 6. Stunar no app

Rust (Host) e/ou JS (Viewer) falam HTTPS com o Rendezvous. Preferência: **Rust no Host** (Token não vive no WebView), **JS no Viewer** (já é ele que tem o `RTCPeerConnection`). Alternativa aceitável: Rust nos dois via `invoke`. Não misturar os dois caminhos no mesmo papel.

Fluxo Host:

1. `open` → `host_token` + `code` (gerado pelo servidor, 6 chars `A-Z0-9`; o Host não escolhe)
2. thread Heartbeat 30s
3. long-poll / WebSocket `inbox` → pending, accepted, signal, gone
4. por cada Accept: cria `PeerTransport`, manda offer no mailbox daquele Viewer
5. Stop → `close` + para Heartbeat

Fluxo Viewer:

1. `ask` → `denied` | `pending` | `accepted` + `viewer_token`
2. se pending, poll `wait` até accepted/rejected/timeout (60s)
3. recebe offer, cria answer, `signal`

## 7. Roster no snapshot

```
roster: [
  { id, nickname, state: "pending" | "connected", since_unix_ms }
]
session_code?: string          // lan + stunar; no Stunar vem do `open` (servidor)
password_set: bool             // nunca a Password em claro
admission: bool
join_mode: "lan" | "direct" | "stunar"
```

Comandos novos: `admit_viewer`, `reject_viewer`, `kick_viewer`, `update_session_credentials`.

## 8. Rodar credenciais

`update_session_credentials { password? }`:

- Atualiza o que a SessionGate usa para AUTH novo.
- Stunar: `POST /v1/host/rotate` com `host_token` (só Password; o código é do servidor e não roda).
- Não toca em `ViewerLink` Connected.
- Password `null` = manter. Password `""` = remover Password (só LAN/Direct; no Stunar a Password é obrigatória e `rotate` recusa remover).

## 9. Limites

| Limite | Valor |
|---|---|
| Viewers Connected | 8 |
| Pending | 8 |
| Nickname | 2–24 |
| Password | 4–64 (Stunar obrigatória; LAN/Direct 0 ou 4–64) |
| Room code | 6 A–Z0–9, gerado pelo servidor no Stunar |
| SDP | 64 KiB |
| Heartbeat | 30s envio / 5 min expirar |
| Ignore list | 5→15 min, 10→1 h, 15→6 h, 20+→24 h (janela 10 min) |
| Tarpit | 10 falhas/10 min → pending falso; ≤ 100 tokens, TTL 2 min, WS 60s |
| ICE gather | 5s (como hoje) |

## 10. Fora de âmbito

TURN, contas, histórico de salas, criptografia extra além de DTLS-SRTP do WebRTC, DNS no Direct, IPv4-as-IPv6 mapped na UI, federação de Rendezvous, vários Hosts na mesma sala.
