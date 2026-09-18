<p align="center">
  <img src="app/icons/icon.png" width="96" height="96" alt="Ícone do goDrinking" />
</p>

<h1 align="center">goDrinking</h1>

<p align="center">
  Divide a tela. Não seus dados.<br />
  Site do projeto: <a href="https://godrinking.jouymaker.com">godrinking.jouymaker.com</a>
</p>

---

O goDrinking é um app de sala com tela compartilhada para Windows e macOS. Você cria uma sala com senha, passa um código de 6 letras para quem você quer chamar, e cada pessoa escolhe o que compartilhar e o que assistir. O vídeo vai direto de um PC para o outro por WebRTC. O servidor só apresenta as pessoas e segura a sala aberta, ele nunca recebe a imagem da sua tela.

O nome confunde de propósito. Não é Go, não é app de bar, e não tem nada a ver com bebida. Em português a gente fala "vai tomando", alguém traduziu e ficou goDrinking.

## O que dá para fazer

- Abrir uma sala com senha e chamar gente com um código curto.
- Usar apelido simples, sem conta e sem login.
- Compartilhar a tela inteira ou só uma janela, com miniatura antes de confirmar.
- Assistir ao compartilhamento de uma ou mais pessoas da sala.
- Ajustar qualidade (resolução, fps e bitrate) e compartilhar o áudio do PC, com opção de tirar apps específicos da mistura.
- Funciona entre sistemas diferentes, o host pode estar no Windows e quem assiste no macOS, ou o contrário.

## O que o projeto não é

O servidor não enxerga a sua transmissão. Ele troca só a sinalização para os participantes se acharem e mantém um websocket para mostrar quem está na sala. A mídia nunca passa por ele, e não existe relay. Sem TURN no caminho, se a conexão direta não fecha (o caso comum é quem está atrás de CGNAT restritivo), o vídeo não conecta. Isso é uma decisão de privacidade, não um bug de configuração.

O código é aberto e o app é exatamente o que está aqui no repositório. Dá para usar no trabalho, em reunião, suporte, estudo. O que a licença não permite é pegar o código e revender, como fork comercial, white-label ou SaaS do mesmo app. A licença é PolyForm Noncommercial, veja o arquivo `LICENSE`.

## Como baixar

O jeito normal é baixar o instalador pronto na página de releases:

- Releases: https://github.com/CosmicSnow/GoDrinking/releases
- A página do site também aponta para a release mais recente: https://godrinking.jouymaker.com

Tem build para Windows e para macOS (Apple Silicon). Baixe só desses dois endereços. 

<ATENÇÃO!!!>

Não aceite o instalador repassado por outra pessoa, por link solto em grupo, DM ou qualquer fonte fora do site e do repositório. Um arquivo repassado pode ter sido alterado no caminho e não tem como garantir que é o mesmo que saiu daqui. Se você instalar algo que não veio das fontes oficiais, a conta é sua: eu não sou responsável por modificação que terceiros tenham feito no arquivo.

</ATENÇÃO!!!>

Se o sistema reclamar que o app não tem assinatura digital, é esperado. Assinar custa dinheiro e este é um projeto independente, o código aberto está aqui para você conferir ou compilar por conta própria.

No macOS, abra o app uma vez, depois vá em Ajustes do Sistema, Privacidade e Segurança, e clique em Abrir Mesmo Assim. No Windows, na tela "O Windows protegeu o PC", clique em Mais informações e depois em Executar mesmo assim. Passo a passo oficial: [Apple](https://support.apple.com/en-us/102445) e [Microsoft](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation).

## Como usar

Tudo acontece na janela principal, não tem conta nem configuração prévia.

Para começar uma sala, escolha um apelido (letras, números, espaço, ponto, hífen e underline, de 2 a 24 caracteres), escreva uma senha (de 4 a 64 caracteres) e clique em Criar sala. O app mostra um código de 6 caracteres. Copie e mande para quem você quer chamar, por onde for mais fácil.

Para entrar, a pessoa coloca o mesmo código, o apelido dela e a senha, e clica em Entrar. Se a sala pedir aprovação, ela aguarda a líder admitir. Dentro da sala, o botão de compartilhar lista as telas e janelas com miniatura. É só escolher e confirmar. Para assistir, clique em Assistir ao lado de quem está compartilhando. O vídeo abre em uma janela própria, dá para fixar e dar zoom. Sair da sala é o botão de sair, e a líder pode admitir ou remover gente.

O campo de servidor quase nunca precisa mudar. O padrão já aponta para o servidor público. Você só mexe ali se hospedar o seu próprio servidor na sua rede, aí é só colar o endereço e usar normal.

## Rodar a partir do código

Você só precisa disso se for desenvolver ou compilar por conta própria. Para só usar o app, a seção de download acima resolve.

Pré-requisitos: Rust e Cargo, Node com npm, e Python para a suíte de verificação. No macOS, o Xcode. No Windows, o MSVC com o SDK.

O servidor fica em `server/` e roda com npm e node:

```bash
cd server
npm ci
node server.mjs
```

Por padrão ele escuta em loopback. Se for expor via Docker ou proxy, a configuração de bind muda, veja os comentários no próprio `server/`.

O frontend fica em `app/web/`:

```bash
cd app/web
npm ci
npm run dev
```

O build do frontend vem sempre antes do build do desktop, porque o Tauri serve o que está em `web/dist`:

```bash
cd app/web
npm run build

cd ../..
cd app
cargo tauri dev -- --bin goDrinking
```

Para gerar o instalador, mesmo esquema, com o frontend pronto primeiro:

```bash
cd app/web
npm run build

cd ../..
cd app
cargo tauri build
```

Quem compila fora do CLI do Tauri (por exemplo cross para Windows com `cargo xwin`) precisa ligar a feature `tauri/custom-protocol`, senão o binário aponta para a URL de dev em vez do frontend embutido.

Testes: `npm test` dentro de `server/` e dentro de `app/web/`, `cargo test` dentro de `app/`, e `python3 scripts/verify.py` na raiz para a suíte completa. O contrato da sinalização está descrito em `server/PROTOCOL.md`.

## Como o projeto se organiza

```
app/              shell Tauri, comandos, ponte de captura, IPC do helper de vídeo
app/web/          frontend React com as telas de lobby, sala e player
core/             estado da sala e mídia WebRTC e H.264, sem Tauri e sem código de OS
platform*/        captura de tela por sistema atrás do mesmo trait VideoSource
server/           rendezvous de sinalização em Node, mais o protocolo em PROTOCOL.md
site/             página de apresentação
scripts/          verificação, análise de mídia e apoio a teste
```

Captura de tela pede permissão no primeiro uso, a partir de um gesto seu. No macOS, se você negar, o sistema marca a negação e você libera em Ajustes do Sistema, Privacidade, Gravação de Tela. Um bundle id novo gera um pedido novo, isso é do sistema, não do app.

## Se der problema

Abra um issue contando o que aconteceu, o que você esperava, se é Windows ou macOS, e o passo a passo para reproduzir. Print e trecho de log ajudam. Pull request é bem-vindo. Senhas, tokens e SDP nunca vão para log nem para o frontend, então pode colar o log sem medo de vazar credencial, mas confira antes de enviar.

## Links

- Site: https://godrinking.jouymaker.com
- Releases: https://github.com/CosmicSnow/GoDrinking/releases
- Código: https://github.com/CosmicSnow/GoDrinking
- Licença: PolyForm Noncommercial, arquivo `LICENSE`
