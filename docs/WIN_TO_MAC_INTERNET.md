# Win → Mac via internet não conecta — diagnóstico completo

> Data: 2026-09-04. Para o Jouy. Contexto: goDrinking 0.1.0, Host no Windows (RTX 3070), Viewer no Mac, via Stunar (`https://together.jouymaker.com/`).

## Resumo executivo

O vídeo **sai** do Windows (provado por log), mas **nunca chega** ao Mac quando estão em redes diferentes. A causa é a rede do Windows, não o app: ela usa **NAT simétrico**, que invalida o endereço público que o Host anuncia — o Mac tenta conectar nesse endereço para sempre (ICE parado em `checking`, `Waiting for media…`). Na direção contrária (Mac → Win) funciona porque o NAT do Mac aceita conexões de entrada. Na mesma rede (ou mesmo PC) funciona porque o NAT nem entra no caminho. A solução para internet é um relay **TURN** (servidor a instalar + mudança no app). Alternativas sem instalar nada: mesma rede ou Tailscale.

## 1. Sintomas observados (com evidência)

| Cenário | Resultado | Evidência |
|---|---|---|
| Win Host → Win Viewer, mesmo PC | ✅ Funciona | Confirmado pelo usuário em 2026-09-04 |
| Mac Host → Win Viewer, internet via Stunar | ✅ Sempre funcionou | Relato do usuário |
| Win Host → Mac Viewer, internet via Stunar | ❌ Nunca funcionou | Viewer parado em ICE `checking`, `Waiting for media…`; no Host, peers do Viewer morrem e renascem (ofertas repetidas `mint offer` p/ o mesmo Viewer no log) |
| Win Host → qualquer Viewer, antes do fix | ❌ Tela preta p/ todos | Causa separada, já corrigida (ver §5) |

Log do Host (Win) no caso quebrado — a mídia é produzida e entregue à rede, mas o peer nunca conecta:

```
INFO  encoder: engaged OpenH264 software encoder
INFO  encoder: first frame accepted (1920x540)
INFO  mint offer: viewer=dfa0e770 nickname=Jouy
INFO  pump: first access unit received (keyframe=false, 340 bytes, profile=Some("42c028"))
INFO  stunar ws message: answer from viewer=dfa0e770
INFO  pump: first keyframe seen, starting stream
INFO  pump: first sample written (36393 bytes)
... (peer morre, Viewer tenta de novo)
INFO  mint offer: viewer=dfa0e770 nickname=Jouy   ← re-mint: o peer anterior falhou
```

Ou seja: sinalização (Stunar) OK, captura OK, encode OK, envio iniciado — e a conexão ICE nunca se completa.

## 2. Como uma chamada WebRTC atravessa a internet (o mínimo necessário)

1. Cada lado descobre seus **candidatos** = endereços onde diz "me encontre aqui":
   - **host**: IPs locais (`192.168.x.x`) — só valem na mesma rede;
   - **srflx**: IP:porta públicos vistos "de fora", descobertos perguntando a um servidor **STUN** ("que endereço você vê em mim?").
2. Os candidatos do Host viajam na **oferta SDP** (via Rendezvous) até o Viewer.
3. Os dois lados testam pares de candidatos (**ICE `checking`**) até achar um caminho bidirecional de pacotes UDP.
4. Achado o caminho, acontece o handshake criptográfico (DTLS) e a mídia flui.

O ponto frágil é o passo 3: o candidato srflx só presta se o roteador/NAT **mantiver** aquele IP:porta válido e **aceitar pacotes de entrada** nele.

## 3. A causa: NAT simétrico no lado Windows (provado por teste)

Teste executado no próprio PC Windows em 2026-09-04 — duas perguntas STUN seguidas ao `stun.l.google.com`, de portas locais diferentes:

```
STUN server: 74.125.250.129
Local 54320 -> mapped 186.205.17.51:6830
Local 54321 -> mapped 186.205.17.51:6831
RESULTADO: NAT SIMÉTRICO (mapeamentos diferentes)
```

Cada conexão de saída ganhou uma **porta pública diferente** (`6830` vs `6831`). Consequência: o endereço srflx que o Host Windows coloca na oferta **já nasce morto** — quando o Mac manda pacotes para ele, o NAT descarta ("não conheço essa encomenda"). O ICE do Mac fica em `checking` para sempre. Não há mensagem de erro porque, do ponto de vista do protocolo, "ainda pode conectar" — ele tenta até desistir e tentar de novo (os re-mints do log).

### Por que é assimétrico (tabela das direções)

| Direção | O que acontece | Resultado |
|---|---|---|
| Mac Host → Win Viewer | Win manda pacotes **de dentro pra fora** (qualquer NAT permite sair); NAT do Mac **aceita a entrada** | ✅ Funciona |
| Win Viewer, mesmo PC | Loopback `127.0.0.1`; NAT nem participa | ✅ Funciona |
| Mesma rede Wi-Fi/cabo | Candidatos locais `192.168.x.x` diretos; NAT nem participa | ✅ Funciona |
| Win Host → Mac Viewer, internet | Mac manda pacotes para o srflx do Win; NAT simétrico **descarta a entrada** | ❌ `checking` eterno |

Analogia: o Mac mora num condomínio cujo porteiro aceita encomendas avisadas; o Windows mora num onde o porteiro troca a fechadura a cada saída e só entrega o que você foi buscar pessoalmente. Sair funciona dos dois lados; **entrar** só no Mac.

## 4. O que foi descartado (com prova)

- **Firewall do Windows**: regras `goDrinking` existem e liberam TCP/UDP entrada e saída em **todos os perfis** (Domain, Private, Public). Verificado com `Get-NetFirewallRule`.
- **Codec/perfil**: o fluxo é H.264 Constrained Baseline (`42c028`), decodificável em qualquer browser. Além disso, problema de codec apareceria *depois* do ICE conectar — aqui o ICE nem sai do `checking`.
- **Captura/encode no Windows**: preview do Host funciona (16,9 fps, milhares de frames) e o log mostra frames aceitos + keyframe + amostra de 36 KB escrita no WebRTC.
- **Sinalização Stunar**: oferta/resposta trocadas e confirmadas no log dos dois lados.
- **Bug do app**: dois bugs reais foram achados e corrigidos nesta sessão (ver §5) — mas nenhum deles explica ICE parado em `checking` entre redes diferentes.

## 5. Contexto: o que já foi corrigido nesta sessão (2026-09-03/04)

1. **Portable em dev-mode**: o `.exe` distribuído carregava `http://localhost:1420` (`ERR_CONNECTION_REFUSED`). Causa: build com `cargo` puro, sem o feature `custom-protocol` que só o `tauri build` ativa. Rebuild correto + reupload.
2. **Tela preta para todos os Viewers no Windows**: `AccessUnitQueue` tinha um `Drop` que fechava a fila compartilhada quando um clone temporário era descartado (`create_windows_encoder` clonava a fila) — todo `try_push` retornava `Closed` para sempre. Zero frames com ICE conectado e preview normal; só no Windows (no macOS a fila é movida, não clonada). Removido o `Drop`; shutdown por `close()` explícito + flags.

## 6. Soluções comparadas

| Solução | Funciona p/ internet? | Esforço | Custo | Privacidade | Nota |
|---|---|---|---|---|---|
| **TURN relay** | ✅ Sim | Médio (servidor + app) | Banda do VPS (~4–8 Mbps por viewer remoto) | Relay vê só pacotes criptografados (DTLS/SRTP), não o conteúdo | Solução padrão da indústria (Meet/Zoom usam relays) |
| **Tailscale** (ambos na mesma Tailnet) | ✅ Provável | Zero código (5 min de teste) | Grátis (usa DERP da Tailscale quando preciso) | Túnel criptografado fim a fim | IPs `100.x` viram candidatos diretos; o Windows já tem Tailscale instalado |
| **Mesma rede** | ✅ Sim | Zero | Zero | Total (LAN) | Limita o uso |
| **UPnP / port forward** | ⚠️ Às vezes | Baixo, mas frágil | Zero | Total | Roteador abre porta inbound; quebra com CGNAT e não atravessa NAT simétrico de operadora |
| **IPv6 público nas 2 pontas** | ✅ Se existir | Zero (se já houver) | Zero | Total | Sem NAT no IPv6; depende do provedor entregar /56 ou similar nos dois lados |

## 7. Proposta TURN (escopo, se aprovado)

**No servidor (VPS, feito pelo dono — este agente não opera outras máquinas):**
- Subir `coturn` (1 container Docker ao lado do Rendezvous; o repo já tem compose): UDP 3478 (+ 5349 p/ TLS), realm + segredo compartilhado com o Rendezvous.
- 1 endpoint novo no `rendezvous/server.mjs` gerando credencial temporária (padrão TURN REST API: `username = expiry:userid`, `password = HMAC(secret)`). Nada hardcoded no app.

**No app (feito aqui):**
- Host (webrtc-rs) e Viewer (browser `RTCPeerConnection`): incluir `turn:`/`turns:` com credencial obtida no Rendezvous ao entrar, nos modos Stunar (e Direct com IP público). LAN continua sem TURN.
- Rebuild + portable novo no release `v0.1.0`, como das outras vezes.

**Estimativa de banda**: ~alvo do encoder por viewer remoto (ex.: 4–8 Mbps). Verificar a franquia do VPS antes.

## Apêndice — como reproduzir o diagnóstico

```powershell
# 1. Firewall (descarta bloqueio local)
Get-NetFirewallRule -DisplayName "goDrinking*" | Select-Object DisplayName, Profile, Enabled
# 2. Tipo de NAT (script em $env:TEMP/opencode/stun-nat-test.ps1):
#    duas perguntas STUN de portas locais diferentes; portas públicas
#    diferentes ⇒ NAT simétrico ⇒ srflx inbound inútil ⇒ TURN obrigatório.
# 3. No log do Host: "mint offer" repetido p/ o mesmo viewer + ausência de
#    "first sample written" nos pumps dele ⇒ peer nunca conectou (ICE).
# 4. No Status do Viewer (Mac): ICE preso em "checking", bitrate zerado.
```
