# changelog

## v0.7.15

- feat: linux screen share through the desktop portal, with per-app PipeWire audio

## v0.7.14

- fix: windows share shows the mouse cursor on display capture
- fix: windows window share no longer draws the colored capture border
- fix: windows share captures each heard app in isolation (experimental); muted apps are never captured, so they cannot leak back choppy
- fix: windows share mix no longer stalls when an app goes quiet, removing the random audio pop

## v0.7.12

- fix: muted apps are left out of the windows share entirely; the rest of the audio stays normal

## v0.7.11

- fix: windows share audio is one continuous stream again (no chopped mix)

## v0.7.10

- fix: windows screen share no longer echoes when both peers share, and discord is not mixed back in
- fix: windows display share shows the mouse cursor
- fix: windows window share no longer draws the capture border
- fix: "ignorar áudio de apps" shows which apps are playing sound

## v0.7.9

- feat: player volume can boost to 200%; double-click resets to 100%
- fix: open update downloads in the OS browser, limited to this repository's GitHub release URLs

## v0.7.8

- feat: persist "seu nick" and "servidor" on the user machine (localStorage), restored on next open
- feat: "ignorar áudio de apps" blocks discord and any discord audio source by default (user can untoggle during the stream)
- feat: "ignorar áudio de apps" blocks goDrinking (app + helper) own audio by default (user can untoggle during the stream)
- ci: remove application regression tests workflow (verified locally via `python3 scripts/verify.py`)
