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
ignore = Map<ip, { fails: number[], until: number }>

Room = {
  code,
  passwordHash,      // null se sem Password
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

GC a cada 15s: se `now - heartbeatAt > 5 * 60 * 1000`, apagar Room e invalidar Tokens.

`code` no Map é o código em maiúsculas. Rotate: apagar chave antiga, inserir nova, **mesmo** objeto Room.

## Password

- Gerar `salt` 16 bytes.
- `scrypt(password, salt, 32, { N: 16384, r: 8, p: 1 })`.
- Guardar hash. Comparar com `timingSafeEqual`.
- Se a sala não tem Password, não chamar scrypt no Viewer (timing: atrasar **sempre** 50–80ms em `ask` falhado **e** em sala inexistente, para não distinguir).

## Rate limit e Ignore list

Por IP (socket). Ver números em SECURITY.md.

Implementação mínima: um Map. Sem Redis.

`X-Forwarded-For`: **só** se `TRUST_PROXY=1`. Senão, IP = `socket.remoteAddress`. Errar para o IP do proxy se estiver mal configurado é melhor do que deixar o Viewer escolher o IP.

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

1. `open` + `ask` sem Password → accepted.
2. `open` com Password, `ask` errada → `denied`. `ask` certa → ok.
3. `ask` a código inventado → `denied` (igual ao 2).
4. Admission on: `ask` → pending; `decide accept` → signal offer/answer.
5. Parar Heartbeat 5 min → `ask` `denied`.
6. `rotate` código → `ask` no antigo `denied`, no novo ok; WS do Viewer aceite continua.
7. 5 `ask` erradas do mesmo IP → 15 min `denied` imediato.
