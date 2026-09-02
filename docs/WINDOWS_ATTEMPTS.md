# Tentativas Windows — goDrinking

> Para análise por IA mais poderosa. Stunar/Direct/LAN + WGC + OpenH264 no Windows 5120x1440 ultrawide (5800X3D + RTX 3070, 60Hz, segundo monitor 1920x1080 funciona).

## Conexão (fixes válidos — commit separado)
- **Stunar 404 `denied — wrong code or password`**: `base=https://together.jouymaker.com/` com `/` virava `//v1` → `404`. Fix: `normalize_base()` em `rendezvous.rs` (`host_open`, `post_heartbeat/decide/rotate/close`, `discover_stunar_room`, `ws_url`) + default `https://together.jouymaker.com` em `src/App.tsx` e placeholders.
- **Direct IPv6 timeout `[2804:...]:port`**: `bind_direct_listener()` no win era `v6-only`, caía para `0.0.0.0:0` (sem IPv6). Fix: `socket2` dual-stack `only_v6(false)` em `room.rs` + `address_worker` + `Test-NetConnection` vs `ping`.
- **Firewall Win**: `Direct/LAN` precisam `TCP inbound` (Stunar não). `firewall.rs` com `needs_firewall()`, `check_firewall_status()` e `reset_firewall_rules()` (best-effort, sem exigir Admin). `engine.rs` chama `ensure_firewall_for_host` só para `Direct/LAN` em thread separada (antes bloqueava `create_in_state` 1-5s). UI com `Reset/Check` foi removida a pedido (sem Admin).
- **LAN/Direct `GOLIVE2`**: `handle_tcp` + `fetch_offer` + `discover_direct` — sem mudanças, apenas firewall acima.
- **Outros**: `Cargo.toml` `socket2=0.5` para dual-stack.

## Streaming — todas as tentativas falharam (preview nunca funcionou, app inteiro congela em 1-5s após Start, mesmo janela pequena 1280x720, em High tem que funcionar)
- **Sintoma**: `Start native session` → 1-5s degradando → app inteiro congela / mata o PC (não só stream). `Tela inteira no segundo monitor 1920x1080` não morre, mas `qualquer janela` morre. `5120x1440` sempre morre.
- **Tentativa 1 — `canStart`**: `App.tsx:154` só liberava se `screen_recording_authorization === "granted"` (macOS). No win é `unsupported`. Fix `canStart = supported && native_capture_implemented` + `permissionLabel` + `permission-dot` + `startSharing` check. Botão liberou, mas `Grant Screen Recording...` ainda bloqueava no `startSharing`.
- **Tentativa 2 — `startSharing`**: mesma checagem `authorization !== "granted"` bloqueava. Fix para `platform !== "windows"`.
- **Tentativa 3 — `OpenH264 max 3840x2160`**: `5120x1440` estourava `Encoder max resolution`. Log `OpenH264 encode skipped` em loop e `Backtraces enabled`. Fix `windows_encoder.rs` com `fit_within_encoder_max()` + `downscale_bgra_nearest()` + log 1x.
- **Tentativa 4 — `WGC 29MB` por frame**: `windows_capture.rs` fazia `buffer.as_nopadding_buffer().to_vec()` de `5120x1440x4` a 60fps (1.7GB/s). Fix downscale **na captura** para `quality` (`High 1920x540`, `Low 1280x360`) antes de alocar `Arc`, com `fitted_even_size`.
- **Tentativa 5 — `skip_frames` + `REMB`**: `OpenH264` warnings `AdaptiveQuant/BackgroundDetection` + `compressed > half` + `REMB` subindo/descendo bitrate degradando. Fix `EncoderConfig::skip_frames(true)`, `adaptive_quantization(false)`, `background_detection(false)` + desabilitar `REMB` em `peer_transport.rs` (deixar bitrate fixo no preset) + `tokio worker_threads 2→4` no win.
- **Tentativa 6 — `fps`**: `High 60fps` pesado no ultrawide. Forçado `win` para `30fps` em `windows_capture.rs`.
- **Tentativa 7 — `WGC Window`**: `Window` via `WGC` congela o app inteiro no win (mesmo pequena), enquanto `Monitor` no segundo não congela. Workaround `resolve_target` para `Window` retornar `Monitor::primary()` (captura tela inteira) — ainda congelou.
- **Tentativa 8 — `WGC 24H2`**: `MinimumUpdateInterval::Custom` congela após 5-6s no `24H2` (`Win32CaptureSample#92`). Teste `Default` piorou (travou o PC inteiro), revertido para `Custom`.
- **Tentativa 9 — `WGC Frame` lifetime**: `E0597 buffer does not live long enough` + `unexpected closing delimiter` ao tentar downscale sem `to_vec()` intermediário. Fix com `buffer` vivo durante `as_nopadding_buffer`.
- **Estado atual**: `cargo run` compila com 10-11 warnings, mas `WGC frame 5120x1440 downscaled to 1920x540` ainda congela o app inteiro em 1-5s, em `High/Medium`, mesmo após todos os fixes acima. `Stunar Host` log só mostra `stunar open 200 OK` e `ws connected`, sem `WGC` depois (congela antes de logar). Preview nunca funcionou.

## Dados para IA
- **HW**: `5800X3D + RTX 3070`, `5120x1440 60Hz` ultrawide + segundo `1920x1080`. `Tela inteira` no segundo não morre, `qualquer janela` morre. `High` tem que funcionar com qualidade.
- **Logs**: `session-*-host-stunar.log` só tem `stunar open`, sem `WGC` após freeze. Console tem `WGC frame ... downscaled`, `OpenH264 Warning:ParamValidation`, depois congela. `cargo` warnings `linker_messages` + `unused_imports`.
- **Reprodução**: `Share → Window ou Screen → High/Medium/Low → Start` no `win` ultrawide → 1-5s degradando → app congela / `PC` afeta. No `mac` tudo ok. No segundo monitor `Screen` não congela, `Window` congela.
- **O que falta**: `Stunar` funciona sem firewall, `Direct` precisa `TCP inbound` mas `WGC` mata antes de chegar no `webrtc`. Preview nunca capturou de verdade.
