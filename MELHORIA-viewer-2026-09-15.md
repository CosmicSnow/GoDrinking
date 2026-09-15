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
