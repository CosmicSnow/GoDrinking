# Viewer e host — implementação e medição em 2026-09-13

Implementadas as quatro mudanças autorizadas. O caminho principal do viewer agora transporta YUV compacto e converte a cor na GPU com WebGL. Em 1080p60, o payload de pixels cai de 497,7 para 186,6 MB/s: **62,5% menos bytes por passagem**. Ainda há cópia e IPC; não é decode/apresentação nativa sem cópias. No macOS, o decoder continua sendo OpenH264 por software.

## Alterações

- **Viewer:** I420 do decoder por software e NV12 do decoder Windows seguem compactos para o player. Texturas são reutilizadas; NV12 não exige separar crominância em JavaScript. Canvas 2D continua como fallback com ImageData reutilizado. O protocolo interno WebView é GLP2, com formato explícito; GLV1 do helper permanece RGBA e inalterado. Medições separadas cobrem codec, conversão/empacotamento, dispatch, desenho e ACK.
- **Host:** a fila não substitui mais unidades H.264 já codificadas. O produtor aguarda capacidade antes de adquirir/processar o próximo quadro e descarta entrada crua antiga. O relógio de pacing evita somar sistematicamente o custo do encode ao intervalo configurado.
- **WGC:** aplica o limite de FPS antes do readback GPU→CPU, libera os frames ignorados e drena a fila de captura com limite. Teste do callback real demonstra 30 readbacks para 60 chegadas no perfil de 30 FPS. Não se extrapola esse ganho para uma fonte já limitada ao FPS alvo.
- **Vários viewers:** conexões WebRTC independentes usam uma captura e um encoder compartilhados. A ponte de captura passa para uma sessão sobrevivente quando seu primeiro dono sai; o último viewer encerra o pipeline. Mudanças de qualidade e pedidos de intra usam o encoder comum. Não requer servidor de mídia; o upload continua crescendo por destinatário.

## Ensaios locais reais

Bundle macOS release, fonte de teste 1080p60, 6000 kbps, conexões locais, Channel Tauri e desenho no WebView. Aquecimento de cinco segundos seguido por aproximadamente 30 segundos de medição. O gate exige pelo menos 54 FPS e gap entre ACKs de no máximo 50 ms. ACK mede submissão do desenho, não scanout físico. Outros aplicativos permaneciam abertos; os ensaios sequenciais não são uma comparação controlada de carga total.

| Execução / diretório em e2e-artifacts | Viewers | FPS de apresentação | Maior gap | Gate |
|---|---:|---:|---:|---|
| cadence-compact (versão intermediária) | 1 | ~59,8 | 72,511 ms | FAIL |
| cadence-shared-two | 2 | ~59,86 cada | 98,507 / 98,494 ms | FAIL |
| cadence-rgba-control | 1 | 59,872 | 69,434 ms | FAIL |
| cadence-yuv-control | 1 | 60,000 | 26,538 ms | PASS |

O teste com dois espectadores confirmou **uma instância de encode**, zero descartes registrados no decode/present e ambos recebendo. As pausas quase simultâneas continuam sendo uma limitação; a média de 60 FPS não as resolve. Os dois controles usam o mesmo build final. O controle RGBA também desenha via WebGL, portanto não representa integralmente o antigo putImageData.

| Custo médio por quadro | Controle RGBA | Controle YUV |
|---|---:|---:|
| Codec | 2,287 ms | 2,203 ms |
| Conversão/empacotamento | 2,697 ms | 0,093 ms |
| Decode total (inclui as linhas anteriores) | 4,994 ms | 2,306 ms |
| Dispatch Rust/Channel | 0,256 ms | 0,100 ms |
| Desenho JavaScript | 0,608 ms | 0,261 ms |
| Envio até ACK | 3,827 ms | 1,858 ms |
| Payload de pixels | 496,6 MB/s | 186,6 MB/s |

Os estágios se sobrepõem; não somar todas as linhas como latência ponta a ponta. O contador GPU indica backend WebGL escolhido, não tempo de GPU. O envio H.264 mede payload codificado uma vez, não o upload agregado dos viewers. Veredictos completos ficam em `e2e-artifacts/<diretório>/verdict.json`.

## Verificação

- Core: 92 testes unitários e 3 de integração passaram. Regressão de congestionamento compara 12 unidades codificadas com uma sequência de referência e decodifica todas; teste com dois peers verifica continuidade após saída do primeiro viewer e troca de resolução.
- App: 106 testes de biblioteca passaram. Web: 101 testes passaram e build de produção concluído.
- Gate WGC: 2 testes passaram executando o helper real independente de OS. Analyzer: 14 autotestes passaram.
- WebGL real no WebKit/macOS: padrões I420, NV12 e RGBA, orientação, troca de resolução/formato, texturas reutilizadas, padrão alternado de 1920 pixels e perda/restauração de contexto passaram. Pixels lidos da GPU coincidiram com a referência CPU nos padrões testados. Fixture: `scripts/fixtures/player-gpu-check.html`.
- Bundle macOS gerado em `app/target/release/bundle/macos/goDrinking.app`; assinatura verificada com `codesign --verify --deep --strict`. A instalação em `/Applications` não foi substituída.
- Checagem Windows tentada com cargo-xwin: removendo flags apenas no processo de check e habilitando compatibilidade CMake 3.5, a dependência `audiopus_sys 0.2.2` ainda falha em intrínsecos SSE4.1/SSSE3 sem a feature correspondente. Log: `e2e-artifacts/windows-app-check.log`. Não foi gerado um executável Windows nem validado o conjunto das alterações contra esse target. Configuração permanente de build preservada.

## Limites e próximos critérios de aceitação

BUG-001/002 continuam abertos. Falta repetir Windows→Mac e Mac→Windows na sessão real, com conteúdo comprovadamente 60 FPS, áudio e traces simultâneos; validar consumo CPU/GPU, janela/pop-up, recuperação de perda e estabilidade com vários espectadores. Nenhum teste aqui comprova equivalência ao Discord.

A apresentação por superfície nativa (VideoToolbox/IOSurface/Metal e D3D11) permanece futura. O envio compartilhado ainda pode propagar demora de escrita local para todos; não há qualidade adaptativa individual. O relógio RTP ainda usa duração nominal, e os contadores não cobrem todos os timeouts/substituições na apresentação. Esses pontos não são considerados resolvidos pelos resultados locais.
