#!/usr/bin/env bash
# E2E empacotado: duas instâncias do .app trocando vídeo sintético de verdade.
#
# - Sobe server/ local em porta livre, lança 2 binários do bundle com planos
#   e2e (CLI `--e2e-plan`), aguarda verdict/timeout, mata tudo sem resíduo.
# - Escreve e2e-artifacts/verdict.json (PASS/FAIL + first-missing) + guarda
#   screenshots e logs. Segredos nunca vão para logs (planos não são ecoados).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ART="$ROOT/e2e-artifacts"
APP="$ROOT/app/target/debug/bundle/macos/GoLive.app"
# The Mach-O inside is named after the cargo binary (golive-app), not the
# product name — resolve it by name (never first-executable: the staged
# video helper lives in the same dir).
BIN="$APP/Contents/MacOS/golive-app"
SERVER="$ROOT/server/server.mjs"
TIMEOUT_S="${E2E_TIMEOUT_S:-120}"
PASSWORD="e2e-packaged-local"

mkdir -p "$ART"
rm -f "$ART/code" "$ART/host.json" "$ART/viewer.json" \
  "$ART/verdict.json" "$ART/screen-final.png" \
  "$ART/host.log" "$ART/viewer.log" "$ART/server.log"

fail() { echo "E2E-FAIL: $1" >&2; return 1; }
[ -n "$BIN" ] && [ -x "$BIN" ] || { fail "no executable inside $APP/Contents/MacOS"; exit 1; }
# Video helper: Tauri bundles only the main binary, so the harness stages
# golive-video next to it (same dir the shell searches at runtime).
# Production packaging would use bundle.externalBin — documented follow-up.
HELPER_SRC="$ROOT/app/target/debug/golive-video"
HELPER_DST="$APP/Contents/MacOS/golive-video"
[ -f "$HELPER_SRC" ] || { fail "helper missing: run cargo build --bin golive-video in app/"; exit 1; }
cp -f "$HELPER_SRC" "$HELPER_DST" && chmod +x "$HELPER_DST" || { fail "helper stage failed"; exit 1; }
[ -f "$SERVER" ] || { fail "server missing: $SERVER"; exit 1; }
command -v node >/dev/null || { fail "node not found"; exit 1; }

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
BASE="http://127.0.0.1:$PORT"
SERVER_PID=""; HOST_PID=""; VIEWER_PID=""

PORT="$PORT" node "$SERVER" >"$ART/server.log" 2>&1 &
SERVER_PID=$!
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null || true
  [ -n "$VIEWER_PID" ] && kill "$VIEWER_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Health do server (limite 15s).
for _ in $(seq 1 150); do
  if curl -sf -m 2 "$BASE/health" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
curl -sf -m 2 "$BASE/health" >/dev/null 2>&1 || { fail "server-up (no health on $BASE)"; exit 1; }

HOST_PLAN="{\"role\":\"host\",\"server\":\"$BASE\",\"password\":\"$PASSWORD\",\"nickname\":\"host-e2e\",\"code_file\":\"$ART/code\",\"status_file\":\"$ART/host.json\"}"
VIEWER_PLAN="{\"role\":\"viewer\",\"server\":\"$BASE\",\"password\":\"$PASSWORD\",\"nickname\":\"viewer-e2e\",\"code_file\":\"$ART/code\",\"status_file\":\"$ART/viewer.json\"}"

"$BIN" --e2e-plan "$HOST_PLAN" >"$ART/host.log" 2>&1 &
HOST_PID=$!
"$BIN" --e2e-plan "$VIEWER_PLAN" >"$ART/viewer.log" 2>&1 &
VIEWER_PID=$!

# Espera dirigida por status com first-missing ordenado.
START="$(date +%s)"
FIRST_MISSING="server-up"
verdict_check() {
  python3 - "$ART" <<'EOF'
import json, os, sys
art = sys.argv[1]
def load(name):
    try:
        with open(os.path.join(art, name)) as f:
            return json.load(f)
    except Exception:
        return None
host = load("host.json") or {}
viewer = load("viewer.json") or {}
code = None
try:
    with open(os.path.join(art, "code")) as f:
        code = f.read().strip() or None
except Exception:
    pass
checks = [
    ("host-room", bool(code)),
    ("host-connected", host.get("state") in ("connected", "quality-applied") and host.get("connected") is True),
    ("host-keyframe", host.get("keyframesSeen") is True),
    ("viewer-joined", viewer.get("state") in ("joined", "watching", "connected")),
    ("viewer-connected", viewer.get("state") == "connected" and viewer.get("connected") is True),
    ("viewer-frames", isinstance(viewer.get("frames"), int) and viewer["frames"] > 0),
    ("viewer-presented", isinstance(viewer.get("presented"), int) and viewer["presented"] > 0),
    ("host-quality", host.get("qualityApplied") is True),
]
for name, ok in checks:
    if not ok:
        print(f"MISSING:{name}")
        sys.exit(0)
print("PASS")
EOF
}

while true; do
  NOW="$(date +%s)"
  ELAPSED=$((NOW - START))
  RESULT="$(verdict_check)"
  if [ "$RESULT" = "PASS" ]; then
    FIRST_MISSING=""
    break
  fi
  FIRST_MISSING="${RESULT#MISSING:}"
  if [ "$ELAPSED" -ge "$TIMEOUT_S" ]; then
    break
  fi
  # Processos morreram cedo? Falha rápida com missing atual.
  if ! kill -0 "$HOST_PID" 2>/dev/null || ! kill -0 "$VIEWER_PID" 2>/dev/null; then
    break
  fi
  sleep 2
done

# Screenshot da tela (janelas com estado). Prova pixels da UI, não do vídeo
# (render de vídeo é gate posterior — declarado, não simulado). Em sessões
# sem permissão de gravação de tela o screencapture falha; o erro exato fica
# registrado e a lista de janelas (sem pixels) serve de evidência alternativa.
SCREEN_OK=true
SCREEN_ERR=""
if ! screencapture -x -t png "$ART/screen-final.png" 2>"$ART/screencapture.err"; then
  SCREEN_OK=false
  SCREEN_ERR="$(head -c 300 "$ART/screencapture.err" 2>/dev/null || true)"
fi
[ -s "$ART/screen-final.png" ] || { SCREEN_OK=false; SCREEN_ERR="${SCREEN_ERR:-empty screenshot}"; }

# Lista de janelas on-screen. Prova PRESENÇA das duas instâncias (o WRY não
# espelha document.title no kCGWindowName, então o conteúdo da janela não é
# observável por aqui — o estado vem dos status do backend + screenshot).
WINDOWS_OK=true
if ! swift "$ROOT/scripts/winlist.swift" >"$ART/windows.txt" 2>"$ART/winlist.err"; then
  WINDOWS_OK=false
fi
GOLIVE_WINDOWS="$(grep -c "^GoLive :: " "$ART/windows.txt" 2>/dev/null || true)"

if [ -z "$FIRST_MISSING" ]; then
  VERDICT="PASS"
else
  VERDICT="FAIL"
fi

ELAPSED="$(($(date +%s) - START))"
python3 - "$ART" "$VERDICT" "$FIRST_MISSING" "$ELAPSED" "$SCREEN_ERR" "$SCREEN_OK" "$WINDOWS_OK" "$GOLIVE_WINDOWS" <<'EOF'
import json, sys, time
art, verdict, first_missing, elapsed, screen_err, screen_ok, windows_ok, golive_windows = sys.argv[1:9]
try:
    n_windows = int(golive_windows.strip() or "0")
except ValueError:
    n_windows = 0
doc = {
    "verdict": verdict,
    "first_missing": first_missing or None,
    "elapsed_s": int(elapsed),
    "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    "screenshot_ok": screen_ok == "true",
    "screen_error": screen_err or None,
    "windows_list_ok": windows_ok == "true",
    "golive_windows_on_screen": n_windows,
    "paths": {
        "host_log": f"{art}/host.log",
        "viewer_log": f"{art}/viewer.log",
        "server_log": f"{art}/server.log",
        "host_status": f"{art}/host.json",
        "viewer_status": f"{art}/viewer.json",
        "screenshot": f"{art}/screen-final.png",
        "windows": f"{art}/windows.txt",
    },
}
with open(f"{art}/verdict.json", "w") as f:
    json.dump(doc, f, indent=2)
print(json.dumps(doc))
EOF

if [ "$VERDICT" = "PASS" ]; then
  echo "E2E-PASS in ${ELAPSED}s"
  exit 0
else
  echo "E2E-FAIL first-missing: $FIRST_MISSING" >&2
  exit 1
fi
