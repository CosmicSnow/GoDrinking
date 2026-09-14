# Verificação reutilizável

Na raiz do repositório, depois de instalar as dependências:

```sh
npm --prefix server ci
npm --prefix app/web ci
cd app/web
npx playwright install chromium
cd ../..
python3 scripts/verify.py
```

No Windows, use `python` no lugar de `python3`. Requisitos: Node 22+, Rust/Cargo, Python 3.10+ e as dependências de compilação nativa já usadas pelo app (MSVC/Windows SDK no Windows, Xcode no macOS). O runner configura a compatibilidade CMake 3.5 apenas no processo de teste.

O comando devolve **código 0 somente se todas as etapas solicitadas passarem**. Não esconde falhas com retries automáticos. Cada execução gera uma pasta nova em `e2e-artifacts/verify-<data-hora>/`, com `report.json` e logs por etapa. `--artifact <pasta-nova>` permite escolher o destino. Guarde a pasta inteira para comparar versões.

## O que a suíte verifica

| Camada | Verificação |
|---|---|
| Servidor real local | Salas, senha incorreta, admissão, sinalização, saída, limites e ausência de mídia no servidor |
| Core Rust | Codec/protocolos, congestionamento preservando referências H.264, reconfiguração e encoder compartilhado; integração WebRTC entre peers |
| Shell do app | Criar/entrar/sair, compartilhar/parar, assistir nos dois sentidos, dois viewers, continuidade após saída do primeiro, qualidade, unwatch/rewatch e links órfãos |
| Frontend | Testes Vitest, typecheck e build de produção |
| Browser automatizado | Fluxo de tela criar/sair/entrar usando o mock existente; separadamente, shader real WebGL com I420/NV12/RGBA, orientação, precisão, resize, fallback e recuperação de contexto |
| Plataforma | Testes do contrato comum e do backend do OS atual; gate WGC executa o helper real sem precisar de captura |
| Ferramentas | Analyzer, rejeição de executáveis PE de console, detecção de resultados de cadência incompletos, lentos ou com pausas |
| Windows nativo | Build dos dois executáveis com frontend embutido; inspeção do cabeçalho PE exige subsystem GUI=2 para ambos |

Os testes de browser não comprovam conexão nativa Tauri. Chromium headless pode usar SwiftShader: comprova resultados do shader, não desempenho da GPU física. Os testes Rust cobrem a conexão real por outro caminho. Fixtures interativas marcadas `ignored` e testes dependentes de dispositivos/variáveis não configurados permanecem fora da cobertura; consulte os logs. Não use a contagem global como prova de captura de tela ou reprodução de áudio.

## Teste de desempenho no app de verdade

Com uma sessão gráfica aberta e FFmpeg instalado:

```sh
python3 scripts/verify.py --desktop
```

Além da suíte acima, faz o build release com frontend atualizado, gera uma fonte 1080p60 e abre instâncias locais reais do app, primeiro com um viewer e depois com dois. Mede 30 segundos após aquecimento. Não precisa da sala de produção nem de autorização de captura de tela: usa vídeo de teste local. Fecha os processos que o harness abriu.

O gate exige cobertura de trace, ≥54 FPS em encode/envio/decode/desenho/apresentação, uma instância de encoder, WebGL exercitado, nenhum erro/descarte registrado nas etapas verificadas e gap de apresentação ≤50 ms. Um resultado com média 60 FPS e pausa de 98 ms **reprova**. Carga de outros aplicativos pode afetar essa medição; uma falha deve ser analisada, não removida aumentando o limite.

Para executar apenas uma medição com binário e arquivo H.264 já existentes:

```sh
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/meu-teste --binary app/target/release/goDrinking --movie /caminho/video.h264 --viewers 2
```

No Windows use `goDrinking.exe`. Não reutilize uma pasta de resultados: o comando recusa sobrescrevê-la.

## Terminal indesejado no Windows

O app e o helper agora declaram `windows_subsystem = "windows"`, inclusive no desenvolvimento. O modo console era o padrão anterior e podia criar uma janela ao abrir o programa pelo Explorer; [referência do Rust](https://doc.rust-lang.org/reference/runtime.html#the-windows_subsystem-attribute).

O teste verifica o **arquivo compilado**, não apenas a presença de uma linha no código:

```sh
python scripts/check-windows-gui.py app/target/release/goDrinking.exe app/target/release/golive-video.exe
```

Ele reprova se faltar um arquivo, o PE for inválido ou o subsystem for console=3. Os executáveis Windows antigos disponíveis localmente reprovaram nos dois casos. A correção exige recompilar/substituir o `.exe`; ela não altera os arquivos já distribuídos. A inspeção PE não testa janelas criadas explicitamente por código ou programas externos.

## CI e limites

`.github/workflows/verify.yml` executa a mesma suíte em macOS e Windows em PRs, pushes para main e por acionamento manual. Preserva os relatórios mesmo em falha. A release Windows também recusa executáveis de console antes do upload. Configurar o workflow no repositório não significa que ele já executou: a execução remota depende do push/CI.

Nenhuma suíte garante ausência de todos os bugs. Para aceitar a correção original de fluidez ainda é necessário testar Windows↔macOS nas máquinas reais, com captura de tela, áudio, conteúdo 60 FPS e rede remota. O teste de cadência mede ACK de desenho, não o scanout físico. Não declarar equivalência ao Discord a partir de um PASS local.
