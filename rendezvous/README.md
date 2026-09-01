# goDrinking Rendezvous

O servidor **inteiro** num ficheiro (`server.mjs`). Um processo. RAM. Sem disco,
sem DB, sem Redis. Só apresenta Host e Viewer — nunca vê, guarda ou reencaminha
Media (WebRTC DTLS/SRTP não passa aqui).

Protocolo: `docs/connectivity/PROTOCOL.md` secção C. Regras de segurança:
`docs/connectivity/SECURITY.md`.

## Correr

```bash
npm install
node server.mjs            # default 127.0.0.1:8787
PORT=8787 BIND=127.0.0.1 node server.mjs
TRUST_PROXY=1 node server.mjs   # só atrás de um proxy teu: confia em X-Forwarded-For
```

Node >= 22. Dependência de produção: só `ws`.

TLS = Caddy/nginx à frente. A app usa `https://dominio`.

## Caddy

```
rendezvous.example.com {
  reverse_proxy 127.0.0.1:8787
}
```

Systemd: restart on failure. Não há persistência a recuperar.

## Deploy (Docker)

Imagem `ghcr.io/<repo>/rendezvous` (linux/arm64), non-root `godrinking` (uid 10001),
porta 8787. Dois perfis no `docker-compose.app.yml`: `dev` e `prod`.

```bash
IMAGE_NAME=ghcr.io/<repo>/rendezvous docker compose -f docker-compose.app.yml --profile dev up -d
```

Deploy automático via GitHub Actions (workflow_dispatch):
`.github/workflows/deploy-rendezvous-dev.yml` e `deploy-rendezvous-prod.yml`.
Precisas dos secrets `ENV_RENDEZVOUS_DEV` / `ENV_RENDEZVOUS_PROD` (ficheiros
`.env.dev` / `.env.production`) e do runner `DEPLOY_RUNNER` (default `oracion`).

A rede é a bridge default — o NPM (Nginx Proxy Manager) faz proxy pela porta do
host, não pela rede docker. Se um dia quiseres NPM na rede docker, adiciona a
network externa `npm` (exemplo comentado no compose).

## Limites (não negociáveis)

| Coisa | Valor |
|---|---|
| Rooms | 256 |
| WS | 512 |
| Viewers por Room | 8 accepted + 8 pending |
| Room code | `^[A-Z0-9]{6}$` |
| Nickname | 2–24, `[A-Za-z0-9 _\-.]+` |
| Password | 0 ou 4–64 |
| Body | 64 KiB (acima: 413 e close) |
| Heartbeat | 30s envio / 5 min expirar (GC 15s) |
| Rate limit | ask 10/min, open 5/min, heartbeat 6/min, WS 20/min, resto 60/min (por IP) |
| Ignore list | 5 falhas/10 min → 15 min `denied` imediato (por IP) |
| Timeouts | headers 5s, request 15s, WS ping 30s |

## Testes manuais (obrigatórios)

Corre o servidor (`node server.mjs`) e executa por ordem. `H`/`V` são os tokens
que os comandos devolvem — substitui nos passos seguintes.

### 1. `open` + `ask` sem Password → accepted

```bash
curl -s -X POST localhost:8787/v1/host/open -d '{"code":"ABC123","nickname":"Ana","admission":false}'
# {"ok":true,"host_token":"H"}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC123","nickname":"Joao"}'
# {"ok":true,"status":"accepted","viewer_token":"V"}
```

### 2. `open` com Password; `ask` errada → denied; certa → ok

```bash
curl -s -X POST localhost:8787/v1/host/open -d '{"code":"ABC124","nickname":"Ana","password":"segredo","admission":false}'
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC124","nickname":"Joao","password":"errada"}'
# {"ok":false,"error":"denied"}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC124","nickname":"Joao","password":"segredo"}'
# {"ok":true,"status":"accepted","viewer_token":"V"}
```

### 3. `ask` a código inventado → denied (igual ao 2)

```bash
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ZZZZZZ","nickname":"Joao"}'
# {"ok":false,"error":"denied"}
```

### 4. Admission on: `ask` → pending; `decide accept` → signal offer/answer

```bash
curl -s -X POST localhost:8787/v1/host/open -d '{"code":"ABC125","nickname":"Ana","admission":true}'
# {"ok":true,"host_token":"H"}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC125","nickname":"Joao"}'
# {"ok":true,"status":"pending","viewer_token":"V"}
```

Depois, com os dois WS abertos (o Host recebe `pending` com o `viewer_id`,
aceita, e o Viewer recebe `accepted` + o signal):

```bash
HOST_TOKEN=H VIEWER_TOKEN=V node --input-type=module <<'EOF'
import WebSocket from "ws";
const base = "ws://127.0.0.1:8787/v1/ws";
const host = new WebSocket(`${base}?role=host&token=${process.env.HOST_TOKEN}`);
host.on("message", (raw) => {
  const msg = JSON.parse(raw.toString());
  console.log("host <-", msg.t, msg.viewer_id ?? "");
  if (msg.t === "pending") {
    fetch("http://127.0.0.1:8787/v1/host/decide", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ host_token: process.env.HOST_TOKEN, viewer_id: msg.viewer_id, action: "accept" }),
    });
  }
  if (msg.t === "signal") console.log("host <- signal (answer) OK");
});
const viewer = new WebSocket(`${base}?role=viewer&token=${process.env.VIEWER_TOKEN}`);
viewer.on("message", (raw) => {
  const msg = JSON.parse(raw.toString());
  console.log("viewer <-", msg.t);
  if (msg.t === "accepted") {
    viewer.send(JSON.stringify({ t: "signal", payload: { type: "answer", sdp: "v=0\r\n" } }));
  }
});
setTimeout(() => process.exit(0), 3000);
EOF
# esperado: viewer <- accepted; host <- pending <id>; host <- signal (answer) OK
```

### 5. Parar Heartbeat 5 min → `ask` `denied`

Deixa o servidor a correr **sem** mandar heartbeats durante 5+ minutos (ou
reinicia com `HEARTBEAT_TTL_MS` menor para testar rápido):

```bash
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC123","nickname":"Joao"}'
# {"ok":false,"error":"denied"}   (sala morreu no GC)
```

### 6. `rotate` código → `ask` no antigo `denied`, no novo ok

```bash
curl -s -X POST localhost:8787/v1/host/rotate -d '{"host_token":"H","code":"XYZ789"}'
# {"ok":true}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ABC125","nickname":"Joao"}'
# {"ok":false,"error":"denied"}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"XYZ789","nickname":"Joao"}'
# {"ok":true,"status":"accepted",...}
```

Um Viewer já accepted antes do rotate continua com o Token (o WS dele não cai).

### 7. 5 `ask` erradas do mesmo IP → 15 min `denied` imediato

```bash
for i in 1 2 3 4 5; do
  curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ZZZZZZ","nickname":"Joao"}'
done
# 5x {"ok":false,"error":"denied"}
curl -s -X POST localhost:8787/v1/viewer/ask -d '{"code":"ZZZZZZ","nickname":"Joao"}'
# {"ok":false,"error":"denied"}   (imediato, sem delay — IP na Ignore list)
```

## O que este servidor recusa-se a ter

- Disco, SQLite, ficheiros de salas
- Contas, email, OAuth
- Listagem `/v1/rooms`, métricas públicas, admin API
- TURN / UDP / RTP (Media nunca entra neste processo)
- CORS aberto (`Access-Control-Allow-Origin: *` proibido)