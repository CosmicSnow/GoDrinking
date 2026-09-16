# Viewer macOS: VideoToolbox e validação

Implementado decoder H.264 por hardware no backend macOS. O shell instala
uma fábrica usando o contrato puro `platform::decode::VideoDecoder`; o core
não importa bindings novos da Apple. A thread dedicada cria/usa/destrói o
backend serialmente. A sessão exige hardware e o callback síncrono copia NV12
com respeito a stride e dimensões. A WebView recebe o formato já suportado.
Ainda existe cópia CPU/IPC; não é apresentação nativa sem cópias.

Falha de driver ou saída inválida troca para software. A recuperação conserva
SPS/PPS validados e espera IDR, evitando alimentar um decoder novo com deltas
que dependem de referências da sessão anterior. A ordem dos quadros permanece
serial. Windows mantém a seleção Media Foundation existente.

## Testes

- 99 testes unitários do core passaram (`vt-core-final-tests.log`).
- 18 testes do backend macOS passaram (`vt-status-platform-tests.log`), incluindo
  VideoToolbox real, SPS/PPS separados, deltas, troca de resolução até 1080p e
  comparação de luminância/crominância com OpenH264.
- 106 testes unitários do app, quatro smoke tests e contratos auxiliares passaram
  (`vt-app-tests.log`); uma fixture interativa permanece ignorada.
- Contadores de status, quatro testes de trace e 17 verificações do analyzer
  passaram após a instrumentação adicional. Build release com frontend atualizado.
- O teste de saída nativa inválida reprovou antes da correção de fallback e passou
  depois (`vt-invalid-output-red.log`, `vt-core-final-tests.log`).

Os logs ficam em `e2e-artifacts/`. A suíte reutilizável está em [TESTING.md](TESTING.md).
Não foi executada validação nativa Windows nesta rodada.

## Medição na sala, antes de confirmar a fonte

Com o mesmo host, duas janelas consecutivas de aproximadamente 54 segundos:

| Métrica | VideoToolbox | OpenH264 |
|---|---:|---:|
| Captura encaminhada | 41,103 FPS | 41,098 FPS |
| Apresentação | 41,343 FPS | 41,035 FPS |
| Maior tempo no codec | 5,867 ms | 135,739 ms |
| Maior intervalo de apresentação | 69,639 ms | 143,327 ms |
| Descartes na apresentação | 0 | 2 |

Artefatos: `live-swt01a-hardware/` e `live-swt01a-software/`. Sem compilação
concorrente, VideoToolbox confirmado e sem fallback. O usuário depois confirmou
que o YouTube estava em `1920x1080@30`. Portanto, o gate mínimo de 54 FPS **não é
apropriado para aceitar/rejeitar 60 FPS nessa fonte**. A configuração da transmissão
não cria movimento de 60 FPS a partir de vídeo de 30 FPS. Quadros de captura
também não equivalem necessariamente a imagens únicas do vídeo.

Os picos medidos de decoder/presentação continuam sendo observações reais, mas
as janelas não contêm quadros idênticos e não permitem prometer um ganho percentual
universal. Foi iniciado novo ensaio após o usuário mudar a fonte para 60 FPS.


## Fonte alterada para 60 FPS e gate completo

Nas janelas seguintes, com fonte informada como 60 FPS, o viewer por hardware
apresentou 55,52 FPS, gap máximo de 93,74 ms e três descartes; o software,
56,18 FPS e gap de 97,62 ms. Ambos reprovaram. Artefatos:
`live-swt01a-60-hardware/` e `live-swt01a-60-software/`.

Um ensaio adicional em software (`live-swt01a-cpu-software/`) mediu 55,83 FPS
e gap de 77,75 ms. A instrumentação separou tempo de parede do tempo de CPU da
thread: houve janelas com picos de parede de 26,93 ms e CPU máxima de 6,66 ms,
e outras com ambos próximos de 22 ms. São máximos por janela, não pares do
mesmo quadro. Há trabalho de CPU e também espera/agendamento; isso não prova
App Nap nem identifica sozinho qual espera ocorreu.

`python3 scripts/verify.py --desktop` concluiu com **FAIL** em
`verify-20260915-001512-318294/report.json`: todas as suítes funcionais passaram,
mas os gates de cadência de um e dois viewers falharam. Com um viewer, o gap
foi 75,83 ms; com dois, 65,32/65,05 ms. O decode por hardware ficou abaixo de
6,1 ms; o encode do host chegou a 53,10/56,46 ms. Os máximos agregados sugerem
investigar o emissor, mas não provam causalidade entre quadros individuais.
O limite de 50 ms foi preservado. O bug continua aberto.


## CPU do emissor e experimento de velocidade

Instrumentação de CPU estendida ao encode. Em `encode-cpu-cadence/`, dois
viewers apresentaram 59,36 FPS, mas gaps de 116,50/113,43 ms. O encode chegou a
108,74 ms; na mesma janela de aproximadamente um segundo, a CPU máxima da
thread foi 3,29 ms. O relógio não mede GPU nem outras threads do driver. Isso
localiza espera fora do trabalho de CPU da thread, sem distinguir ainda driver,
agendamento ou contenção do sistema. Os viewers tiveram pausas na janela
sobreposta; o trace agregado não identifica o quadro individual.

Experimento isolado com `PrioritizeEncodingSpeedOverQuality=true`
(`encode-speed-cadence/`): encode máximo 44,97 ms, média 7,09 ms; gaps de
82,07/79,61 ms, gate novamente FAIL. Uma execução por configuração não prova
melhoria estável e a opção permite sacrificar qualidade. A alteração foi
**revertida**; manteve-se apenas a instrumentação. A opção mais invasiva
`EnableLowLatencyRateControl` também não foi ativada: impõe GOP infinito e
perfil High, exigindo validar recuperação por IDR e compatibilidade primeiro.

Próximo isolamento: medir separadamente submissão ao VT, callback de conclusão
e retomada do worker. O host já aberto não recebe instrumentação nova apenas
porque o executável no disco foi recompilado.

## Sala ao vivo SWT01A com fonte 60 FPS (live-swt01a-final-hardware/)

Viewer novo (VideoToolbox confirmado, sem fallback) contra o host ao vivo
compartilhando o Display 3. Janela ~54s: host encode 57,3 FPS (pico 45ms),
viewer decode 57,3 FPS, present 57,2 FPS com 6 descartes — e mesmo assim FAIL:
gap de apresentação de 249ms, draw máximo 1ms (WebGL ativo, 3064 frames GPU).

Separação CPU vs parede no viewer (trace com relógio de CPU da thread):
decode pico de parede 194ms contra CPU máxima 21,7ms por quadro (30ms em 47
amostras). Ou seja, a maior parte do pico é espera/agendamento, mas há quadros
que sozinhos estouram o orçamento de 16,7ms em CPU. No host (binário anterior,
sem relógio de CPU): source 113ms, send 43ms, capture_input gap até 119ms.

Leitura: a travada que você descreve ("um quadro fica por mais tempo") aparece
nos dois lados. O host tem gaps de captura/envio e o viewer, sem jitter
buffer, congela o último quadro a cada gap ou desescalonamento — o pipeline é
serial (recebe → decodifica → apresenta), então qualquer buraco vira quadro
parado na tela. Os testes sintéticos na mesma máquina estão poluídos (load
15: Steam ~95% CPU + vários helpers do Vivaldi), por isso a sala real é o
julgamento válido. BUG-001 segue aberto.

## Pacer de apresentação + host sem rajada (build atual)

Implementado `PresentPacer` no viewer (capacidade 2, ritmo por deltas RTP,
low-watermark: rede boa ≈ 0ms extra; buffer cheio ≤ 2 intervalos ≈ 33ms @60fps;
estouro conta decode.dropped + PLI com espera por IDR). Host verificado sem
rajada (encode_loop já reancora, FrameSlot single-slot) — sem mudança no emissor.
Testes: 105 core passed (4 novos do pacer, determinísticos).

Cadence sintético 1 viewer pós-pacer (`pacer-cadence-1v/`): host encode 59,9 FPS
(prepare 35ms, pool 1,9ms), viewer decode 59,9 FPS (17,9ms), present 59,8 FPS
gap 59,3ms. Ainda FAIL no gate de 50ms, mas gap caiu de 150–255ms para ~59ms
nesse ensaio. Máquina com load alto contamina sintético; julgamento válido é a
sala ao vivo. BUG-001 segue aberto até reteste real.

## Suíte funcional pós-pacer

`python3 scripts/verify.py` (sem --desktop): PASS em todas as etapas
(web-unit, web-build, web-browser, core, platform, platform-macos, app) —
`e2e-artifacts/verify-20260915-043658-434508/report.json`. Sem regressão
funcional. Falta apenas o reteste ao vivo na SWT01A (depende da sala aberta).

## Correção da cascata PLI/IDR + reteste ao vivo UWOAHY

Achado: overflow do pacer (pós-decode, inofensivo por si) armava
recover_decoder + PLI → RecoveringSoftware aguardando IDR → deltas
descartados → novo PLI → ~7s de freeze com 281 drops e gap de 994ms
(`live-uwoahy-pacer-hardware/`). Política corrigida: overflow pós-decode é só
drop+count em decode.dropped, sem PLI/IDR; PLI fica só para perda pré-decode
(AU gap, erro de depacketize/decode). Teste novo
`pacer_overflow_drop_leaves_decoder_chain_intact` passa; 106 core passed.

Reteste ao vivo UWOAHY (`live-uwoahy-noidr-hardware/`, mesma fonte Display 3):
decode 57,5 FPS com 1 drop e 0 PLI (antes 52,8 FPS, 289 drops, 9 PLIs);
codec máx 13,5ms; present 57,4 FPS, 2 drops, maior gap 80ms (antes 994ms).
Pior decode 50,5ms de parede contra 274µs de CPU máx por quadro → resíduo é
agendamento sob carga da máquina, não trabalho do codec. Host: encode 57,5 FPS
(pico 63ms, resume 57ms = mesma assinatura de agendamento), capture gap 63ms.
Gate de 50ms ainda reprova por picos isolados, mas a cascata está eliminada.

Pendente: build Windows com estas mudanças para a espectadora no Windows 10
sentir o efeito (cross-compile bloqueado na dependência Opus; ver validação).
BUG-001 segue aberto até confirmação visual nas duas pontas.

## Reteste D5LC1C macOS host+viewer (`live-d5lc1c-hardware/`)

Sala real, build com pacer sem-PLI-em-overflow. Janela ~54s: host encode 57,6
FPS (máx 19ms; submit 4ms, completion 10,5ms, resume 12ms; send máx 17ms;
capture 57,6 FPS com gaps de aquisição 19–37ms). Viewer VT sem fallback:
decode 57,6 FPS, codec máx 14ms, convert 9,4ms, dispatch 11ms, draw 2ms
(WebGL, 3078 frames GPU), present 57,6 FPS com 0 drops.
12 dos 13 drops de decode ficaram no warmup (entrada/troca de perfil); no
regime: 1 drop, 0 PLI em 54s. Cascata eliminada também aqui.
Restam 3 gaps isolados de apresentação (58/75/81ms) sem causa upstream nas
janelas adjacentes (decode/codec vizinhos com 5–14ms e contagens normais;
host calmo a ±1,2s) — assinatura de agendamento do processo de diagnóstico
(headless, background, sujeito a throttle de timers do SO), não de custo do
pipeline. O gate automático de 50ms reprova; o julgamento visual em janela
real visível passa a ser o critério válido.

## Relógio suavizado (EWMA+slew) + primeiro PASS ao vivo

`live-d5lc1c-ewma-hardware/`: **PASS no gate** (falhas: nenhuma) — present
57,9 FPS, gap máximo 45,2ms, 0 drops em todos os estágios; decode 57,9 FPS
(codec máx 10ms), draw 3ms WebGL, pacer_hold médio 0,23ms. Histograma:
gaps >20ms 200→112, >25ms 61→34, >34ms 28→8, >50ms 16→0.
Leitura honesta: o host também estava mais calmo neste ensaio (send 96→26ms,
capture gap 109→37ms), então a queda mistura relógio melhor + fonte melhor;
não dá para atribuir percentual a cada um com uma execução por configuração.
111 testes core passed. BUG-001 segue aberto até confirmação visual.


## Revisão de baixa latência após auditoria (15/09)

`core/src/media.rs`: deadlines atendidos durante leitura RTP e espera pelo
worker serial; o mesmo future é preservado entre deadlines. Primeiro quadro
imediato e capacidade 2 mantidos. Ao retomar tarde, só o quadro vencido mais
recente é liberado; descartes pós-decode continuam sem PLI. Deltas isolados
maiores que 2 intervalos + 1ms não alteram a EWMA; três amostras semelhantes
confirmam uma mudança sustentada. A tolerância evita classificar 33333µs
como pausa quando a estimativa era 16666µs.

A decisão `deliver_decoded` é compartilhada pelo loop e pelos testes: erro
software após o primeiro quadro pede PLI; overflow não pede. Fallback Windows
agora usa RecoveringSoftware e espera IDR com os headers enviados pelo host;
a execução nativa Windows continua pendente. Não foi adicionada detecção de
perda/reordenação por sequência RTP nesta revisão.

Validação em `e2e-artifacts/verify-20260915-054958-509128/`:

- Dois testes novos reproduziram antes do fix: pausa de 500ms contaminando
  a EWMA e liberação de dois quadros vencidos após pausa de 100ms.
- Core corrigido: 116 unitários + 3 integração passaram (`core-corrected.log`).
  O relatório original conserva duas falhas intermediárias de arredondamento
  na troca 60→30 FPS, corrigidas e cobertas pelo reteste completo do core.
- Teste com decoder bloqueado exige apresentação antes de liberar o worker.
  Teste de 300 quadros regulares a 60 FPS exige retenção agendada zero.
- Harness, analyzer, servidor, web unit/build/browser, platform, platform-macos
  e app passaram; app inclui 107 unitários e 4 smoke, com 1 interativo ignorado.
  Frontend e binários release macOS recompilados.
- Cadência 1 viewer: 57,650 FPS, gap 164,105ms, zero drops; FAIL. Hold médio
  13,485ms, máximo 27,127ms. Host encode máximo 158,482ms.
- Cadência 2 viewers: ambos 57,123 FPS, gaps 934,935/917,096ms; FAIL. Hold
  médio 10,880/10,971ms, máximo 28,288/22,661ms. Zero drops. Na janela dos
  gaps, host encode 905,229ms (prepare 897,916ms), sem PLI no viewer.

Os gates de 54 FPS/50ms foram preservados e continuam reprovados por gaps.
A correlação temporal com o host não identifica a causa da pausa do emissor.
Não há comparação A/B controlada nem nova confirmação visual remota. O limite
de dois quadros é orçamento de retenção agendada em regime estável, não
garantia de latência real ou proteção contra pausas longas. BUG-001 fica aberto.
