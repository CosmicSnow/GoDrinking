#!/bin/bash
# Plays a looping 440 Hz tone so audio exclusion can be verified:
# start a screen share, exclude "afplay", confirm the viewer no longer hears it.
set -euo pipefail
WAV="$(mktemp -t golive-tone).wav"
python3 - "$WAV" <<'PY'
import math, struct, sys, wave
path = sys.argv[1]
rate = 44100
with wave.open(path, "w") as out:
    out.setnchannels(1)
    out.setsampwidth(2)
    out.setframerate(rate)
    for i in range(rate * 2):
        sample = int(math.sin(2 * math.pi * 440 * i / rate) * 16000)
        out.writeframes(struct.pack("<h", sample))
PY
echo "playing $WAV (Ctrl-C to stop)"
while true; do
  afplay "$WAV"
done
