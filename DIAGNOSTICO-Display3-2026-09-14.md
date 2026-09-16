# Cadência do Display 3 — diagnóstico ainda aberto

Cenário informado: YouTube 60 FPS, monitor de 144 Hz configurado em 120 Hz.
A localização do viewer em relação ao display capturado ainda precisa ser
confirmada para excluir a própria janela entrando na transmissão.

## Medições ao vivo

| Janela após aquecimento | Host captura | Viewer apresentação | Maior intervalo de apresentação |
|---|---:|---:|---:|
| Baseline observado | ~50,3 FPS | ~50,1 FPS | 122,343 ms |
| Decoder em thread dedicada | ~39,7 FPS | ~39,7 FPS | 100,172 ms |

Artefatos: `e2e-artifacts/live-ppyvnk-baseline/` e
`e2e-artifacts/live-ppyvnk-worker/`. Os trechos têm entradas/cargas distintas;
esses números **não são uma comparação A/B controlada**. O segundo resultado
continua incompatível com uma transmissão estável de 60 FPS. ACK de desenho
não mede scanout físico.

O profiler identificou OpenH264 dentro de um worker Tokio. A alteração move
criação, decodificação e destruição do codec para uma thread serial dedicada,
com uma requisição em voo. Testes verificam progresso do runtime enquanto o
codec bloqueia, ordem dos trabalhos, afinidade e destruição. O recebimento RTP
ainda aguarda o resultado do decoder; não foi implementado jitter buffer.
A mudança não elimina todos os picos observados.

O trace antigo mostra zero descartes na ponte durante os trechos destacados,
mas não observa os callbacks antes do limitador SCK. A nova etapa
`capture_input` conta callbacks, descartes do gate, fila cheia, amostras
inválidas e maior intervalo. São contadores de aquisição, não prova de que
cada callback contém uma imagem nova do vídeo.

## Limite encontrado na captura isolada

A fixture de captura recebeu PermissionDenied do macOS. Uma tentativa sem
janela no executável principal expirou na inicialização; essa CLI foi removida.
Nenhuma das tentativas gerou uma medição válida de FPS. Logs:
`e2e-artifacts/live-display3-acquisition.log` e
`e2e-artifacts/display3-cli-probe.log`.

O host que estava aberto não foi interrompido. Ele continua com o código antigo
em memória e precisa ser reiniciado com a build atual para fornecer os novos
contadores. O próximo ensaio deve manter o viewer fora do Display 3 e capturar
host + viewer simultaneamente. Não remover BUG-001 até repetir o cenário e
aprovar a cadência.

## Verificação reutilizável

Execute `python3 scripts/verify.py` na raiz. Para incluir vídeo sintético no
app real e gates de FPS/maior pausa: `python3 scripts/verify.py --desktop`.
Veja [TESTING.md](TESTING.md) para requisitos e limites. Aprovação funcional
não comprova fluidez do Display 3 nem execução nativa Windows.

Analyzer atualizado: 16 verificações passaram em
`e2e-artifacts/acquisition-analyzer-self-test.log`. O novo teste primeiro
reprovou porque os contadores eram agregados mas não apareciam no relatório;
a saída foi corrigida e o mesmo teste passou.

Nova execução funcional: PASS em todas as 10 etapas de
`e2e-artifacts/verify-20260914-160127-773370/report.json`, incluindo 94 testes
unitários do core, 106 do app e quatro smoke tests. A fixture de captura
interativa permanece ignorada. Esta execução não incluiu `--desktop`;
a reprodução ao vivo continua reprovando a cadência.


## Host reiniciado na build atual — duas novas medições

Confirmado o processo `goDrinking` atualizado e a presença de `capture_input`.
As duas janelas excluem aproximadamente dez segundos de aquecimento e incluem
cerca de 54 segundos de trace. A janela é filtrada pelo término de cada registro;
como os estágios fecham seus registros em instantes diferentes, as contagens
não devem ser subtraídas como se fossem quadros perfeitamente alinhados.

| Métrica | Coleta 1 | Repetição |
|---|---:|---:|
| Captura encaminhada | 56,895 FPS | 56,611 FPS |
| Apresentação no viewer | 56,875 FPS | 56,594 FPS |
| Maior intervalo de apresentação | 297,715 ms | 63,254 ms |
| Maior tempo decorrido no codec | 180,029 ms | 63,079 ms |
| Conversão máxima | 4,238 ms | 3,516 ms |
| Desenho máximo | 3 ms | 2 ms |
| Descartes pelo gate SCK | 7 | 1 |
| Descartes por fila SCK cheia | 0 | 0 |
| Callbacks sem imagem utilizável | 30 | 52 |

Ambas as coletas reprovam `verdict_failures` do gate reutilizável por intervalo
de apresentação acima de 50 ms. Evidências em `e2e-artifacts/live-ctvd2c/` e
`e2e-artifacts/live-ctvd2c-repeat/`, incluindo `summary.json`,
`cadence-verdict.json`, trace do viewer e recorte do host. Os viewers de teste
foram encerrados; o host foi preservado.

O usuário informou 286 dropped of 69800 no YouTube (~0,41% acumulado).
Não há contagem inicial/final correspondente ao ensaio, nem confirmação da
resolução atual e posição do viewer. Não atribuir as pausas ao YouTube com
base somente nesse acumulado.

O pico de 298 ms coincidiu aproximadamente com picos no encode/envio do host.
O tempo de codec é wall-clock: inclui desagendamento. A repetição confirma
picos menores, mas não distingue computação do codec de disputa pelo sistema.
O caminho Mac continua em OpenH264 por software; a thread dedicada não o torna
um decoder de hardware. Não foi feita mudança adicional no pipeline durante
estas duas medições.

Possibilidades para a próxima comparação controlada, ainda não correções
comprovadas: decoder VideoToolbox no Mac, medição de CPU da thread versus
wall-clock, e pool de superfícies SCK. O código não configura queueDepth,
portanto depende do padrão do sistema. A Apple documenta padrão 3 e exemplo
60 FPS com 5 superfícies; superfícies retidas podem interromper a aquisição.
Ver [apresentação da Apple](https://developer.apple.com/videos/play/wwdc2022/10155/?time=937).
Callbacks sem imagem não implicam corrupção: SCK pode entregar status idle
quando não há mudança na tela, conforme [SCFrameStatus](https://developer.apple.com/documentation/screencapturekit/scframestatus).
O trace atual ainda não separa esses status. BUG-001 permanece aberto.
