# Live media diagnostics (opt-in)

Build the frontend with `npm run build` in `app/web/`, then run
`cargo tauri build` in `app/`. Close the old app before launching the
instrumented release. Debug tracing is disabled unless `GOLIVE_TRACE_DIR`
is set before the process starts or the executable has the opt-in marker
described below. An explicitly empty `GOLIVE_TRACE_DIR` disables tracing.

## Normal launch of an unbundled executable

If launching through a shell produces a Screen Recording denial while
opening the executable normally works, use the normal launch method:
place an empty file named `.golive-media-trace` beside `goDrinking` (or
`goDrinking.exe`), then reopen the apps normally. No environment variable
or bash launcher is needed. Each process writes its own JSONL file in the
adjacent `media-trace/` directory. For `app/target/debug/goDrinking`, that
is `app/target/debug/media-trace/`.

Remove the marker and restart the apps to disable tracing. Use this method
only with unbundled executables; do not insert files into a signed `.app`
bundle. This does not grant or alter Screen Recording permission.

## Environment-based launch

On macOS, from the repository root:

```sh
bash scripts/debug-media.sh
```

The default directory is `e2e-artifacts/media-trace/` (git-ignored).
Each process appends `golive-trace-PID.jsonl`. Follow those files while
sharing a real screen with visible motion; test a static screen separately.
For two machines, enable tracing on both, reproduce, then collect both sets
of files. No debug server, port, uploads, or telemetry service is involved.

Windows PowerShell, next to the newly built exes:

```powershell
$env:GOLIVE_TRACE_DIR = Join-Path $PWD 'media-trace'
& .\goDrinking.exe
```

Launch normally without the variable to disable tracing again. Stop the
test processes before deleting their trace files.

## Reading the data

Each stage instance emits a numeric aggregate roughly once per second
when active, plus a final partial record on graceful teardown. Silence is
not a zero-rate sample: it can mean that stage is blocked or inactive.
`timestamp_ms` uses the machine clock; `elapsed_us` uses a monotonic clock.
`instance` distinguishes stage instances within a PID, not room/link IDs.

- `capture`: frames successfully forwarded by the screen bridge; drops
  include cadence rejection and a full downstream channel. Timeouts mean
  the platform stream supplied no packet within the bridge's 100 ms wait.
  Work time covers bridge conversion/forwarding, not OS capture time.
- `source`: frames received by the encoder from that bridge. Work time
  measures waiting for input. Timeouts distinguish repeated fallback
  frames from fresh capture arrivals.
- `encode`: encoded frames, actual H.264 bytes, keyframes, encode work time,
  dimensions and requested FPS. Includes GPU conversion fallback time.
  `intra_applied` counts viewer FIR/PLI requests applied as forced IDRs
  (inbound bursts coalesce: one applied IDR may answer several requests).
- `send`: access units submitted to the WebRTC track, H.264 bytes, and
  submission work time. Success does not establish delivery to the watcher.
- `rtp`: here `frames` counts RTP packets, not video frames; `bytes` is RTP
  payload bytes, excluding headers and transport overhead.
- `decode`: decoded frames and decode-plus-RGBA-conversion work time.
  `dropped` means an access unit produced no picture (including decode
  errors). Assembly/wait time is excluded. `pli_sent` counts PLI requests
  actually sent for irrecoverable access-unit gaps (debounced ~1/s per
  SSRC); `pli_suppressed` counts gaps that asked for nothing because the
  debounce window was still held — a storm reads as `pli_sent: 1` beside a
  large `pli_suppressed`.
- `present`: helper acknowledgements and RGBA bytes. Work time includes
  IPC, helper rendering, and the acknowledgement wait. `repeats` counts
  re-sends in older traces; these are not fresh video frames. The feeder now
  waits for a fresh frame after a timeout, so new traces should report zero
  repeats. `max_gap_us` is the worst ack-to-ack gap inside the record
  (microseconds, 0 with fewer than 2 acks): presenter judder the 1s-average
  rate hides. The analyzer reports it per present summary and raises
  `JITTER` when it reaches 2x the mean present interval.
- `present`: helper acknowledgements and RGBA bytes. Work time includes
  IPC, helper rendering, and the acknowledgement wait. `repeats` counts
  re-sends in older traces; these are not fresh video frames. The feeder now
  waits for a fresh frame after a timeout, so new traces should report zero
  repeats.

Rate = count × 1,000,000 / `elapsed_us`. Fresh presentation rate uses
`frames - repeats`. Mean measured work is `work_us / observations`;
`max_work_us` reveals individual stalls. `gpu_frames` identifies retained
GPU capture packets. Counts reset each record; dimensions/FPS describe the
last observed nonzero value within that record. CPU/GPU utilization itself
must be sampled using OS profiling tools; these timings are not utilization.

The schema accepts only numbers and a fixed stage enum. It cannot carry
pixels, screen titles, nicknames, room codes, tokens, passwords, SDP, or
candidates. Trace write failures disable that trace without stopping media.

## Repeatable moving-video check

The debug profiles keep OpenH264 optimized because its RGBA conversion runs
on the receive path. To check sustained throughput without screen permission,
generate a local test fixture at the repo root:

```sh
mkdir -p e2e-artifacts
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1280x720:rate=30 -t 3 -c:v libx264 -profile:v baseline -preset ultrafast -x264-params slices=1 -y e2e-artifacts/motion-repro.h264
cd app
GOLIVE_MOVIE="$PWD/../e2e-artifacts/motion-repro.h264" cargo test --manifest-path ../core/Cargo.toml --test e2e_two_peer movie_file -- --nocapture
```

Requires FFmpeg and Node on PATH. The movie test asserts at least 125 fresh
decoded frames over five seconds after startup, for a 30 FPS source. It does
not verify native capture, helper rendering, 60 FPS, or a remote Windows peer.


## Current WebView player cadence

The desktop canvas now emits `present` records too. One valid player ack
counts a completed canvas draw (not a measurement of physical scan-out).
`work_us` spans dispatch to ack, including IPC and the draw; `max_gap_us`
is the largest gap between successful draw acknowledgements. `dropped`
counts pending decoded frames superseded before dispatch plus failed draw
acks. `repeats` remains zero; a cached image transferred into a popup can
still count as a draw. The helper and WebView use separate stage instances.

The canvas updates when a decoded frame arrives and acks that draw directly.
It does not wait for an extra requestAnimationFrame tick in the one-frame
IPC round trip. The browser compositor still controls screen refresh.

A local end-to-end cadence check exercises the real Tauri Channel and canvas,
using `--e2e-plan` with optional `quality` (ordinary app behavior unaffected):

```sh
python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/cadence-check
```

Requires a release executable built after `npm run build` and the fixture
`e2e-artifacts/live-20260912-viewer/motion-1080p60.h264` (or pass `--movie`).
Generate a fixture with FFmpeg `testsrc2=size=1920x1080:rate=60`, one second,
H.264 baseline, `slices=1`; the test loops its frames at the explicit profile.
Pass `--binary` and optionally `--viewer-binary` for other executable locations.
Keep the viewer visible. The check uses only a loopback test room, measures
30 seconds after warmup, and stops its own processes. It requires >=54 FPS
both decoded and drawn, and <=50 ms worst ack gap. This is a performance
check sensitive to host load/display scheduling, not a deterministic unit test.
It does not establish Windows native performance when executed on macOS.


## Viewer YUV / GLP2 and shared host encode (2026-09-13)

The desktop player now receives tight I420 (OpenH264) or NV12 (MF/DXVA),
then uploads reusable WebGL textures and converts BT.601 limited-range color
in the fragment shader. Without WebGL, a reusable Canvas2D ImageData buffer
provides the CPU fallback. This is not native GPU surface sharing: CPU YUV
still crosses IPC. At 1080p60 the payload falls from 497,664,000 to
186,624,000 bytes/second (62.5% less), excluding headers and additional copies.
Mac decode is still OpenH264; this patch does not add VideoToolbox decode.

Desktop IPC is versioned **GLP2**: four magic bytes, then LE u32 seq, width,
height, format; pixels begin at offset 20. Format 0 = RGBA, 1 = I420, 2 = NV12.
YUV dimensions are even and payload size is exactly w*h*3/2. Both endpoints
ship together. The separate GLV1 helper protocol stays unchanged and RGBA.

New numeric-only trace stages (opt-in with the existing trace mechanism):

- `codec`: decoder call, including backend output/readback where applicable.
- `convert`: luma validation plus compact plane packing or legacy RGBA conversion.
- `dispatch`: synchronous shell callback, including packet copy and IPC submission.
- `draw`: JavaScript draw/upload submission time reported with a valid ACK;
  `gpu_frames` distinguishes WebGL from fallback. Browser timer precision applies.
- `present` retains send-to-ACK time and ACK gaps. It does not measure scanout or
  GPU completion. Dispatch, draw and present overlap; do not add their times as
  if they were independent pipeline segments.

`GOLIVE_VIEWER_RGBA=1` selects the legacy pixel layout for a diagnostic comparison
using the same current WebGL renderer. It does not select the previous binary's
Canvas2D path. Leave unset for compact YUV. On Windows the existing
`GOLIVE_DISABLE_HW=1` hook can independently compare software decoding.

Host FrameSlot now waits for capacity before encoding and never replaces an
encoded reference frame. Stale raw capture inputs are drained before encode and
counted in `source.dropped`. Deadlines reanchor after overruns. RTP still uses the
nominal per-sample duration; capture-time/A-V synchronization is a separate
remaining task. Multiple viewers share one capture/encoder/video track with
independent peer connections and audio. `host.send.bytes` counts shared source
payload once, not total wire upload across bindings. One transport write task
fans out through webrtc-rs; a blocked local socket write can still delay the
shared producer. No independent per-peer quality adaptation is introduced.

Run `python3 scripts/check-viewer-cadence.py --artifact <new-directory> --viewers 2`
with the built executable. The report requires one host encode instance and checks
each viewer's FPS and largest ACK gap. A near-60 average does not override a
failed maximum-gap check. WGC's gate is tested without Windows using
`cargo test --manifest-path ../platform/Cargo.toml --test windows_readback` from app/;
this tests skipped readback callbacks, not native driver performance.

GPU conformance fixture: bundle `app/web/src/playerRenderer.ts` with esbuild into
`playerRenderer.js`, copy `scripts/fixtures/player-gpu-check.html` beside it as
`index.html`, and serve that directory locally. It checks real GL pixels against
the CPU reference (orientation, colors, NV12/I420, resize, 1920-wide precision,
context loss/restoration) plus the no-WebGL fallback. The artifact used here is
`e2e-artifacts/viewer-gpu-check/`.


### Acquisition and decoder isolation (2026-09-14)

On macOS, fresh builds emit `capture_input` before the SCK callback throttle.
`frames` counts screen callbacks, not necessarily valid/new video pictures.
`gate_dropped`, `queue_dropped` and `invalid_frames` distinguish early throttle,
full delivery channel and samples without usable pixels. `max_gap_us` is the
largest callback arrival interval, not capture-to-display latency. Counts can
straddle adjacent trace windows; aggregate several seconds. Windows does not
provide these acquisition counters yet. Existing running processes must restart
with the new binary to emit them; rebuilding does not update a running host.

The decoder now owns a dedicated serial thread. `codec` and `convert` measure
work inside that thread; `decode` also includes scheduling and reply overhead.
The receive task still awaits each decoded access unit before reading the next;
this isolates blocking codec work from Tokio but is not a separate RTP drain or
a presentation jitter buffer. Compressed access units retain their order.

A capture-only fixture is available from `app/`:

```sh
GOLIVE_CAPTURE_DISPLAY=3 cargo test --release --manifest-path ../platform-macos/Cargo.toml --test capture_probe -- --ignored --nocapture
```

It captures for 30 seconds without saving pixels or opening a viewer. The test
binary needs its own screen recording permission. The local attempt was denied;
it is intentionally ignored in the default suite and is not evidence of live
capture passing. A standalone main-app CLI attempt timed out and was removed.
For the ordinary app path, enable tracing as above, restart the fresh host and
share the requested display normally. Keep the viewer off the captured display.


### macOS hardware viewer decode

The desktop app now installs the platform-macos VideoToolbox decoder through
`platform::decode::VideoDecoder`. Core-only users retain software unless their
shell installs a native factory. Windows MF selection remains unchanged.
VideoToolbox requires hardware in the decoder specification; creation failure
falls back to software. A redacted `decode backend=videotoolbox` line is emitted
only after successful native session creation. `GOLIVE_DISABLE_HW=1` on the
viewer process selects software for comparison (it also disables encoding
hardware if used on a host process).

Decompression uses neither asynchronous nor temporal-processing flags: the
callback completes before the call returns. The callback copies visible NV12
rows while the CVPixelBuffer is valid; no borrowed native pointer crosses into
core/IPC. A native failure destroys the session, retains validated parameter
sets and waits for IDR before resuming in software; it never starts a fresh
software decoder with dependent frames from the old session.

This is hardware decode plus CPU NV12 transfer and WebGL rendering, not native
surface presentation. `codec` now includes the VideoToolbox call and NV12 copy;
`convert` includes statistics and optional RGBA fallback. Do not compare the
substage labels as if their work were identical to OpenH264's plane extraction.
Real native regression tests cover separate SPS/PPS, delta frames, resolution
changes to 1080p, padding and luma/chroma agreement with software. Core tests
inject driver failure on a delta/IDR and IDR without in-band parameter sets.


For missing-image SCK callbacks, `idle_frames` and `blank_frames` are subsets
of `invalid_frames` (the historical name for callbacks without usable pixels).
Do not add them to that total or treat idle as corruption. They reflect Apple's
numeric status metadata, read only when neither GPU nor CPU pixels are available.
Older host builds omit these two fields and cannot establish the reason.


O trace opt-in também mede `cpu_work_us`, `cpu_samples` e `max_cpu_work_us`
nos estágios encode/decode quando o backend fornece relógio de CPU da thread
(macOS). Esse relógio exclui espera, trabalho da GPU e de outras threads;
`max_work_us` continua sendo tempo de parede. Os máximos por janela podem
pertencer a quadros diferentes. Campo sem amostras não significa CPU zero.


No encoder VideoToolbox, `encode_submit` mede a chamada de submissão ao VT;
`encode_completion` mede da submissão até o callback terminar de preparar a
unidade H.264 e enfileirá-la; `encode_resume` mede desse ponto até o worker
retomar após consumir a conclusão. A captura do timestamp no callback é
opt-in e não escreve arquivos na thread do driver. Os intervalos se sobrepõem:
**não somar** submit/completion. Completion inclui agendamento do callback e
extração H.264, não apenas execução na GPU. Resume também pode incluir trabalho
síncrono que o worker ainda precisava concluir antes de consumir a fila.

`encode_prepare` mede no worker a conversão I420→NV12 para o staging reutilizável
(`i420_to_nv12_into`, sem alloc por frame) mais o memcpy para o pixel buffer do pool
(bulk por plano quando o stride é tight). `encode_pool` mede só `create_pixel_buffer` +
lock dentro do mesmo bloco — é sub-intervalo do prepare antigo (`prepare + pool ≈
prepare` anterior). No próximo cadence: pool dominante = backpressure do VT; prepare
dominante = memcpy/conversão. **Não somar** nenhum deles com completion/submit.

O viewer mantém no máximo 2 quadros entre decode e `on_frame`, num relógio
suavizado dos deltas RTP: EWMA com alpha 0.125, clamp 1ms..1s e slew de no
máximo 2ms por quadro. A primeira amostra plausível substitui a semente;
deltas 0 e >1s são ignorados. Pausas isoladas não alteram o ritmo estimado
(ver revisão abaixo). O primeiro quadro apresenta imediatamente. Em regime
estável, a fila agenda até 2 intervalos (~33ms a 60fps) de retenção; isso não
limita atrasos do SO/IPC. Após uma pausa, quadros vencidos obsoletos são
descartados, sem apresentar uma rajada para compensar.

Estouro e descarte de quadros vencidos são pós-decode: contam em
`decode.dropped`, sem PLI nem espera por IDR. O decoder já avançou suas
referências. Erros de decode e estados de recuperação solicitam PLI com
debounce. `dropped` sozinho não identifica a causa: também pode representar
AU obsoleta ou decode sem saída. `pacer_hold` mede somente retenção agendada;
gaps de ACK e máximos agregados não provam causalidade por quadro.

O host segue sem rajada pós-overrun: após estouro o `encode_loop` reancora o deadline
para `frame_time + frame_duration` (pula slots, nunca publica 2 unidades para
compensar) e o `FrameSlot` comporta 1 unidade — no máximo 1 publicada por intervalo,
latest-only. Nenhuma mudança foi necessária; latência base intacta.

Micro-travadas do viewer separam-se em duas medidas: `present` conta por janela
quantos gaps de ack passaram de 20/25/34/50ms (`gap_gt_*`, aninhados — um stall de
60ms conta nas quatro faixas; subtrair faixas adjacentes para histograma exclusivo) e
`pacer_hold` mede por quadro apresentado só a retenção agendada (`due − ready`, ≥0).
`pacer_hold` exclui espera de decode, IPC, draw e ack; `present` contém
dispatch+draw+ack — **não somar** hold+dispatch+present. Leitura: `pacer_hold`
alto indica retenção agendada. Hold ~0 com gaps altos não localiza a causa:
pode faltar quadro upstream, o processo pode retomar tarde ou haver atraso de
IPC/draw/ack. `due − ready` não mede `release − due` nem latência total.
Registros antigos sem os campos leem como 0.


### Revisão do pacer: deadlines durante decode (15/09/2026)

O mesmo future de leitura/decode fica ativo enquanto deadlines liberam quadros
já prontos. O decoder continua serial; não há fila adicional nem pre-roll.
Após desescalonamento, somente o quadro vencido mais recente é liberado; os
obsoletos contam em `decode.dropped`, sem PLI. Overflow também é pós-decode.
Erros de decode, inclusive software após o primeiro quadro, solicitam PLI com
o debounce existente. O fallback Windows aguarda IDR antes de alimentar o
decoder software novo. Detecção completa de perdas/reordenação RTP continua
fora desta alteração.

Deltas maiores que duas vezes o intervalo estimado precisam de três amostras
semelhantes para mudar o ritmo; uma pausa isolada não desacelera a retomada.
Isso filtra timestamps, sem guardar quadros adicionais. A capacidade permanece
2: em regime estável, retenção agendada até dois intervalos (~33ms a 60fps);
não é limite de latência real sob pausa do SO, IPC ou mudança de FPS.

`decode.work_us` cobre submissão até retorno observado do worker. Agora pode
sobrepor também o dispatch de um quadro anterior enquanto aguarda o decoder.
Não somar esse tempo com dispatch, hold ou present para obter latência.


### Diagnóstico detalhado do preparo VT (15/09/2026)

Novos subestágios opt-in: `encode_convert` (I420→NV12), `encode_copy` (pixels
para o buffer VT) e `encode_unlock`. Parede e CPU da thread são medidos na
mesma operação. Eles sobrepõem `encode_prepare`, que também inclui bookkeeping
e I/O dos traces internos; não somar com prepare/encode.

`cpu_at_max_work_us` é a CPU da observação que estabeleceu `max_work_us`, válida
quando `cpu_at_max_work_available=1`. Diferente de `max_cpu_work_us`, que pode
vir de outra observação. Empates não apagam um pareamento disponível.

`previous_write_us`, `previous_write_cpu_us` e
`previous_write_cpu_available` descrevem a serialização/escrita do flush
anterior do mesmo estágio, carregadas no registro seguinte. A última escrita
pode não ter sucessor. Não atribuir esse custo à emissão do registro atual.

O cadence salva `start_ms`/`end_ms`, snapshots de memória no macOS e aceita
`--sample-host` e `--no-host-trace` (controle que não aprova o gate completo).
O exemplo `encode_probe` isola o encoder de WebRTC/WebView. Experimentos e
limitações em [DIAGNOSTICO-host-2026-09-15.md](DIAGNOSTICO-host-2026-09-15.md).


`max_work_end_ms` / `max_gap_end_ms` registram o término do pico selecionado,
não o flush. Callback completion/resume preservam o instante de término mesmo
quando reportados depois; precisão de ms, sem identidade de frame.
`--observe-host` acrescenta snapshots numéricos de libproc da thread encoder
(~10 ms) e memória do processo (~100 ms), apenas para o host criado pelo teste.
`python3 scripts/correlate-host-stalls.py <artifact>` cruza picos contínuos do
encoder com esses snapshots. RUNNING inclui runnable; WAITING não revela o
recurso; page-ins são do processo inteiro. Buracos do observador são reportados.
Não soma estágios nem correlaciona send/prepare líquido como trabalho contínuo.
