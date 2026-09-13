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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--binary', type=Path, default=ROOT / 'app/target/release/goDrinking')
    parser.add_argument('--viewer-binary', type=Path)
    parser.add_argument('--movie', type=Path, default=ROOT / 'e2e-artifacts/live-20260912-viewer/motion-1080p60.h264')
    parser.add_argument('--seconds', type=int, default=30)
    args = parser.parse_args()
    art = args.artifact.resolve()
    art.mkdir(parents=True, exist_ok=False)
    if not args.binary.is_file() or not args.movie.is_file():
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
        for role in ('host', 'viewer'):
            plan = dict(role=role, server=base, password='local-cadence-test', nickname=f'{role}-cadence',
                        code_file=str(art / 'code'), status_file=str(art / f'{role}.json'))
            if role == 'host':
                plan.update(share=f'movie:{args.movie.resolve()}', quality=dict(w=1920, h=1080, bitrate_kbps=6000, fps=60))
            binary = args.viewer_binary if role == 'viewer' and args.viewer_binary else args.binary
            launch([str(binary.resolve()), '--e2e-plan', json.dumps(plan)], role, {'GOLIVE_TRACE_DIR': str(art / role)})
        deadline = time.monotonic() + 90
        while True:
            host, viewer = read_json(art / 'host.json'), read_json(art / 'viewer.json')
            if host.get('qualityApplied') and viewer.get('presented', 0) > 0:
                break
            if any(p.poll() is not None for p in children) or time.monotonic() > deadline:
                raise RuntimeError('media startup failed; inspect local artifacts')
            time.sleep(.2)
        time.sleep(5)  # Exclude the profile switch and warmup from the measurement.
        start_ms = time.time() * 1000
        print(f'measuring {args.seconds}s of real player cadence', flush=True)
        time.sleep(args.seconds)
        end_ms = time.time() * 1000
        stages = {}
        for role in ('host', 'viewer'):
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
                    dropped=sum(r['dropped'] for r in a), max_work_ms=max(r['max_work_us'] for r in a) / 1000,
                    max_gap_ms=max(r['max_gap_us'] for r in a) / 1000)
        decode, present = stages.get('viewer.decode', {}), stages.get('viewer.present', {})
        passed = decode.get('fps', 0) >= 54 and present.get('fps', 0) >= 54 and present.get('max_gap_ms', float('inf')) <= 50
        report = dict(passed=passed, minimum_fps=54, maximum_present_gap_ms=50, stages=stages)
        (art / 'verdict.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2))
        return 0 if passed else 1
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
