#!/usr/bin/env python3
"""Local real-WebView 1080p60 cadence check. Requires built desktop binary and motion fixture.
Example: python3 scripts/check-viewer-cadence.py --artifact e2e-artifacts/cadence-before
Runs only local test rooms; terminates only its own processes. No desktop capture permission.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def verdict_failures(stages, viewers, seconds):
    """Reject short/stale traces, silent failures and good averages with stalls."""
    failures = []
    required = ['host.encode', 'host.send']
    for role in ['viewer'] + [f'viewer{i}' for i in range(2, viewers + 1)]:
        required.extend(f'{role}.{stage}' for stage in ('decode', 'draw', 'present'))
    for name in required:
        stage = stages.get(name, {})
        if stage.get('seconds', 0) < max(1, seconds - 4):
            failures.append(f'{name}: insufficient trace coverage')
        if stage.get('fps', 0) < 54:
            failures.append(f'{name}: below 54 FPS')
        if stage.get('errors', 0) or stage.get('dropped', 0):
            failures.append(f'{name}: errors or dropped frames')
        if name.endswith('.present') and stage.get('max_gap_ms', float('inf')) > 50:
            failures.append(f'{name}: presentation gap above 50 ms')
        if name.endswith('.draw') and not stage.get('gpu_frames', 0):
            failures.append(f'{name}: WebGL not exercised')
    if stages.get('host.encode', {}).get('instances') != 1:
        failures.append('host.encode: expected exactly one shared encoder')
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--binary', type=Path, default=ROOT / 'app/target/release/goDrinking')
    parser.add_argument('--viewer-binary', type=Path)
    parser.add_argument('--movie', type=Path, default=ROOT / 'e2e-artifacts/live-20260912-viewer/motion-1080p60.h264')
    parser.add_argument('--seconds', type=int, default=30)
    parser.add_argument('--viewers', type=int, choices=(1, 2, 3), default=1)
    parser.add_argument('--source', help='explicit screen source, e.g. display:3; requires screen capture permission')
    args = parser.parse_args()
    if args.seconds < 5:
        parser.error('--seconds must be at least 5 for useful trace coverage')
    roles = ['host', 'viewer'] + [f'viewer{i}' for i in range(2, args.viewers + 1)]
    art = args.artifact.resolve()
    art.mkdir(parents=True, exist_ok=False)
    if not args.binary.is_file() or (not args.source and not args.movie.is_file()):
        raise SystemExit('Build the desktop binary and generate the motion fixture first')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    base = f'http://127.0.0.1:{port}'
    children, logs = [], []
    def launch(command, name, extra_env):
        log = (art / f'{name}.log').open('w')
        logs.append(log)
        process = subprocess.Popen(command, cwd=ROOT, env={**os.environ, **extra_env}, stdout=log, stderr=subprocess.STDOUT)
        children.append(process)
    try:
        launch(['node', str(ROOT / 'server/server.mjs')], 'server', {'PORT': str(port), 'BIND': '127.0.0.1'})
        deadline = time.monotonic() + 15
        while True:
            try:
                with urllib.request.urlopen(base + '/health', timeout=1):
                    break
            except OSError:
                if time.monotonic() > deadline:
                    raise RuntimeError('local server did not start')
                time.sleep(.1)
        for role in roles:
            plan = dict(role='host' if role == 'host' else 'viewer', server=base, password='local-cadence-test', nickname=f'{role}-cadence',
                        code_file=str(art / 'code'), status_file=str(art / f'{role}.json'))
            if role == 'host':
                plan.update(share=args.source or f'movie:{args.movie.resolve()}', quality=dict(w=1920, h=1080, bitrate_kbps=6000, fps=60))
            binary = args.viewer_binary if role != 'host' and args.viewer_binary else args.binary
            launch([str(binary.resolve()), '--e2e-plan', json.dumps(plan)], role, {'GOLIVE_TRACE_DIR': str(art / role)})
        deadline = time.monotonic() + 90
        while True:
            host, viewer = read_json(art / 'host.json'), read_json(art / 'viewer.json')
            if host.get('qualityApplied') and all(read_json(art / f'{role}.json').get('presented', 0) > 0 for role in roles[1:]):
                break
            if any(p.poll() is not None for p in children) or time.monotonic() > deadline:
                raise RuntimeError('media startup failed; inspect local artifacts')
            time.sleep(.2)
        time.sleep(5)  # Exclude the profile switch and warmup from the measurement.
        start_ms = time.time() * 1000
        print(f'measuring {args.seconds}s of real player cadence', flush=True)
        deadline = time.monotonic() + args.seconds
        while time.monotonic() < deadline:
            if any(p.poll() is not None for p in children):
                raise RuntimeError('a test process exited during measurement')
            time.sleep(min(.2, max(0, deadline - time.monotonic())))
        end_ms = time.time() * 1000
        stages = {}
        for role in roles:
            rows = []
            for path in (art / role).glob('golive-trace-*.jsonl'):
                for line in path.read_text().splitlines():
                    try:
                        row = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if start_ms + 1200 <= row['timestamp_ms'] <= end_ms:
                        rows.append(row)
            for stage in sorted({r['stage'] for r in rows}):
                a = [r for r in rows if r['stage'] == stage]
                elapsed = sum(r['elapsed_us'] for r in a) / 1e6
                stages[f'{role}.{stage}'] = dict(seconds=round(elapsed, 3), fps=round(sum(r['frames'] for r in a) / elapsed, 3),
                    instances=len({r['instance'] for r in a}),
                    mean_work_ms=round(sum(r['work_us'] for r in a) / max(1, sum(r['observations'] for r in a)) / 1000, 3),
                    bytes_per_second=round(sum(r['bytes'] for r in a) / elapsed), gpu_frames=sum(r['gpu_frames'] for r in a),
                    errors=sum(r['errors'] for r in a), dropped=sum(r['dropped'] for r in a), max_work_ms=max(r['max_work_us'] for r in a) / 1000,
                    max_gap_ms=max(r['max_gap_us'] for r in a) / 1000)
        failures = verdict_failures(stages, args.viewers, args.seconds)
        passed = not failures
        report = dict(passed=passed, failures=failures, viewers=args.viewers, minimum_fps=54, maximum_present_gap_ms=50, stages=stages)
        (art / 'verdict.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
        return 0 if passed else 1
    except Exception as error:
        (art / 'verdict.json').write_text(json.dumps(dict(passed=False, failures=[str(error)]), indent=2) + '\n')
        print(f'FAIL: {error}', flush=True)
        return 1
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
        for child in reversed(children):
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        for log in logs:
            log.close()


if __name__ == '__main__':
    raise SystemExit(main())
