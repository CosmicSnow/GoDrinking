# Códigos gerados no servidor e tarpit no Rendezvous

O Host não escolhe o Room code Stunar, e o Rendezvous tem de aguentar scanners sem revelar salas nem gastar CPU.

**Decisão:** o servidor gera o código no `open` — 6 chars `A-Z0-9` via `randomBytes` base36, retry até 10 se colidir com sala viva, `busy` se não houver código livre — e devolve-o com o `host_token`. O Host deixa de enviar código; `rotate` deixa de aceitar `code` (só Password). Password **obrigatória** (4–64) em toda sala Stunar; `open`/`rotate` recusam vazia.

Ignore list escalada por IP (janela 10 min): 5 falhas → 15 min, 10 → 1 h, 15 → 6 h, 20+ → 24 h. Acima de 10 falhas em 10 min, `ask` responde `pending` falso (tarpit): `viewer_token` dummy, WS que manda `roster` vazio, ignora mensagens e fecha aos 60s; máx 100 tokens, TTL 2 min, mesmo timing do `denied` (scrypt dummy + 50–80 ms).