# goDrinking

Não é um app de bar. Não é Golang. Ninguém aqui está "going drinking".

Em português a gente fala "vai tomando". Alguém traduziu isso pro inglês, ficou goDrinking, e agora o GitHub acha que somos um clube da cachaça escrito em Go. Somos um screen share P2P na LAN. Discord de pobre. Sem servidor na nuvem, sem STUN público, sem a sua tela passando por um datacenter em Virginia.

Se você abriu isto procurando drink recipes: fecha a aba. Se abriu procurando `fmt.Println`: também.

## o que é

Dois (ou mais, se a LAN aguentar o drama) Macs na mesma rede. Um compartilha tela. O outro assiste. Código de 6 caracteres. WebRTC host-only, captura nativa no macOS (ScreenCaptureKit + VideoToolbox), viewer no WKWebView.

Áudio de sistema é opcional. Dá pra tirar o Discord do mix pra você continuar xingando no headset sem o espectador ouvir. Em tese. Se o Core Audio acordar do lado certo da cama.

## o que não é

- Não é Go
- Não é um tracker de shots
- Não é Discord
- Não é "a gente sobe um TURN e resolve"
- Não é o Rendezvous a ver o teu ecrã. Nunca.
- Não é `npm run tauri dev` como veículo de teste de captura (o TCC não gruda nesse binário)

## como usar

No Mac, a partir da raiz:

```bash
npm install
npm run macos:app
```

Isso gera um `.app` debug, tenta assinar, e abre. Screen Recording precisa estar ligado pra **goDrinking** em Ajustes do Sistema. Se você recusou o diálogo da maçã por acidente, o app congela a alma e você força o quit. Já aconteceu. Vai acontecer de novo.

### host

1. Share screen
2. Liga Screen Recording se o macOS ainda não confia em você
3. Escolhe tela inteira ou janela
4. Qualidade: Low / Medium / High (H.264, 720p30 / 1080p30 / 1080p60). Customize se fores power user (codec, 1440p, bitrate).
5. System audio se quiser. Chip nos apps que não devem ir na mix (Discord, Spotify, o seu pai no Zoom)
6. Start. Aparece o picker do macOS. Escolhe a fonte.
7. Copia o código de 6 letras. Manda no WhatsApp da família. Ou grita.

### watch

1. Watch
2. Cola o código
3. Join
4. Se conectar, a tela muda. Volume no hover. Video only. Fullscreen. Esc sai, uma camada por vez, como um adulto.

Dois processos no mesmo Mac funcionam (loopback ICE). Duas máquinas na mesma LAN também, se o roteador não for um tijolo com Wi-Fi.

## como funciona (versão curta)

Host captura com ScreenCaptureKit, encode H.264 Baseline no VideoToolbox, manda com webrtc-rs. Sinalização é UDP 17424 + TCP na LAN. Sem nuvem. O viewer é `RTCPeerConnection` no WebView. Se alguém perguntar "cadê o servidor", a resposta é "na sua casa, desligado, como deveria ser".

## stack

Tauri 2, React 19, Rust. macOS only pra captura. Windows é um comentário sarcástico no futuro.

## licença

[PolyForm Noncommercial 1.0.0](LICENSE). Código visível. Uso pessoal, hobby e organizações não-comerciais: ok. Uso comercial: só com autorização explícita. Isto não é Open Source OSI.
