# Investigação das pausas do host — 15/09/2026

## Resultado

O pico de 905ms visto no teste anterior ainda não tem causa de kernel identificada.
Um novo baseline reproduziu encode de 785ms sem compilação concorrente, mas as
execuções instrumentadas posteriores não repetiram centenas de milissegundos em
regime. Há pausas com tempo de parede muito maior que CPU da própria operação.
Não foi aplicada uma correção especulativa de codec, prioridade ou buffer.
BUG-001 permanece aberto. Revisão independente feita com auxílio do Luna.

## Experimentos

Artefatos: `e2e-artifacts/host-prepare-20260915-*`.
Resumo numérico: `e2e-artifacts/host-prepare-investigation-summary.json`.
As tabelas de cadência usam o mesmo recorte do gate (`start_ms + 1200` até
`end_ms`, pelo timestamp de emissão dos registros). Picos do warmup não entram
nessas comparações. São janelas agregadas, não eventos sincronizados por frame.

| Ensaio | Duração medida | Viewers | Maior gap de ACK | Resultado |
|---|---:|---:|---:|---|
| Baseline anterior à instrumentação nova | 30s | 2 | 494.3 / 591.5ms | FAIL; host encode 785ms |
| Sem trace no host #1 | 30s | 2 | 99,6 / 105,4ms | viewer reprova; gate completo indisponível |
| Instrumentado #1, com sample | 30s | 2 | 62,6 / 60,6ms | FAIL |
| Sem trace no host #2 | 30s | 2 | 43,0 / 42,6ms | gaps aprovados; gate completo indisponível |
| Instrumentado #2 | 30s | 2 | 66,9 / 65,6ms | FAIL |
| Instrumentado, um viewer | 30s | 1 | 33,6ms | PASS, 60,0 FPS |
| Instrumentado longo, com sample | 90s | 2 | 54,1 / 56,9ms | FAIL, ~59,9 FPS |
| Filme reduzido a 12 frames | 45s | 2 | 24,9 / 29,7ms | PASS, ~60 FPS |
| Filme original novamente | 45s | 2 | 31,5 / 37,3ms | PASS, ~60 FPS |

Encode isolado (`app/examples/encode_probe.rs`, sem WebRTC/WebView):

- Trace ligado, 30s: 1750 quadros, encode máximo 36,166ms, nenhum >50ms.
- Trace desligado, 30s: 1658 quadros, encode máximo 26,559ms, nenhum >50ms.
- A geração/pacing da fonte não entra no custo do encode; contagens diferentes
  impedem interpretar o menor máximo como ganho geral de throughput.
- A fonte de barras do probe é diferente do filme: isto é isolamento do caminho,
  não um A/B de qualidade ou complexidade H.264 idêntica.

## O que os dados permitem dizer

1. **Escrita síncrona do trace não é causa necessária.** O controle sem trace
   também teve gaps >100ms. Nas janelas instrumentadas de 30s, a maior escrita
   anterior registrada foi ~0,32ms; no ensaio longo, 6,881ms. Não explica por si
   os picos observados. A instrumentação pode perturbar agendamento, portanto
   os controles desligados continuam importantes.
2. **Parede alta não significa custo de CPU alto.** Em regime, uma conversão
   levou 27,828ms com 3,643ms de CPU na mesma observação. No teste longo,
   encode chegou a 63,096ms com 1,262ms de CPU. São compatíveis com espera ou
   desescalonamento; não identificam qual recurso causou a espera. A observação
   de conversão de ~68ms durante a investigação ocorreu no warmup.
3. **O teste de filme tem um working set grande.** `preload_movie` guarda até
   300 frames I420 1080p (~0,93GB). Com 12 frames, RSS do host ficou ~143MB,
   contra ~937MB observado no original. Ambos os ensaios finais passaram:
   reduzir o working set não é condição necessária para passar. O clipe curto
   também muda o conteúdo repetido, logo esse controle não isola só memória.
4. **Havia pressão de memória global.** Em uma janela de 30s, `vm_stat` registrou
   ~1,6 milhão de descompressões e ~20 mil swap-ins (páginas de 16KiB). Isso
   inclui outros processos. Não atribui uma falha de página ao frame lento.
   No último ensaio original, `proc_pid_rusage` registrou pageins 460→460 no
   host; esse ensaio passou. Os primeiros contadores via `ps` eram indisponíveis
   (`-`), e não foram interpretados como zero.
5. **Profiling ainda não identifica a causa do kernel.** `sample` capturou a
   thread golive-encode em sleep de pacing, espera pelo callback VT e conversão/
   memmove. Seu agregado não separa runnable, page fault e bloqueio no instante
   exato do pico. `xctrace`/Instruments não está instalado neste ambiente.

Portanto, espera/desescalonamento sob carga e pressão de memória continuam
hipóteses; não há base para culpar exclusivamente VT, memória, trace ou App Nap.
O pico original veio do lane MovieFile, não de captura SCK: a captura com GPU
pode seguir `encode_cv_pixel_buffer`, evitando essa preparação I420/NV12.
Não extrapolar diretamente o teste do filme para o compartilhamento real.

## Instrumentação e reprodução deixadas no projeto

- `encode_convert`, `encode_copy`, `encode_unlock`: parede + CPU da mesma
  operação; sobrepõem `encode_prepare`, não somar com prepare/encode.
- `cpu_at_max_work_us` + `cpu_at_max_work_available`: CPU da observação que
  estabeleceu `max_work_us`. Não é o máximo independente de CPU da janela.
- `previous_write_*`: custo de serialização/escrita do flush anterior daquele
  estágio, carregado no registro seguinte. Não mede a escrita do registro atual;
  o último flush pode não ter registro seguinte.
- `--no-host-trace`: controle diagnóstico; não pode aprovar o gate completo.
- `--sample-host`: amostra somente o host iniciado pelo próprio teste, com
  saída em `host-sample.txt` e erros em `sample.log`.
- Janelas start/end e snapshots de memória agora ficam nos artefatos. O wrapper
  macOS usa RUSAGE_INFO_V0 documentado no SDK e distingue indisponível de zero.

Exemplos, a partir da raiz (diretórios de saída precisam ser novos):

```sh
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/host-next --viewers 2 --seconds 90 --sample-host --movie e2e-artifacts/verify-20260915-054958-509128/motion-1080p60.h264
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/host-control --viewers 2 --seconds 90 --no-host-trace --movie e2e-artifacts/verify-20260915-054958-509128/motion-1080p60.h264
```

Para encode isolado, em `app/`:

```sh
cargo build --release --features tauri/custom-protocol --example encode_probe
GOLIVE_TRACE_DIR=/tmp/golive-encode-probe ./target/release/examples/encode_probe 30
GOLIVE_TRACE_DIR='' ./target/release/examples/encode_probe 30
```

Próxima evidência necessária: capturar uma nova pausa grande já instrumentada,
preferencialmente com a fonte SCK real e System Trace para atribuir a espera
à thread/recurso. Não aumentar o buffer para esconder a pausa.

## Verificação do código deixado

- Core: 117 unitários + 3 integração passaram (`host-prepare-core-final.log`).
- App: 107 unitários + suites auxiliares e 4 smoke passaram; 1 fixture
  interativa ignorada (`host-prepare-app-final.log`).
- Harness Python: 7 testes passaram, incluindo leitura de memória de processo
  próprio e PID inexistente. Analyzer: 19 checks e parsing dos traces novos
  passaram. Frontend atualizado e binários release/exemplo recompilados.
- Logs em `e2e-artifacts/`; `git diff --check` limpo. Windows não executado.


## Nova captura: pausa grande correlacionada à thread

Ensaio `e2e-artifacts/host-timeline-20260915-2v/`: 180 segundos, filme
1080p60, dois viewers, observador externo a ~10 ms e `sample` do host.
Gate **FAIL**: apresentação 59,138/59,150 FPS, gaps máximos
303,756/304,870 ms. Média de FPS não encobre o problema.

- Maior encode: **354,771 ms de parede / 3,602 ms de CPU**, término
  `1789519812467` ms. Os gaps máximos dos viewers terminam em
  `1789519812478` e `1789519812476` ms: alinhamento temporal forte com
  a pausa do host.
- Submit→callback VT: **275,043 ms**. As 23 consultas dentro do intervalo
  aproximado mostram WAITING, com maior intervalo entre consultas de 13 ms.
  Isso localiza uma espera na conclusão do encoder, mas não distingue
  fila/hardware VT, trabalho no serviço ou atraso do callback.
- Outra pausa: conversão I420→NV12 **152,133 ms / CPU 4,465 ms**;
  12 consultas, cinco RUNNING/runnable, cinco UNINTERRUPTIBLE, duas WAITING;
  maior intervalo de consulta 14 ms. O tempo não é explicado só por CPU
  de conversão. Não há stack temporal que identifique o recurso de espera.
- Nos dois eventos acima, **delta de page-ins do host = 0** nos snapshots
  que os cercam (~100 ms). Isso não exclui compressão, minor faults,
  pressão de memória ou atividade de outro processo/serviço.
- Em outra conclusão VT de 116,660 ms houve +211 page-ins no intervalo
  ampliado de memória. Não atribuir esses page-ins ao encoder sem prova.
- Maior escrita anterior dos traces: 4,565 ms. Não explica sozinha 355 ms;
  não é uma prova de custo zero da instrumentação.

O observador obteve 15.057 snapshots. Algumas outras pausas têm buracos de
amostragem de até 90 ms: nelas, a ausência de estados de espera não permite
concluir ausência de espera. `sample` agrega stacks e não vincula uma stack
à pausa específica. Ensaio único com observação ativa; não é A/B causal.
O caminho testado é filme I420, não SCK/IOSurface ao vivo.

### Correções e limites da linha do tempo

`max_work_end_ms` associa um horário ao pico, separado do horário do flush;
`max_gap_end_ms` faz o mesmo para o gap de apresentação. Precisão de ms,
sem ID de frame. O correlator exclui `send` (outra thread) e `encode_prepare`
(custo líquido após subtrair pool, portanto não é intervalo contínuo).

Na build deste ensaio, completion usou o horário de registro após o worker
retomar, não o horário exato do callback. O custo de 275,043 ms continua
válido; a posição temporal é aproximada (resume máximo do ensaio 10,577 ms,
além de possível bookkeeping). Corrigido no código para próximos ensaios:
completion e resume usam seus respectivos Instants de término. Os traces
originais foram preservados. `thread_cpu_delta_raw` tem unidade ns e cobre
os snapshots ao redor do evento, incluindo trabalho vizinho.

Reprodução a partir da raiz, após atualizar frontend/binários:

```sh
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/host-timeline-next --seconds 180 --viewers 2 --observe-host --sample-host --movie e2e-artifacts/verify-20260915-054958-509128/motion-1080p60.h264
python3 scripts/correlate-host-stalls.py e2e-artifacts/host-timeline-next
```

**Conclusão:** há pausas no host antes de o viewer poder apresentar. Não
adicionamos buffer nem mudamos o pacing para escondê-las. A próxima comparação
útil é a mesma instrumentação no SCK real, que evita a conversão I420 desta
fixture; atribuir a espera restante do VT exige uma captura de scheduler/kernel
(ex.: Instruments System Trace, indisponível nesta instalação). BUG-001 aberto.


Validação final desta rodada: 118 testes unitários core + 3 integração,
11 testes Python e 19 checks do analyzer passaram; `git diff --check` limpo.
A correção posterior do timestamp de callback foi compilada/testada no core;
o ensaio release descrito acima precede essa correção. Nenhuma mudança de
buffer, algoritmo de pacing ou política de codec foi feita nesta rodada.


## Tentativa autorizada de compartilhar Display 3

Fonte explicitamente solicitada: `display:3`, 1080p60/6000 kbps, dois viewers.
Frontend e binários recompilados; segundo ensaio com bundle Tauri assinado
pela identidade configurada no projeto, incluindo helper atualizado.
Tanto `display3-live-20260915-observed` (executável avulso) quanto
`display3-live-20260915-bundle` (bundle assinado) retornaram PermissionDenied
na captura: zero frames, qualidade ainda não aplicada. Portanto não há
medição de cadência do Display 3 nestas tentativas, nem conclusão sobre SCK.
É necessária autorização do goDrinking em Gravação de Tela do macOS para repetir.
Não houve alteração das permissões ou da política TCC por script.
