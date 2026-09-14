#!/usr/bin/env python3
"""Reusable local validation. Exit 0 only when every requested stage passes."""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--desktop', action='store_true', help='also build and measure real desktop 1080p60 with 1 and 2 viewers; needs a desktop session and ffmpeg')
    parser.add_argument('--artifact', type=Path, help='new output directory; default is a unique timestamped directory')
    args = parser.parse_args()
    artifact = (args.artifact or ROOT / 'e2e-artifacts' / ('verify-' + datetime.now().strftime('%Y%m%d-%H%M%S-%f'))).resolve()
    artifact.mkdir(parents=True, exist_ok=False)
    report = dict(passed=False, desktop=args.desktop, platform=sys.platform,
                  started_at=datetime.now(timezone.utc).isoformat(), stages=[],
                  limits=['Synthetic/local media does not validate screen capture permission, real devices, remote network or physical scanout.',
                          'Interactive/manual tests marked ignored by Cargo are not executed; inspect stage logs.'])
    environment = dict(os.environ)
    environment.setdefault('CMAKE_POLICY_VERSION_MINIMUM', '3.5')
    # Diagnostic overrides would change what this command claims to validate.
    for key in ('GOLIVE_VIEWER_RGBA', 'GOLIVE_DISABLE_HW', 'GOLIVE_MOVIE', 'GOLIVE_TRACE_DIR'):
        environment.pop(key, None)

    def save():
        (artifact / 'report.json').write_text(json.dumps(report, indent=2) + '\n')

    def run(name, command, cwd=ROOT, timeout=1200):
        print(f'RUN  {name}', flush=True)
        start = time.monotonic()
        log = artifact / f'{name}.log'
        command = list(map(str, command))
        command[0] = shutil.which(command[0]) or command[0]
        with log.open('w', encoding='utf-8') as output:
            try:
                result = subprocess.run(command, cwd=cwd, env=environment, stdout=output, stderr=subprocess.STDOUT, timeout=timeout)
                code = result.returncode
            except (OSError, subprocess.TimeoutExpired) as error:
                output.write(f'Validation could not finish: {error}\n')
                code = 124 if isinstance(error, subprocess.TimeoutExpired) else 127
        report['stages'].append(dict(name=name, passed=code == 0, exit_code=code,
                                     seconds=round(time.monotonic() - start, 2), log=str(log)))
        save()
        print(f'{"PASS" if code == 0 else "FAIL"} {name} ({report["stages"][-1]["seconds"]}s) — {log}', flush=True)
        return code == 0

    try:
        # Independent suites all run even when another suite fails.
        run('harness-tests', [sys.executable, '-m', 'unittest', 'discover', '-s', 'scripts/tests', '-v'])
        run('trace-analyzer', [sys.executable, 'scripts/analyze-trace.py', '--self-test'])
        run('server', ['npm', 'test'], ROOT / 'server')
        run('web-unit', ['npm', 'test'], ROOT / 'app/web')
        web_built = run('web-build', ['npm', 'run', 'build'], ROOT / 'app/web')
        run('web-browser', ['npm', 'run', 'test:browser'], ROOT / 'app/web', timeout=120)
        app = ROOT / 'app'
        for name in ('core', 'platform'):
            run(name, ['cargo', 'test', '--release', '--manifest-path', f'../{name}/Cargo.toml', '--', '--show-output'], app)
        native = {'darwin': 'platform-macos', 'win32': 'platform-windows'}.get(sys.platform)
        if native:
            run(native, ['cargo', 'test', '--release', '--manifest-path', f'../{native}/Cargo.toml', '--', '--show-output'], app)
        else:
            report['limits'].append('No native capture backend test lane for this OS.')
        if web_built:
            run('app', ['cargo', 'test', '--release', '--features', 'tauri/custom-protocol', '--tests', '--', '--show-output'], app)
        if args.desktop or sys.platform == 'win32':
            built = web_built and run('desktop-build', ['cargo', 'build', '--release', '--features', 'tauri/custom-protocol', '--bins'], app)
            if built and sys.platform == 'win32':
                run('windows-no-console', [sys.executable, 'scripts/check-windows-gui.py',
                    'app/target/release/goDrinking.exe', 'app/target/release/golive-video.exe'])
            if built and args.desktop:
                movie = artifact / 'motion-1080p60.h264'
                generated = run('motion-fixture', ['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i',
                    'testsrc2=size=1920x1080:rate=60', '-t', '6', '-an', '-c:v', 'libx264',
                    '-preset', 'ultrafast', '-tune', 'zerolatency', '-pix_fmt', 'yuv420p',
                    '-g', '60', '-bf', '0', '-f', 'h264', movie], timeout=120)
                if generated:
                    binary = ROOT / 'app/target/release' / ('goDrinking.exe' if sys.platform == 'win32' else 'goDrinking')
                    for viewers in (1, 2):
                        run(f'cadence-{viewers}-viewers', [sys.executable, 'scripts/check-viewer-cadence.py',
                            '--artifact', artifact / f'cadence-{viewers}', '--binary', binary,
                            '--movie', movie, '--viewers', viewers], timeout=180)
        report['passed'] = all(stage['passed'] for stage in report['stages'])
    except KeyboardInterrupt:
        report['interrupted'] = True
        report['passed'] = False
    finally:
        save()
    print(f'{"PASS" if report["passed"] else "FAIL"}: {artifact / "report.json"}', flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
