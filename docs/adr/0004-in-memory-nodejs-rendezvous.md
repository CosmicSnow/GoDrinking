# Rendezvous Node.js em memória, um processo

O servidor tem de ser tão pequeno que caiba numa VPS mínima e que outra LLM implemente sem framework.

**Decisão:** Node 22, `node:http` + `ws` + `node:crypto` (scrypt). Um processo. Map em RAM. Sem base de dados, sem Redis, sem contas. Reiniciar o processo apaga as salas — aceitável: o Host volta a abrir a sala com Heartbeat.

HTTPS na prática: TLS no reverse proxy (Caddy/nginx) à frente. A app fala `https://`. Horizontal scale fica de fora: um processo, um sítio.
