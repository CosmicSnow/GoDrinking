# Diagnóstico da transmissão ao vivo — 2026-09-12

**Nova revisão:** ver [REVISAO-performance-2026-09-12.md](REVISAO-performance-2026-09-12.md). Liberação de H.264 pelo marker aplicada e testada; novo E2E local: 60,011 FPS apresentados, gap máximo 28,215 ms. Pendências estruturais de host/viewer documentadas.

**Estado da correção anterior:** aplicada uma mudança no agendamento do canvas compartilhado por macOS e Windows: desenho/ack sem esperar um requestAnimationFrame extra. Validação local completa em release: 60,006 FPS decodificados e desenhados, zero descartes na janela medida, gap máximo de ack 40,867 ms. Isso elimina a dependência do ack em callbacks de animação, mas não demonstra correção dos picos remotos de decode de 123 ms. Windows nativo e nova sessão real ainda pendentes; BUG-001 permanece aberto.

Conclusão atualizada após o segundo trace Windows (`16648`): o host sustenta ~59,4 FPS de envio em 1080p60 e o viewer ~59,1 FPS de decode, sem descartes no decoder na janela estável de ~30 segundos. Não há teto fixo de 24–30 FPS demonstrado. A baixa taxa do primeiro trace já existia na captura/ponte, mas não ficou provado se vinha do conteúdo ou da aquisição. A nova coleta começa configurada em 30 FPS e muda para 60 após ~68 segundos. Persistem picos medidos no decode/conversão do viewer de até 122,6 ms, suficientes para interromper a cadência mesmo quando a média se aproxima de 60 FPS. A apresentação física no canvas ainda não é medida pelo trace. As análises anteriores abaixo são histórico.

## Evidências da sessão

Conexão real à sala solicitada, assistindo ao host indicado, com o aplicativo instalado `/Applications/goDrinking.app`, versão exibida 0.7.5. Executável datado de 12/09 às 01:07; não foi recompilado nem substituído. A revisão exata desse binário não foi estabelecida; o checkout estava em `755d0b3` e já possuía alterações locais. Nenhuma configuração de qualidade do host foi alterada.

Trace ativado por `GOLIVE_TRACE_DIR`, contendo somente agregados numéricos. Snapshot congelado em `e2e-artifacts/live-20260912-viewer/snapshot.jsonl`; resumo em `summary.json` na mesma pasta. Excluindo os primeiros 15 segundos:

| Medida | Resultado |
|---|---:|
| Janela de decode | 160,344 s |
| FPS decodificados médios | 33,098 |
| Mediana das taxas por registro | 29,925 FPS |
| Resolução observada | 1920×1080 |
| Unidades descartadas pelo decoder | 0 |
| PLI enviados / suprimidos | 0 / 0 |
| Trabalho médio decode + conversão RGBA | 5,047 ms |
| Maior trabalho de decode + conversão RGBA | 135,529 ms |
| Registros com ao menos um trabalho acima de 16,7 ms | 21 de 157 |
| Payload RTP recebido médio | 1,412 Mbps |

No início houve 40 unidades sem imagem e 5 PLI enviados, com 35 suprimidos. Esses eventos ficaram concentrados na entrada; não devem ser interpretados como perda contínua. Ausência de descarte/PLI não prova ausência de jitter ou de perda de pacotes: o trace RTP atual não mede lacunas de sequência.

Um snapshot da UI mostrou 5250 decodificados e 5196 apresentados (54 de diferença, aproximadamente 1,03%; pode incluir frame em voo e trocas de janela). Os contadores representam desenho/ack do canvas, não apresentação física no monitor. O descarte final da interface não explica, por si só, a diferença entre 60 FPS desejados e cerca de 30 observados. Ainda é possível atraso síncrono do callback de apresentação reduzir a própria leitura de RTP.

A taxa configurada de 6000 kbps não é garantia de 6 Mbps constantes. Os 1,412 Mbps de payload medidos não estabelecem saturação da rede, nem identificam o encoder do host. O bitrate mostrado pela UI mede RGBA pós-decode, não H.264 da rede.

## Reprodução executada

`python3 e2e-artifacts/live-20260912-viewer/check_fps.py`

O script lê os dez registros mais recentes do decoder e reprova abaixo de 54 FPS (90% do alvo informado de 60). Resultados executados: 28,190; 32,168; 26,945 FPS — todos FAIL, todos com zero descartes e zero PLI nas respectivas janelas. É um gate de throughput observável, não um teste determinístico da causa: conteúdo/cadência do host e condições de rede podem mudar. Captura de conteúdo estático ou vídeo original de menor FPS exige interpretação diferente.

Também executado `python3 scripts/analyze-trace.py` sobre o trace do processo; self-test do analisador: 14 checks passaram. A amostra de processo de 8 segundos identificou OpenH264 e conversão YUV→RGBA no viewer real. A leitura pontual de CPU do processo principal foi 26,1%; não inclui o total dos subprocessos WebKit nem mede GPU. Não estabelece saturação global.

## O que foi isolado e o que falta

1. O decoder local tem média inferior ao orçamento de 16,7 ms, mas apresenta picos de até 135,5 ms: evidência de trabalho local irregular, capaz de contribuir para engasgos. Não prova que isso explique toda a baixa taxa sustentada.
2. O pop-up permaneceu perto de 32 FPS. A leitura do código e a UI confirmaram que ele usa o mesmo player WebView, portanto não é um controle independente com o helper GLV1.
3. No código atual, `core/src/media.rs` chama `on_frame` sincronamente após encerrar a medição do decoder. `app/src/pump.rs` encaminha ao `present_inline` no app desktop. `app/src/player.rs` envia RGBA pelo Tauri Channel, mantém o último frame e um pacote em voo, e aguarda ack. `app/web/src/StreamPlayer.tsx` desenha com requestAnimationFrame/putImageData antes do ack. A 1080p, cada frame tem 8.294.400 bytes. Esse caminho não emite o trace `present` existente no helper; não é possível medir seu atraso completo com os traces atuais.
4. Falta medir o host: `capture → source → encode → send`, backend realmente selecionado e tempos por estágio. Se capture já estiver perto de 30 FPS, investigar captura/cadência/conversão; se capture estiver em 60 e encode em 30, investigar encoder; se send sustentar 60 mas viewer ficar em 30, investigar transporte e callback do viewer com métricas adicionais.

## Coletar trace no Windows 10

O usuário informou que não existe pasta `media-trace`. Sua ausência é esperada quando tracing não foi ativado; não prova inexistência da instrumentação na build.

O arquivo `e2e-artifacts/live-20260912-viewer/diagnosticar-windows.cmd` deve ser colocado ao lado do executável do host. Fechar o app antigo normalmente e abrir pelo CMD fornecido. O iniciador define `GOLIVE_TRACE_DIR` apenas para o processo iniciado, sem alterar configurações permanentes; procura goDrinking.exe ou golive-app.exe na mesma pasta. Ele não foi executado em Windows neste diagnóstico.

Entrar na sala e compartilhar conteúdo com movimento durante 30–60 segundos com as mesmas configurações. Enviar os arquivos `media-trace/golive-trace-*.jsonl` para continuar a análise. Se a pasta continuar ausente mesmo compartilhando, confirmar o caminho/build do executável; a build pode não conter a instrumentação. Não concluir isso somente pela ausência atual da pasta. Para voltar ao lançamento sem trace, fechar e abrir o app normalmente.

Nenhuma correção de produto foi aplicada; BUGS.md permanece aberto e intacto. Os arquivos de código previamente modificados foram preservados. Viewer retornado à sala e deixado conectado conforme o pedido; seu trace continua ativo enquanto esse processo estiver aberto.


## Atualização — trace do host Windows 10 recebido

Arquivo: `e2e-artifacts/live-20260912-viewer/golive-trace-6728.jsonl`, 941 registros. Janela de timestamps do host: 2026-09-12 05:56:34,786–06:00:34,769 UTC. O viewer continuou coletando e possui dados coincidentes. Comparação por timestamps de máquina, sem sincronização independente dos relógios; serve para taxas sustentadas, não para latência ponta a ponta.

O trace começa em 1280×720@30, durante cerca de 20 segundos, e muda para 1920×1080@60. A média global de 24 FPS mistura configurações; não foi usada isoladamente para o diagnóstico. Na janela de 40 a 200 segundos após o primeiro registro, o perfil já está em 1080p60:

| Etapa | Taxa | Trabalho médio | Maior trabalho |
|---|---:|---:|---:|
| Captura/ponte Windows | 23,88 FPS | 8,605 ms | 53,116 ms |
| Entrada fresca no encoder | 23,88 FPS | 30,015 ms de espera | 94,172 ms |
| Encode | 23,88 FPS | 11,406 ms | 45,490 ms |
| Envio | 23,88 FPS | 0,409 ms | 18,447 ms |
| Decode no viewer | 23,88 FPS | 6,335 ms | 78,732 ms |

A ponte teve 1 descarte em todo esse trecho e nenhum timeout. O decoder teve zero descartes e zero PLI. Bitrate H.264 enviado: ~1,948 Mbps; payload RTP recebido: ~1,952 Mbps (janelas de agregação ligeiramente diferentes). O encode acompanhou os quadros frescos disponíveis e passou bastante tempo esperando entrada. Isso localiza a baixa cadência antes do encoder; não demonstra que o encoder sustentaria qualquer conteúdo a 60 FPS.

O final não possui teto fixo de 24 FPS: os últimos registros de captura sobem por aproximadamente 34,9 → 46,5 → 59,2 FPS, com registro parcial final de 53,5. Essa passagem é curta e não valida 60 FPS sustentados.

### Cadência do conteúdo versus captura lenta

A hipótese de conteúdo a ~24 FPS precisa ser testada antes de classificar toda a baixa taxa como perda. Na sessão anterior, o host mostrava um vídeo no YouTube; o FPS desse vídeo e a continuidade desse conteúdo na nova coleta ainda não foram confirmados pelo usuário.

O backend de display chama `AcquireNextFrame(100)` (`platform-windows/src/dxgi.rs:188`), processa um frame disponível e aplica um teto de cadência; não fabrica 60 imagens novas por segundo. A API entrega quadros quando a imagem do desktop ou o cursor muda, conforme a documentação oficial: [Microsoft — AcquireNextFrame](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe). Logo, uma tela que muda a 24 Hz pode produzir cerca de 24 FPS mesmo configurada para até 60. O trace não identifica sozinho se a fonte escolhida era display/DXGI ou janela/WGC.

Limite da instrumentação: `capture.work_us` começa após `stream.next_frame()` retornar em `app/src/screen.rs`; não inclui aquisição/readback do backend. Os descartes feitos no backend antes da ponte também não aparecem nesse contador. Portanto, a igualdade das taxas elimina a hipótese de grande descarte depois da ponte nesse trecho, mas não elimina custo ou descarte antes dela.

Há engasgos reais de trabalho local acima do orçamento de 16,7 ms em ambas as máquinas. O estágio inicial em 720p30 também teve conversão/ponte média de 101 ms, máximo 253 ms e captura de 9,72 FPS, enquanto o encode levava 6,4 ms em média. É um episódio separado do trecho 1080p60; não atribuir automaticamente à mesma causa.

### Próxima validação que distingue as causas

Compartilhar conteúdo com 60 FPS conhecidos, mantendo 1080p/6000 kbps/60 FPS, e coletar simultaneamente por 30–60 segundos. Confirmar a taxa real do conteúdo na fonte. Se a captura continuar em ~24, instrumentar aquisição, readback e descartes do backend Windows; se capture/send chegarem a 60 mas decode não, investigar recepção/callback; se decode chegar a 60 mas apresentação não, medir Channel/canvas/ack. Não usar apenas uma imagem parada ou vídeo de 24/30 FPS como prova de falha no alvo 60.

Resumo numérico reproduzível salvo em `e2e-artifacts/live-20260912-viewer/host-viewer-summary.json`. Nenhuma correção de produto aplicada; não há evidência suficiente para escolher um patch específico ainda.


## Validação controlada local — 1080p60 / 6000 kbps

Após o pedido para continuar, executei o harness real de dois peers com perfil explícito 1920×1080, 6000 kbps e 60 FPS, engine Auto, servidor local e fonte FFmpeg `testsrc2` de 60 FPS. O harness temporário exige pelo menos 270 quadros novos em cinco segundos (54 FPS). Resultado: **300 quadros novos em cinco segundos, PASS**. O teste completo terminou em 9,26 segundos. Os traces confirmaram dimensões 1920×1080 e alvo de encode 60 FPS.

Comando executado a partir de `app/`:

```sh
GOLIVE_MOVIE="$PWD/../e2e-artifacts/live-20260912-viewer/motion-1080p60.h264" GOLIVE_TRACE_DIR="$PWD/../e2e-artifacts/live-20260912-viewer/local-1080p60" cargo test --release --manifest-path ../core/Cargo.toml --test diagnostic_1080p60 movie_file -- --nocapture
```

O protótipo foi retirado de `core/tests/` após o teste e preservado como `e2e-artifacts/live-20260912-viewer/diagnostic_1080p60.rs`. Para reproduzir o comando, copiar esse arquivo de volta para `core/tests/` temporariamente. Ele deriva de `e2e_two_peer.rs`, substituindo o perfil inicial por 1080p60/6000 e o limiar de throughput por 270; executar somente `movie_file`, como no comando. Os testes originais permaneceram intactos.

Médias do trace de todo o teste (incluem inicialização): encode 60,0 FPS, decode 59,6 FPS; encode médio 5,906 ms, máximo 13,383 ms; decode médio 5,705 ms, máximo 22,872 ms. Dois descartes e um PLI de entrada. A taxa de 300/5 s é a janela sustentada após estabelecer o fluxo.

Limites: teste local macOS, build release atual, H.264 via WebRTC real; não exerce captura/readback Windows, rede remota, áudio, nem Channel/canvas do player. Assim, demonstra capacidade do core para essa fonte controlada, sem absolver todo o viewer de qualquer engasgo.

Foi criado também `e2e-artifacts/live-20260912-viewer/teste-1080p60.mp4` para repetir no host: 10 segundos, 1920×1080, exatamente 600 quadros, `r_frame_rate=60/1` e `avg_frame_rate=60/1`, verificados com ffprobe. Compartilhar em reprodução contínua com o mesmo perfil e trace permite distinguir conteúdo original de menor FPS de uma limitação no Windows. Confirmar também que o reprodutor de origem efetivamente acompanha 60 FPS.


## Segunda coleta Windows — `golive-trace-16648.jsonl`

518 registros, de 06:15:21,391 a 06:17:31,971 UTC (130,58 segundos entre os timestamps extremos). Resolução constante 1920×1080; alvo inicial 30 FPS, alterado para 60 FPS nos registros de encode a +67,867 s e capture a +68,580 s. Esses são instantes de emissão dos agregados, não o instante exato do clique. Por isso, os 40,7 FPS da média global misturam dois perfis e períodos de interrupção, e não descrevem desempenho sustentado em 60 FPS.

O viewer original continuou coletando. Seu trecho coincidente foi congelado em `e2e-artifacts/live-20260912-viewer/viewer-overlap-16648.jsonl`. Na janela relativa de 90 a 120 segundos do host:

| Etapa | Taxa | Trabalho médio | Maior trabalho | Descartes |
|---|---:|---:|---:|---:|
| Captura/ponte Windows | 59,320 FPS | 6,55 ms aproximadamente | 16,187 ms | 0 |
| Entrada fresca no encoder | 59,342 FPS | espera por entrada | 101,976 ms | 0 |
| Encode Windows | 59,409 FPS | ~10,44 ms | 16,553 ms | 0 |
| Envio Windows | 59,413 FPS | ~0,42 ms | 2,244 ms | 0 |
| Decode no viewer macOS | 59,118 FPS | 6,160 ms | 122,593 ms | 0 |

Resumo exato por estágio em `summary-16648.json`. A janela soma aproximadamente 29–30 segundos por estágio devido aos limites dos registros. Bitrate enviado ~5,784 Mbps, payload RTP recebido ~5,790 Mbps. Nenhum PLI no decoder nesse trecho. Um timeout na ponte e dois na entrada do encoder mostram pequenas interrupções, sem queda sustentada para 24/30 FPS. Comparações temporais usam relógios das máquinas não sincronizados independentemente; não calculam latência ponta a ponta.

### Engasgos residuais no viewer

No trecho estável, o decoder registrou picos de 122,593; 116,861; 104,094 e 104,056 ms. Por exemplo, a +107,384 s o agregado do decoder contém 47 quadros e um pico de 104 ms; o seguinte, a +108,399 s, contém 74 quadros e um pico de 123 ms. Nos agregados próximos do host, capture/encode/send continuavam perto de 60 FPS. Isso é compatível com entrega irregular e recuperação posterior da quantidade de quadros: média boa não garante cadência suave.

O intervalo medido termina antes de `on_frame`, portanto esses picos são de decode + conversão RGBA, incluindo qualquer pausa de agendamento ocorrida nesse intervalo; não são tempo de canvas nem prova isolada de CPU saturada no codec. A amostra anterior já identificou OpenH264 e conversão RGBA no viewer. Ainda não há medida de ack/intervalos do canvas nessa coleta.

Conclusão delimitada: o teste retira a hipótese de um teto sustentado inevitável de 24/30 FPS do host nessa condição. O primeiro trace pode refletir conteúdo com menor cadência; isso permanece hipótese, não confirmação. Existe evidência direta de pausas locais no caminho de decode do viewer que podem explicar parte da sensação de engasgo. A apresentação precisa de instrumentação própria para quantificar o efeito visual completo. Sem essa medição, não escolher um patch no encoder do Windows baseado apenas na média antiga.

Nenhuma mudança de produto aplicada. Ao consultar a UI após a coleta, o host já tinha saído e não havia transmissão ativa, portanto os contadores de apresentação daquela sessão não estavam mais disponíveis.


## Correção do agendamento do player e validação final

`app/web/src/StreamPlayer.tsx` agora usa `playerFrameDelivery.ts`: ao receber um pacote binário, valida o frame, desenha no canvas e envia o ack em `finally`, inclusive com `drawn=false` se o desenho falhar. Antes, um requestAnimationFrame intermediário mantinha o único frame em voo retido até o navegador executar o callback. A espera extra saiu; o compositor continua controlando a atualização física da tela. A política latest-only do backend foi preservada e não foi adicionada fila de quadros antigos.

O trace opt-in do `app/src/player.rs` agora cobre o player WebView com `Stage::Present`, medindo intervalo de desenho/ack e duração do envio ao ack. `dropped` mede substituições antes do envio e acks de desenho falho, não toda perda possível; um timeout de flight ou um detach não provam que o quadro não chegou à tela. Também foi adicionado um perfil opcional ao plano E2E e o harness `scripts/check-viewer-cadence.py` para exercitar o Channel/canvas real em 1080p60.

Os testes determinísticos da nova função de entrega falharam com o agendamento anterior (zero desenhos/acks enquanto RAF permanecia pendente), e passaram com entrega imediata. Eles cobrem o contrato de agendamento/ack, não o decoder ou a rede. A integração real foi medida separadamente.

As primeiras execuções do harness ficaram sem acks de desenho e expiraram. Durante o diagnóstico, uma execução ainda com RAF e probes temporários de estado conseguiu 59,971 FPS de desenho, gap máximo de 47,327 ms. Como havia probes de UI nessa execução, ela não é uma comparação A/B rigorosa de desempenho e não prova que todo atraso anterior vinha de RAF.

Todos os probes `[DEBUG-viewer]` foram removidos antes da build final. Execução final:

```sh
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/cadence-after --viewer-binary e2e-artifacts/cadence-player.app/Contents/MacOS/goDrinking --seconds 30
```

Resultado: PASS. Janela de trace útil de apresentação 29,231 s (o harness exclui agregados que atravessam o início da medição):

| Etapa | FPS | Descartes | Maior trabalho/gap |
|---|---:|---:|---:|
| Encode | 60,007 | 0 | 8,169 ms de trabalho |
| Envio | 60,002 | 0 | 8,842 ms de trabalho |
| Decode | 60,006 | 0 | 18,344 ms de trabalho |
| Canvas/ack | 60,006 | 0 | 32,552 ms de ida/volta; 40,867 ms de gap |

Veredicto completo: `e2e-artifacts/cadence-after/verdict.json`. O teste exige pelo menos 54 FPS em decode e canvas e no máximo 50 ms de gap; é sensível à carga/scheduling da máquina. Acks medem conclusão do desenho, não scan-out físico.

Validação adicional: web 96 testes passaram, incluindo os dois novos; app `cargo test --release --lib` 106 passaram; build web e build release desktop passaram; analisador 14 checks passaram; `git diff --check` passou. Não houve alteração de protocolos de sala/GLV1, de captura/encoder ou do mock de navegador.

A alteração ainda precisa rodar numa nova build Windows e ser comparada na sessão real nas duas direções. Não foi implementada uma troca de decoder, decodificação em thread dedicada ou conversão por GPU nesta etapa: os traces disponíveis não isolam qual dessas mudanças resolveria os picos locais de 123 ms. Eles continuam sendo uma investigação distinta do atraso extra de agendamento removido.
