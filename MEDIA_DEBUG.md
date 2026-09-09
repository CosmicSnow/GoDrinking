# Live media diagnostics (opt-in)

Build the frontend with `npm run build` in `app/web/`, then run
`cargo tauri build` in `app/`. Close the old app before launching the
instrumented release. Debug tracing is disabled unless `GOLIVE_TRACE_DIR`
is set before the process starts or the executable has the opt-in marker
described below. An explicitly empty `GOLIVE_TRACE_DIR` disables tracing.

## Normal launch of an unbundled executable

If launching through a shell produces a Screen Recording denial while
opening the executable normally works, use the normal launch method:
place an empty file named `.golive-media-trace` beside `golive-app` (or
`golive-app.exe`), then reopen the apps normally. No environment variable
or bash launcher is needed. Each process writes its own JSONL file in the
adjacent `media-trace/` directory. For `app/target/debug/golive-app`, that
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
& .\golive-app.exe
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
- `send`: access units submitted to the WebRTC track, H.264 bytes, and
  submission work time. Success does not establish delivery to the watcher.
- `rtp`: here `frames` counts RTP packets, not video frames; `bytes` is RTP
  payload bytes, excluding headers and transport overhead.
- `decode`: decoded frames and decode-plus-RGBA-conversion work time.
  `dropped` means an access unit produced no picture (including decode
  errors). Assembly/wait time is excluded.
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
