# SECURITY — não negociável

O código vai ser público. Assumir adversário com o repo aberto.

## 1. O Rendezvous não é de confiança para Media

Mesmo comprometido, só tem Signaling. DTLS-SRTP do WebRTC continua a cifrar Media. Mesmo assim: um Rendezvous mau pode **juntar-te ao Host errado**. Mitigação de produto: Password + Admission. Mitigação fora de âmbito: pinning de certificado do Host (não nesta versão).

## 2. Enumeração de salas

`ask` a um código inexistente e `ask` com Password errada são indistinguíveis: HTTP 404, `{ok:false,error:"denied"}`, tempo ~igual (delay 50–80ms).

Não há `HEAD`, não há timing óbvio no scrypt (correr delay mesmo sem hash).

Acima de 10 falhas em 10 min do mesmo IP, `ask` deixa de responder `denied` e responde `pending` falso (tarpit) — o scanner não sabe se a sala existe, se a Password está errada ou se está a ser enganado. Mesmo timing do `denied`.

## 3. Password

- **Obrigatória (4–64) em toda sala Stunar.** `open` sem Password → `invalid`; `rotate` com `""` → `invalid`. LAN/Direct continuam a permitir vazia.
- Nunca em logs, SDP, snapshot Tauri, query string, nome de ficheiro.
- Snapshot: `password_set: bool`.
- Rendezvous: scrypt, salt por sala.
- Host Direct/LAN: não precisa scrypt (o segredo não sai do processo); comparar em constant-time.
- Transmitida: HTTPS no Stunar; TCP em claro no Direct/LAN. Direct assume que o canal humano (WhatsApp) já levou o endereço — a Password é anti-scanner, não anti-MITM. Quem precisa de MITM-resistant usa Stunar.

## 4. Tokens

- 32 bytes `crypto.randomBytes`, hex.
- Um Token = um papel. Host Token não funciona em `ask`.
- Kick / reject / close / GC da sala invalidam.
- Rotate de Password **não** invalida Tokens já accepted. Rotate de código tampouco.

## 5. Ignore list

| | Valor |
|---|---|
| Janela de falhas | 10 minutos |
| Limiar | 5 falhas → 15 min; 10 → 1 h; 15 → 6 h; 20+ → 24 h |
| Duração | escala com o nível (acima) |
| Chave | IP do socket (ou XFF se `TRUST_PROXY=1`) |
| O que conta | Password errada, sala inexistente (no Rendezvous, para não dar oráculo), PROTO lixo no TCP |
| O que não conta | Reject, Kick, FULL, Timeout Pending |

No Host Direct: IP ignorado → `ERR BANNED` e close, **antes** de ler NICK. No Rendezvous: `denied` (mesmo body).

**Tarpit (Rendezvous):** depois de 10 falhas em 10 min, `ask` responde `pending` falso com `viewer_token` dummy — o WS desse token manda `roster` vazio, ignora mensagens e fecha aos 60s. Tokens falsos: máx 100, TTL 2 min. O IP continua a escalar a Ignore list normalmente.

IPv6: /128 no v1 (não agregamos /64). Documentar que um atacante com prefixo grande contorna; v1 aceita.

## 6. Rate limit (Rendezvous)

Por IP, além da Ignore list:

| Rota | Limite |
|---|---|
| `POST /v1/viewer/ask` | 10 / min |
| `POST /v1/host/open` | 5 / min |
| `POST /v1/host/heartbeat` | 6 / min (o cliente manda 2/min; folga) |
| WS upgrade | 20 / min |
| resto | 60 / min |

Exceder → 429 `{ok:false,error:"busy"}`. Sem `Retry-After` variável que vaze estado.

## 7. Tamanhos e abusos baratos

- Body 64 KiB. Acima: 413 e close.
- Nickname 2–24, charset: letras, números, espaço, `_-.`. Sem control chars.
- Room code: `^[A-Z0-9]{6}$`.
- Máx 256 salas, 512 WS, 8+8 Viewers.
- Timeout HTTP 15s. WS ocioso 120s sem ping.
- Headers: `Connection: close` em respostas REST. Slowloris: timeout de headers 5s (se o runtime deixar).

## 8. DDoS

Node **não** aguenta volumetric. Camadas:

1. Este processo: limites acima.
2. Reverse proxy: `limit_req`, max body, timeouts.
3. Rede: Cloudflare ou equivalente **opcional**, proxy laranja, WebSockets ligados.
4. Não abrir `0.0.0.0` sem proxy na VPS de produção.

Não implementar “anti-DDoS” caseiro com CPU pesado (captcha, proof-of-work) no v1.

## 9. SDP / Signaling

- Só `type` offer|answer + `sdp` string.
- Recusar outro JSON (ice-trickle avulso, datachannels, URLs).
- Não logar SDP (tem IPs).
- Host só recebe answer do `viewer_id` que aceitou. Viewer só recebe offer da sala do Token.

## 10. ICE / STUN

STUN público vê IPs. Esperado. Não enviar Password ao STUN (o protocolo não manda).

LAN: `ice_servers` vazio. Não “ligar STUN por defeito” no LAN — fura o modelo “sem internet”.

## 11. Direct exposto

Porta TCP no IP público = superfície. AUTH primeiro. Ignore list. Sem banner (`goDrinking/1.0` proibido). Primeira linha inválida → close sem texto.

UPnP: só a porta desta Session. Remover mapeamento no Stop. Se o Stop falhar, o mapeamento pode ficar — documentar.

## 12. Admission

Pending **não** recebe SDP. Accept é o único gatilho. Um Viewer não aceita outro Viewer.

## 13. App opensource

Não meter segredos no repo (URL default pode ser público). `host_token` não vai para analytics. Issues/PRs: colar SDP = vazar IP; o README do Rendezvous avisa.

## 14. Checklist de review

- [ ] Nenhuma rota revela se o código existe
- [ ] Nenhuma Password em log
- [ ] Nenhum SDP em log
- [ ] Media não tem socket no processo Node
- [ ] Kick invalida Token
- [ ] Heartbeat 5 min mata a sala
- [ ] LAN sem STUN
- [ ] Direct não fala com o Rendezvous
- [ ] Código é gerado no servidor; `rotate` não aceita `code`
- [ ] Password obrigatória (4–64) em toda sala Stunar; `open`/`rotate` recusam vazia
- [ ] Tarpit: tokens falsos ≤ 100, TTL 2 min, WS fecha aos 60s, timing igual ao `denied`
