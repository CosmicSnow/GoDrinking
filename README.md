# goDrinking

Não é um app de bar. Não é Golang. Ninguém aqui está "going drinking".

Em português a gente fala "vai tomando". Alguém traduziu isso pro inglês, ficou goDrinking, e agora o GitHub acha que somos um clube da cachaça escrito em Go. Somos um screen share P2P. Sem servidor na nuvem, sem a sua tela passando por um datacenter em Virginia.

Se você abriu isto procurando drink recipes: fecha a aba. Se abriu procurando `fmt.Println`: também.

Windows 10/11 e macOS 14.2+ (Apple Silicon). Site: [godrinking.jouymaker.com](https://godrinking.jouymaker.com). Instaladores na [última release](https://github.com/CosmicSnow/GoDrinking/releases/latest).

## o que é

Dois PCs (ou mais). Um compartilha tela. O outro assiste. Código de 6 caracteres. O vídeo vai direto de uma máquina para a outra (WebRTC). O servidor, quando entra, só apresenta os lados. RTP não passa por ele.

Dois modos:

- **Broadcast:** um Host, N Viewers.
- **Sala:** todo mundo pode compartilhar. Cada um escolhe quem assistir. Grelha, pin, zoom.

Três jeitos de se achar:

- **LAN:** mesma Wi-Fi. Sem internet.
- **Direct:** IP:porta que o Host mostra.
- **Stunar:** redes diferentes. O Rendezvous só troca recado. Não é STUN. Não vê o vídeo.

Áudio de sistema é opcional. Dá pra tirar apps da mix (o chat de voz, o Spotify, o seu pai no Zoom).

## o que não é

- Não é Go
- Não é um tracker de shots
- Não é "a gente sobe um TURN e resolve"
- Não é o Rendezvous a ver o teu ecrã. Nunca.
- Não é `npm run tauri dev` como veículo de teste de captura no Mac (o TCC não gruda nesse binário)

## baixar

[Releases](https://github.com/CosmicSnow/GoDrinking/releases/latest). Windows: `setup.exe` ou `.msi`. macOS: `.dmg` (Apple Silicon).

Requisitos: Windows 10/11 (GTX 1050+), macOS 14.2+ (M1+).

## como usar

1. Abre o app. Share.
2. Tela ou janela. Qualidade Low / Medium / High (720p30 / 1080p30 / 1080p60). Áudio se quiser.
3. Broadcast ou Sala. Start.
4. Copia o código de 6 letras. Manda no chat. Ou grita.
5. Do outro lado: Watch, cola o código, Join.

Na Sala, Watch/Stop é por pessoa. Leave volta para a tela de antes.

Dois processos no mesmo PC funcionam. Duas máquinas na mesma LAN também, se o roteador não for um tijolo.

## como desenvolver

Precisa de Node, Rust (e no Mac, Xcode / Screen Recording ligado para **goDrinking** em Ajustes do Sistema).

```bash
npm install
npm run tauri dev
```

No Mac, o binário que o TCC reconhece é o `.app`:

```bash
npm run macos:app
```

Se você recusou o diálogo de Screen Recording por acidente, o app congela a alma e você força o quit. Já aconteceu. Vai acontecer de novo.

Build de release:

```bash
npm run release:macos
npm run release:windows
```

## contribuir

O código está aqui para ser lido, compilado, quebrado e consertado. Se algo no app te irrita, abre uma issue. Se você já sabe o patch, abre um PR.

Coisas que ajudam de verdade: Windows e macOS (os dois lados), captura, WebRTC, Sala (watch/unwatch, grelha, pin), i18n, o site. Não precisa pedir permissão para um typo. Para mudança grande, uma issue primeiro evita retrabalho.

`dsh-plugins/` no working tree não entra no repo. Deixa quieto.

## licença

[PolyForm Noncommercial 1.0.0](LICENSE). Código visível. Isto não é Open Source OSI: não pode pegar o código e vender como se fosse seu.

Usar o app no trabalho pode. Reunião, suporte interno, ensinar o colega, o que for. O que está vedado é revender o código: fork comercial, app white-label, SaaS cobrando pelo mesmo programa.

Dúvida sobre um caso específico: abre uma issue.
