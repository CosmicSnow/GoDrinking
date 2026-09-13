# Revisão de eficiência do host e viewer — 12/09/2026

Revisão local com apoio do Luna. O objetivo é cadência regular e baixa latência em 1080p60. Ainda não existe uma comparação controlada com Discord nas mesmas máquinas/rede; não há fundamento para prometer equivalência. Os traces Windows/Mac fornecidos mostram médias próximas de 60 FPS, mas picos de trabalho no viewer de até 122,6 ms.

## Correção aplicada nesta revisão

O viewer montava uma unidade H.264 por timestamp e só a liberava quando chegava um pacote do próximo quadro. Agora usa o marcador do último pacote para liberar a imagem imediatamente, mantendo o fallback por mudança de timestamp. O marcador é a indicação antecipada definida na [RFC 6184, seção 5.1](https://www.rfc-editor.org/rfc/rfc6184.html#section-5.1); o código não depende exclusivamente dele.

Isso retira uma espera de um intervalo entre quadros no fluxo regular (aproximadamente 16,7 ms a 60 FPS, 33,3 ms a 30 FPS). É uma consequência do agendamento, não uma medição de latência ponta a ponta. O último quadro marcado também aparece quando a fonte fica ociosa. O buffer comprimido é reciclado após consumo e pacotes repetidos do timestamp recém-liberado não reapresentam a imagem.

Arquivos: `core/src/media.rs`, `VideoAssembly` e `read_loop`. Nenhuma alteração nos protocolos de sala ou GLV1. A remoção anterior da espera extra por requestAnimationFrame no canvas foi preservada.

Teste executado a partir de `app/`:

```
cargo test --release --manifest-path ../core/Cargo.toml --lib viewer_releases_marked_picture_without_next_timestamp
```

Antes da correção: FAILED, “last RTP packet must release the picture while the source is idle”. Depois: PASS. O teste usa H264Payloader real, um IDR fragmentado com parameter sets e OpenH264 para decodificar o resultado; não envia um segundo timestamp para liberar o primeiro quadro. Outro teste cobre ausência de marker e rollover de timestamp. A suíte completa do core passou: 88 testes. O teste `two_peer_synthetic` com peers e servidor reais passou. Para viabilizar essa suíte no macOS, o teste preexistente de roundtrip de hardware passou a selecionar EngineKind::Auto e sua asserção específica de DXVA ficou protegida por cfg(windows).

## Melhorias prioritárias ainda pendentes

| Prioridade / situação | Evidência no código | Mudança proposta e validação necessária |
|---|---|---|
| Viewer: custo por quadro, inclusive com só um espectador | `H264Decoder::decode` converte para RGBA; `Surface::dispatch` copia RGBA para um Vec de IPC; `StreamPlayer` usa putImageData. 1080p60 transporta ~498 MB/s de pixels nesse limite local, antes das cópias adicionais. | Prototipar apresentação de YUV/texturas na GPU e decodificação acelerada no macOS. Comparar CPU total dos processos, p95/p99 de tempo por quadro, gaps de apresentação e memória, com o mesmo vídeo. O custo é comprovável; sua participação exata nos picos remotos ainda não foi isolada. |
| Host: descarte de H.264 sob congestionamento | `FrameSlot::publish` substitui uma unidade já codificada sem contar a substituição. A duração do vencedor não inclui os quadros substituídos. | Descartar antes do encode; quando perder unidade comprimida, preservar continuidade/reiniciar por IDR com política explícita. Contar descartes e manter o relógio RTP. Testar consumidor lento com IDR→deltas e recuperação; não basta confirmar que o decoder não lançou erro. Hoje um delta pode depender de um quadro que nunca foi enviado. |
| Host: múltiplos espectadores | `pump::on_watch` cria/adota um Publisher por watcher; `AppState::build_source_session` abre nova captura/bridge para Display/Window e novo encoder. Uma Track por Publisher não significa um encoder por sala. | Compartilhar captura e encode por fonte/perfil; manter PeerConnection, ICE, áudio e controle de congestionamento por conexão. Teste com 1/2/3 viewers deve mostrar encode/captura constantes, e tráfego crescendo com espectadores. Ganho de CPU ainda não medido. |
| Host Windows: captura de janela WGC | `grab_wgc_frame` faz readback GPU→CPU antes de `gate_open` no loop; try_send ignora fila cheia. | Adquirir/liberar frames excedentes sem copiar pixels, aplicar gate antes do readback e medir rejeições da fila. Validar nativamente no Windows 10 com fonte 60 Hz e perfil 30 FPS; não extrapolar ganho desse caso para a fonte já limitada a 60 FPS. |
| Diagnóstico sob carga | Trace decode acaba antes do callback síncrono on_frame; present mede ACK do canvas, não scanout. Timeout de ACK e substituição de unidades comprimidas não são contabilizados por completo. | Medir separadamente recepção, decode/conversão e envio à apresentação; contar filas, descartes e timeout. Usar percentis e janelas curtas, não só média de FPS. Evitar filas ilimitadas ao separar workers. |

A ordem depende do cenário: para o travamento observado com um viewer, priorizar o caminho de pixels e medir as pausas; para salas com vários espectadores, o encode compartilhado é essencial. Descarte de H.264 é risco de correção sob carga, não apenas uma oportunidade de reduzir CPU. Nada nesses achados prova que o bitrate de 6000 kbps seja o causador.

## Critério para aproximar a experiência desejada

Testar Windows→Mac e Mac→Windows com o mesmo vídeo 1080p60, áudio e as mesmas condições de rede. Medir FPS por janela, gaps/pausas, tempo até o primeiro quadro, latência ponta a ponta, recuperação de perda e CPU/GPU total. Repetir com 1 e vários viewers, janela normal e pop-up, além de mudança de resolução. A medição local do canvas não comprova fluidez física nem desempenho de um viewer Windows.

As mudanças estruturais da tabela são recomendações desta revisão e NÃO estão implementadas nesta entrega. A correção implementada é a liberação antecipada do quadro RTP, além do ajuste de canvas já existente.

## Validação do aplicativo empacotado

`python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/cadence-marker-review --binary app/target/release/bundle/macos/goDrinking.app/Contents/MacOS/goDrinking`

PASS em sessão local de 30 segundos após aquecimento e troca para 1080p60/6000 kbps. Traces confirmam 1920×1080 no encode/decode:

| Estágio | FPS | Maior trabalho registrado |
|---|---:|---:|
| Host encode | 60,005 | 10,055 ms |
| Host send | 60,002 | 1,322 ms |
| Viewer decode | 60,009 | 12,147 ms |
| Viewer present | 60,011 | 7,731 ms |

Maior intervalo entre ACKs de apresentação: 28,215 ms. Zero descartes registrados no decode/present na janela e nenhum erro de encode/decode nela. Não há contador completo de substituições do FrameSlot; zero em send não prova ausência delas. O gate do harness exige médias de pelo menos 54 FPS e gap máximo de 50 ms; não mede scanout nem percentis. A diferença em relação ao ensaio anterior não é uma comparação A/B controlada, portanto não se atribui uma porcentagem de ganho ao patch.

Web build, Tauri build e verificação da assinatura passaram. Bundle atualizado em `app/target/release/bundle/macos/goDrinking.app`. A instalação em `/Applications` não foi substituída. Não foi gerado ou validado executável Windows nesta revisão.
