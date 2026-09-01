# SERVER — Rendezvous Node.js

Um ficheiro (ou dois). Um processo. RAM. Isto é o servidor **inteiro**.

## Layout

```
rendezvous/
  package.json          // name: godrinking-rendezvous, type: module
  server.mjs            // escuta, rotas, WS
  README.md             // como correr atrás de Caddy
```

Dependência de produção: `ws`. Nada mais. `scrypt` e `timingSafeEqual` vêm de `node:crypto`.

```
node --watch server.mjs
PORT=8787 BIND=127.0.0.1 node server.mjs
```

Default: `127.0.0.1:8787` HTTP. TLS = Caddy/nginx na frente. A app usa `https://dominio`.

## Estado (RAM)

```js
// pseudo
rooms = Map<code, Room>
tokens = Map<token, { role, code, viewerId? }>
ignore = Map<ip, { fails: number[], until: number, level: number }>
fakeTokens = Map<token, { until: number }>   // tarpit; máx 100, TTL 2 min

Room = {
  code,
  passwordHash,      // sempre presente: Password obrigatória no Stunar
  passwordSalt,
  admission,
  hostNickname,
  hostToken,
  heartbeatAt,
  viewers: Map<viewerId, {
    nickname, token, state: "pending"|"accepted",
    ws,              // opcional
    inbox: payload | null
  }>
}
```

GC a cada 15s: se `now - heartbeatAt > 5 * 60 * 1000`, apagar Room e invalidar Tokens. GC também limpa `fakeTokens` vencidos (TTL 2 min) e `ignore` vencidos.

`code` no Map é o código em maiúsculas, gerado pelo servidor no `open` (6 chars `A-Z0-9`, `randomBytes` base36, retry 10; sem código livre → `busy`). Não há rotate de código.

## Password

- Obrigatória (4–64) em toda sala Stunar. `open` sem Password → `invalid`. `rotate` com `""` → `invalid`.
- Gerar `salt` 16 bytes.
- `scrypt(password, salt, 32, { N: 16384, r: 8, p: 1 })`.
- Guardar hash. Comparar com `timingSafeEqual`.
- Timing: atrasar **sempre** 50–80ms em `ask` falhado **e** em sala inexistente, para não distinguir. No tarpit, correr scrypt com Password dummy (mesmo custo) + o mesmo delay.

## Rate limit e Ignore list

Por IP (socket). Ver números em SECURITY.md.

Implementação mínima: um Map. Sem Redis.

`X-Forwarded-For`: **só** se `TRUST_PROXY=1`. Senão, IP = `socket.remoteAddress`. Errar para o IP do proxy se estiver mal configurado é melhor do que deixar o Viewer escolher o IP.

Ignore list escalada por nível (janela 10 min, `fails` guarda timestamps):

| Falhas | Duração |
|---|---|
| 5 | 15 min |
| 10 | 1 h |
| 15 | 6 h |
| 20+ | 24 h |

## Tarpit

Depois de **10 falhas em 10 min** do mesmo IP, `ask` deixa de responder `denied` e responde `{ ok: true, status: "pending", viewer_token: "<dummy>" }`:

- `viewer_token` falso, `randomBytes`, guardado em `fakeTokens` (máx **100**, TTL **2 min**; cheio → `busy`).
- Mesmo timing do `denied`: scrypt com Password dummy + 50–80 ms.
- WS desse token: aceita o upgrade, manda `roster` vazio, ignora mensagens, fecha aos **60s**. Nunca `pending`/`accepted`/`signal` reais.
- O IP continua a contar falhas e a escalar a Ignore list normalmente.

## Concorrência

JS single-thread. Sem locks. Não há await entre “checar cheio” e “inserir Viewer” — fazer sincrono nesse trecho.

Máximo 8 accepted + 8 pending por Room. Máximo **256** Rooms no processo. Acima: `busy` em `open`.

Máximo **512** WS. Acima: fechar a nova.

## Logs

Uma linha por evento, sem Password, sem SDP, sem Token completo (prefixo 4 hex no máximo):

```
ts level ip event code? viewer? error?
```

Níveis: `info` `warn`. Nada de dump de body.

## Health

`GET /health` → `200 {"ok":true}`. Sem lista de salas.

## O que este servidor recusa-se a ter

- Disco, SQLite, ficheiros de salas
- Contas, email, OAuth
- Listagem `/v1/rooms`
- Métricas públicas
- TURN / UDP / RTP
- Admin API
- CORS aberto (`Access-Control-Allow-Origin: *` proibido). Desktop Tauri não precisa de CORS. Se algum dia houver web viewer, allowlist.

## Deploy mínimo

Caddyfile:

```
rendezvous.example.com {
  reverse_proxy 127.0.0.1:8787
}
```

Systemd: restart on failure. Não há persistência a recuperar.

## Testes manuais (obrigatórios no README)

1. `open` + `ask` com Password certa → accepted.
2. `open` com Password, `ask` errada → `denied`. `ask` certa → ok.
3. `ask` a código inventado → `denied` (igual ao 2).
4. Admission on: `ask` → pending; `decide accept` → signal offer/answer.
5. Parar Heartbeat 5 min → `ask` `denied`.
6. `rotate` Password → `ask` com a antiga `denied`, com a nova ok; WS do Viewer aceite continua.
7. 5 `ask` erradas do mesmo IP → 15 min `denied` imediato.
8. 10 `ask` erradas do mesmo IP em 10 min → tarpit: `ask` devolve `pending` com token falso; WS desse token recebe `roster` vazio e fecha aos 60s; o 11.º `ask` continua `pending` falso.
9. 20 `ask` erradas do mesmo IP → 24 h `denied` imediato (nível máximo).
