# PROTOCOL — bytes na rede

Três fios. Nunca misturar.

1. **LAN discovery** — UDP 17424 (só Join mode `lan`)
2. **Host TCP Signaling** — LAN e Direct
3. **Rendezvous HTTPS + WebSocket** — só Stunar

Media = WebRTC DTLS/SRTP. Não está neste ficheiro.

---

## A. UDP discovery (LAN)

Igual ao código atual. Porta **17424**. Magic `GOLIVE1`.

Viewer → broadcast `255.255.255.255:17424`:

```
GOLIVE1 FIND ABC123
```

Host, se o código **atual** da Session bater (depois de rotate, só o novo):

```
GOLIVE1 HOST ABC123 41234
```

`41234` é a porta TCP. O Viewer liga TCP ao IP de origem do datagrama.

Sem Password no UDP. AUTH é no TCP. Quem não sabe a Password não leva offer.

---

## B. TCP Signaling (LAN e Direct) — GOLIVE2

Texto UTF-8, uma instrução por linha, `\n`. Primeira linha **obrigatória**:

```
HELLO GOLIVE2
```

Se a primeira linha for `GET_OFFER` (GOLIVE1), o Host fecha. Sem compatibilidade com apps antigos neste pacote — os dois lados sobem juntos.

### B1. Viewer → Host

```
HELLO GOLIVE2
AUTH <password>
NICK <nickname>
```

- `AUTH` com Password vazia: linha `AUTH` sem mais nada.
- `NICK` já validado no cliente; o Host volta a validar.

Depois, só depois de `OK`:

```
GET_OFFER
ANSWER <json-numa-linha>
```

`<json>` é `{"type":"answer","sdp":"..."}` — o `PeerSignal` de hoje.

### B2. Host → Viewer

| Linha | Quando |
|---|---|
| `OK <viewer_id>` | AUTH ok e Admission desligada |
| `PENDING <viewer_id>` | AUTH ok e Admission ligada |
| `OFFER <json-numa-linha>` | Depois de OK, ou depois de Accept |
| `REJECT` | Host recusou Pending |
| `KICK` | Host mandou embora (também no meio da Session) |
| `ERR AUTH` | Password errada ou em falta |
| `ERR BANNED` | IP na Ignore list |
| `ERR FULL` | 8 Connected ou 8 Pending |
| `ERR NICK` | Nickname inválido |
| `ERR PROTO` | Linha a mais, SDP enorme, HELLO errado |

Depois de `ERR *` ou `REJECT` ou `KICK`, o Host fecha o TCP.

`PENDING`: o TCP **fica aberto**. O Host escreve `OK <id>` + `OFFER ...` quando Accept, ou `REJECT` quando Reject. Timeout Pending no Host: **60s** sem decisão → `REJECT`.

### B3. AUTH

Comparar Password com `crypto` constant-time no Host (hash SHA-256 da Password da Session vs hash do que veio; ou `subtle` compare dos bytes se vazia).

Não responder `OFFER` antes de `OK`.

Ignore list: ver SECURITY.md. IP = `peer_addr` do TCP (não o IP do ICE).

### B4. Um TCP por Viewer

Não reutilizar o TCP de um Viewer para outro. O Host aceita N ligações, cada uma um `ViewerLink`.

---

## C. Rendezvous (Stunar)

Base URL: `https://<host>` (sem slash final). JSON UTF-8. Header `Content-Type: application/json`. Corpo máximo **64 KiB**.

Erros **sempre**:

```json
{ "ok": false, "error": "denied" }
```

exceto:

| `error` | HTTP | Uso |
|---|---|---|
| `denied` | 401 ou 404 **à escolha, mas estável** — usar **404** para tudo o que seja auth/sala | Password, sala inexistente, Token mau, Admission recusada |
| `full` | 429 | Sala cheia |
| `invalid` | 400 | JSON/campos |
| `busy` | 429 | Rate limit |

Nunca `unknown_room`, `bad_password`, `expired`. Quem implementa não “melhora” isto.

### C1. REST

**`POST /v1/host/open`**

```json
{
  "password": "obrigatória",
  "nickname": "Ana",
  "admission": false
}
```

Resposta:

```json
{ "ok": true, "host_token": "<32 bytes hex>", "code": "ABC123" }
```

O código é gerado pelo servidor: 6 chars `A-Z0-9` via `randomBytes` base36, com retry até 10 se colidir com sala viva; se mesmo assim não houver código livre, `busy`. O Host não escolhe nem envia código. Password obrigatória (4–64) em toda sala Stunar; `open` sem Password → `invalid`.

**`POST /v1/host/heartbeat`**

```json
{ "host_token": "..." }
```

```json
{ "ok": true }
```

**`POST /v1/host/rotate`**

```json
{ "host_token": "...", "password": "nova" }
```

Só Password (e `admission`, se o Host a mudar ao vivo). Sem rotação de código: o código é do servidor e vive até a sala morrer. Omitir `password` = não mexer. Password `""` (remover) → `invalid`: no Stunar a Password é obrigatória.

**`POST /v1/host/close`**

```json
{ "host_token": "..." }
```

Apaga a sala. Viewers ligados no Rendezvous recebem `gone` no WS.

**`POST /v1/viewer/ask`**

```json
{
  "code": "ABC123",
  "password": "obrigatória",
  "nickname": "Joao"
}
```

Resposta possível:

```json
{ "ok": true, "status": "pending", "viewer_token": "..." }
{ "ok": true, "status": "accepted", "viewer_token": "..." }
{ "ok": false, "error": "denied" }
{ "ok": false, "error": "full" }
```

`accepted` imediato só se Admission estiver desligada e AUTH ok.

Password errada ou em falta → `denied` (toda sala Stunar tem Password).

**Tarpit:** depois de 10 falhas em 10 min do mesmo IP, `ask` responde `{ "ok": true, "status": "pending", "viewer_token": "<dummy>" }` — um pending falso, com o mesmo timing do `denied` (scrypt dummy + 50–80 ms). O WS desse token é um tarpit: manda `roster` vazio, ignora mensagens e fecha aos 60s. Tokens falsos: máx 100, TTL 2 min.

**`POST /v1/host/decide`**

```json
{ "host_token": "...", "viewer_id": "...", "action": "accept" | "reject" | "kick" }
```

`viewer_id` é o id que o Host viu no Roster via WS.

### C2. WebSocket `/v1/ws`

Query: `?role=host&token=<host_token>` ou `?role=viewer&token=<viewer_token>`.

Um socket por papel. Reconnect ok; o Token continua válido até close/kick/expirar sala.

Mensagens **servidor → cliente**, JSON numa frame:

```json
{ "t": "pending", "viewer_id": "a1b2", "nickname": "Joao" }
{ "t": "accepted", "viewer_id": "a1b2" }
{ "t": "rejected" }
{ "t": "kicked" }
{ "t": "gone" }
{ "t": "roster", "entries": [ { "id": "a1b2", "nickname": "Joao", "state": "connected" } ] }
{ "t": "signal", "viewer_id": "a1b2", "payload": { "type": "offer", "sdp": "..." } }
```

- `pending` / `roster` / `signal` com `viewer_id` → Host
- `accepted` / `rejected` / `kicked` / `gone` / `signal` → Viewer (`viewer_id` pode ir no accepted para o Viewer guardar)

Mensagens **cliente → servidor**:

```json
{ "t": "signal", "payload": { "type": "answer", "sdp": "..." } }
```

O servidor só reencaminha `signal` se o Token estiver **accepted**. Pending não manda nem recebe SDP. Host manda `signal` com `viewer_id`. Viewer não manda `viewer_id` (está no Token).

Token de tarpit (dummy do `ask`): o WS aceita, manda `roster` vazio, ignora tudo o que o cliente mandar e fecha aos 60s. Nunca `pending`/`accepted`/`signal` reais.

Heartbeat do Host é REST, não WS ping. WS ping/pong do protocolo WebSocket a 30s para não morrer o proxy — não substitui o Heartbeat da sala.

### C3. Regras de encaminhamento

O Rendezvous **não lê** o SDP. Trata `payload` como blob JSON com `type` ∈ {`offer`,`answer`} e `sdp` string ≤ 64 KiB.

Não guarda SDP depois de entregar (mailbox de 1 slot por Viewer; offer novo substitui offer não lido).

Não envia `signal` a um Viewer noutro `code`.

---

## D. O que os utilizadores trocam (humano)

| Modo | Host manda ao Viewer | Viewer escreve |
|---|---|---|
| LAN | Room code (+ Password se houver) | Room code, Password, Nickname |
| Direct | IPv4 e/ou IPv6 + porta (+ Password) | endereço, porta, Password, Nickname |
| Stunar | Room code (+ Password) | Room code, Password, Nickname |

Nunca mandam SDP à mão nesta versão. Nunca mandam Tokens.
