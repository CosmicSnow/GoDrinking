# goDrinking

Screen share P2P entre um Host e um ou mais Viewers. A mídia nunca passa por um servidor. Um servidor, se existir, só apresenta os dois lados.

## Language

**Host**:
A pessoa que captura e envia o ecrã.
_Avoid_: streamer, sender, broadcaster, server (para esta pessoa)

**Viewer**:
A pessoa que recebe e assiste o ecrã.
_Avoid_: client, peer (sozinho), spectator, watcher

**Session**:
Uma transmissão ao vivo, do Start ao Stop do Host. Viewers entram e saem da Session; a Session não recomeça quando um Viewer cai.
_Avoid_: call, room (quando quiseres dizer a transmissão inteira)

**Join mode**:
Como um Viewer encontra o Host: LAN, Direct, ou Stunar.
_Avoid_: protocol, connection type, network mode

**LAN**:
Join mode em que o Viewer usa um Room code e descoberta por broadcast na rede local. É o modo que já existe.
_Avoid_: local, same network (como nome do modo)

**Direct**:
Join mode em que o Viewer liga ao endereço IP e porta que o Host mostra. Sem broadcast. Sem Rendezvous.
_Avoid_: IP mode, manual, unicast (como nome do modo)

**Stunar**:
Join mode em que Host e Viewer falam com o Rendezvous por HTTPS usando Room code (e Password, se houver). O Rendezvous só apresenta os dois. Não é STUN.
_Avoid_: STUN mode, cloud mode, online mode, signaling mode (como nome do produto)

**Rendezvous**:
O processo Node.js que guarda salas vivas e troca recados de sinalização. Não vê, não guarda, não reencaminha mídia.
_Avoid_: STUN server, TURN, media server, VPS (quando quiseres dizer o programa), signaling server (ok em spec técnica, não no glossário de produto)

**Room code**:
O código curto que identifica a Session no LAN e no Stunar. Não se usa para entrar no Direct.
_Avoid_: PIN, invite, token (o Token é outra coisa)

**Password**:
Segredo opcional escolhido pelo Host. Quem entra precisa dele além do Room code (LAN/Stunar) ou do endereço (Direct).
_Avoid_: PIN, key, secret (como nome de campo na UI)

**Nickname**:
Nome que Host e Viewer declaram para aparecer no Roster. Não é conta. Não é único.
_Avoid_: username, handle, identity, account

**Roster**:
A lista que o Host vê: quem pediu para entrar e quem já está dentro.
_Avoid_: user list, peers, participants panel (ok na UI spec, não no domínio)

**Admission**:
Regra da Session: se ligada, o Host tem de aceitar cada Viewer antes da sinalização. Se desligada, Password + Room code (ou endereço) bastam.
_Avoid_: lobby, approval, whitelist, knock

**Ignore list**:
IPs que o Host (Direct/LAN) ou o Rendezvous (Stunar) recusam por um tempo depois de falhas repetidas.
_Avoid_: ban, blocklist, firewall (o Ignore list é temporário e local)

**Heartbeat**:
Sinal periódico do Host ao Rendezvous a dizer que a Session ainda existe. Sem Heartbeat durante 5 minutos, a sala some.
_Avoid_: ping, keepalive (ok no fio, não no domínio)

**Token**:
Segredo opaco que o Rendezvous dá depois de um pedido aceite. Substitui a Password no resto da conversa.
_Avoid_: session id, cookie, API key

**Signaling**:
Troca de offer/answer WebRTC (e recados de Admission/Kick). Não é o vídeo.
_Avoid_: handshake (sozinho), connection, STUN

**Media**:
Os pacotes WebRTC de vídeo/áudio entre Host e Viewer. Nunca entram no Rendezvous.
_Avoid_: stream (quando quiseres dizer os bytes), transmission (quando quiseres dizer os pacotes)

**STUN**:
Espelho público que diz a cada PC o IP:porta visto de fora. Não junta pessoas. Não guarda Room code.
_Avoid_: Stunar, Rendezvous

**ICE**:
O mecanismo WebRTC que tenta os endereços possíveis até a Media ligar.
_Avoid_: hole punch (é uma tática do ICE, não o nome)

**TURN**:
Relay de Media. Fora de âmbito nesta versão.
_Avoid_: Rendezvous (o Rendezvous não é TURN)

**Broadcast**:
Modo de Session em que uma pessoa captura e as outras só assistem. É o default. É o modelo actual (1 Host, N Viewers).
_Avoid_: classic, one-way, presenter mode, webinar

**Sala**:
Modo de Session em que cada Membro pode capturar (Share slot) e assistir aos outros. O Rendezvous (ou o Master, em LAN) guarda o código enquanto houver gente. Não é a Session inteira — ver Session.
_Avoid_: room (quando quiseres dizer Broadcast), call, meeting, lobby

**Master**:
O Membro que pode mudar a Password, a Admission e kickar. Quem abre a Sala começa Master. Se sair, a coroa passa a quem entrou a seguir e ainda está dentro.
_Avoid_: owner, admin, host (Host continua a ser quem captura), moderator

**Membro**:
Uma pessoa dentro de uma Sala (capturando ou não). Em Broadcast não se usa: aí há Host e Viewer.
_Avoid_: participant, peer, user

**Share slot**:
A captura de um Membro na Sala: parada ou a enviar Media P2P para os outros. Vários slots podem estar Live ao mesmo tempo.
_Avoid_: stream (os bytes), session (a Session é o contentor), camera

**Customize**:
Painel fechado por omissão com resolução, fps, codec, encoder e bitrate. Power user. Os presets Low/Medium/High não dependem dele.
_Avoid_: advanced settings, expert mode, quality panel (quando for o painel extra)

**Benchmark**:
Medição local do encoder neste PC para recomendar Low, Medium ou High. Não abre Session. Não fala com Viewer nem com o Rendezvous.
_Avoid_: speed test, network test (é encode, não rede)

**Play Together**:
Futuro: Viewer manda um comando (XInput) ao PC de quem está a capturar, por DataChannel P2P. Off por omissão. Não implementado.
_Avoid_: Parsec (é a analogia, não o nome), remote play, netplay
