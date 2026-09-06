# Visual E2E — Sala Watch (LOCAL-ONLY)

Two packaged goDrinking instances on one machine: host opens a Stunar room and
shares, joiner joins, asserts **Watch enabled → click → live video slot +
first-frame milestone**. Screenshots and console evidence land in
`e2e/artifacts/`.

> **Never runs in CI.** Root `package.json` has no script that reaches `e2e/`,
> and `e2e/run.mjs` refuses to start unless `GODRINKING_E2E=1` is set and no CI
> env is detected. This suite needs a display, TCC-granted capture, and two
> installed app copies — none of which CI provides.

## Prerequisites

1. **Packaged `.app`, never `tauri dev`.** Screen Recording (TCC) behavior is
   only representative from the bundle:
   `tauri build` (or `src-tauri/build-debug-app.sh`), then grant
   **System Settings → Privacy & Security → Screen Recording** for
   `goDrinking.app` and **reopen the app**. Until first grant, capture reads
   fail exactly like production.
2. **Two signed instances.** Copy the bundle so host and joiner have separate
   identities/data:
   `cp -R goDrinking.app goDrinking-joiner.app` (re-sign if your flow
   requires: `codesign --force --deep --sign -`). If the build enforces
   single-instance, the second launch focuses the first — that is a product
   bug for this suite, not a config issue; fall back to two user accounts or
   two Macs with the same steps.
3. **A reachable Stunar rendezvous** both instances can dial (LAN IP or public
   URL). Same-PC loopback notes:
   - Prefer the LAN address over `localhost` so ICE host candidates match what
     a second machine would see.
   - The joiner window is *on the shared screen*: keep it minimized or on
     another Space after clicking Watch, or the screenshots recurse.
   - macOS firewall prompts appear per app copy — allow both.
   - Close other capture consumers (QuickTime, Teams) so the host gets the
     real display, not a black frame.
4. **tauri-driver** on `localhost:4444` (`tauri-driver` binary in PATH, run it
   in its own terminal).
5. `cd e2e && npm install` (WebDriverIO harness, local only).

## Run

```sh
cd e2e
GODRINKING_E2E=1 \
GODRINKING_HOST_APP="/path/to/goDrinking.app" \
GODRINKING_JOINER_APP="/path/to/goDrinking-joiner.app" \
GODRINKING_RENDEZVOUS="https://rendezvous.example.com" \
GODRINKING_PASSWORD="visual-e2e-1" \
npm test
```

| Env | Meaning |
| --- | --- |
| `GODRINKING_E2E` | Must be `1`; the gate refuses anything else. |
| `GODRINKING_HOST_APP` | Packaged app (or binary) the host session drives. |
| `GODRINKING_JOINER_APP` | Second copy the joiner session drives. |
| `GODRINKING_RENDEZVOUS` | Stunar URL typed into both instances. |
| `GODRINKING_PASSWORD` | Room password (default `visual-e2e-1`). |

## What the spec asserts

`e2e/specs/sala-watch.e2e.mjs` (multiremote `host` + `joiner`):

1. Host: Share tab → Stunar + Room → Open room → reads the 6-char code →
   **Share my screen**.
2. Joiner: Watch tab → Stunar URL + code + nickname + password → Join →
   `.room-watch` exists **and is enabled** → click.
3. Joiner: a `video[data-slot]` reaches `readyState >= 2` within 90 s.
4. Screenshots at every step (`01-host-room-form … 06-joiner-live`).
5. Joiner console saved to `artifacts/joiner-console.log` and must contain a
   `viewer-milestone … ontrack-fired` line.

## Offer → answer → IDR log grep (manual, after a run)

The spec covers the viewer side. For the full signaling chain, grep the host
engine log (Console.app for the bundle, or stdout when launched from a
terminal) for the opaque attempt echo and encoder milestones:

```sh
# attempt echo ties offer → answer together; then first valid IDR + first RTP
grep -E "offer_attempt|answer|IDR|first.*(rtp|packet)" <host-log> | tail -40
```

Expected shape: `offer … attempt=<id>` → `answer … attempt=<id>` (same id,
verbatim) → first valid IDR with parameters → first RTP sent. If the viewer
has packets but no decoded frames, suspect codec/packetization, not the path
(see the failure-classification table in the reliability docs).

## What stays manual

- TCC grant / re-open dance and first-run permission prompts.
- Eyeballing color, text sharpness, ultrawide shape, 720p30/1080p30/1080p60.
- System-audio mix and per-app exclusion.
- Windows Host/Viewer pairings and forced OpenH264 fallback.
- Late join, reconnect, capture restart, PLI/FIR, repeated Start/Stop.
- Anything on the Morgan Stanley box: external-browser compat is out of scope.
