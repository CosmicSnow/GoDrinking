# changelog

## v0.7.9

- feat: player volume can boost to 200%; double-click resets to 100%
- fix: open update downloads in the OS browser, limited to this repository's GitHub release URLs

## v0.7.8

- feat: persist "seu nick" and "servidor" on the user machine (localStorage), restored on next open
- feat: "ignorar áudio de apps" blocks discord and any discord audio source by default (user can untoggle during the stream)
- feat: "ignorar áudio de apps" blocks goDrinking (app + helper) own audio by default (user can untoggle during the stream)
- ci: remove application regression tests workflow (verified locally via `python3 scripts/verify.py`)
