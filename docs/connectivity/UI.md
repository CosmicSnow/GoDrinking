# UI — campos e estados

Vocabulário da UI = `CONTEXT.md`. Inglês da UI atual (`Share screen`, `Watch`) mantém-se. Labels novos abaixo em inglês para bater com o app.

Não redesenhar o layout. Encaixar nos painéis que já existem.

---

## 1. Nickname (os dois lados)

Campo `Nickname` persistido em `localStorage` (`godrinking.nickname`). Obrigatório. Placeholder: `Your name`. Start/Join desativados se inválido.

Host: sidebar ou painel de sessão, sempre visível. Não é conta.

---

## 2. Seletor de Join mode

Segmented control **LAN | Direct | Stunar**, no Host (painel Share, acima do CTA) e no Viewer (painel Join, acima do código).

Default: LAN.

Texto de ajuda, uma linha:

| Modo | Host | Viewer |
|---|---|---|
| LAN | `Same network. They type your code.` | `Code from the host on your network.` |
| Direct | `They type your IP and port.` | `IP and port the host sent you.` |
| Stunar | `Internet. They type your code.` | `Code from the host. Needs the relay.` |

“relay” aqui é o Rendezvous — **não** escrever STUN na UI.

---

## 3. Host — por modo, depois do Start

### LAN (já existe, acrescentos)

- Room code copiável (já existe)
- Password opcional (input, masked, `Set password`)
- Toggle `Require approval`
- Roster

Mudar código: botão `New code` confirma. Password: editar e `Update`. Quem já está dentro fica.

### Direct

Não mostrar Room code.

Mostrar lista de endereços copiáveis, uma linha cada:

```
LAN     192.168.1.40:41234     [Copy]
Public  201.15.8.9:41234       [Copy]
IPv6    [2001:db8::2]:41234    [Copy]
```

Se não houver Public: omitir a linha, nota `No public IPv4. Direct over the internet may fail.`
Se não houver IPv6: omitir.
Se `mapping: false`: nota `Port mapping failed. Viewers on other networks need this port open.`

Password + Admission + Roster iguais ao LAN.

### Stunar

- Room code copiável
- Password + Admission + Roster
- Estado do Rendezvous: `Calling…` / `Live` / `Relay unreachable`
- Se a URL do Rendezvous estiver vazia: não deixa Start. Nota `Set the Stunar URL in settings.`

Settings (mínimo): um campo `Stunar URL` persistido (`godrinking.rendezvous_url`). Sem UI de STUN servers no v1 (default no código).

---

## 4. Viewer — Join

Campos comuns: Nickname. Join mode.

| Modo | Campos |
|---|---|
| LAN | Room code, Password (se o Host não tiver, o campo pode ir vazio) |
| Direct | Address, Port, Password |
| Stunar | Room code, Password |

Password sempre visível como campo opcional — o Viewer não sabe se o Host tem. Vazio = enviar vazio.

Estados do botão Join: `Join` → `Looking…` / `Waiting for approval…` → connected.

Erros, genéricos, **iguais** para senha errada e sala inexistente:

- `Could not join.`
- Direct: `Could not reach that address.` só quando TCP falha (timeout/refused). AUTH falhou = `Could not join.`
- Stunar unreachable = `Stunar is unreachable.`
- `full` = `This session is full.`
- Kick = `The host disconnected you.`
- Reject = `The host declined.`

---

## 5. Roster (Host)

Bloco `People` no painel direito, abaixo do preview.

Pending (só se Admission on e lista não vazia):

```
Joao    [Accept] [Decline]
```

Connected:

```
Joao    [Disconnect]
Ana     [Disconnect]
```

Host não aparece como Viewer. O Nickname do Host pode aparecer no canto: `Sharing as Ana`.

Disconnect = Kick. Confirmação não é necessária (é reversível: a pessoa volta a pedir).

Cap: se 8 connected, nota `Session full`.

---

## 6. Watch com vários Hosts? Não.

Um Viewer = uma Session. Join de novo substitui. Disconnect já existe (`disconnectWatch`).

---

## 7. Acessibilidade / copy

- Não mostrar IPs no modo LAN (o código chega).
- Não mostrar Tokens.
- Não mostrar SDP.
- Password: `type=password`. Toggle mostrar/esconder ok.
- Room code: monospace, maiúsculas, como hoje.

---

## 8. O que não fazer

- Não pedir IP no LAN.
- Não pedir código no Direct.
- Não chamar o modo de “STUN”.
- Não listar Viewers no lado Watch.
- Não auto-switch de modo se Join falhar.
