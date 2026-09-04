# SPEC — Play Together (futuro)

Data: 2026-09-04  
[PRD](./PRD.md). **Não implementar agora.**

## Seams (quando for TDD)

| Seam | Observável |
|------|------------|
| `PadMap` XInput 16-bit | A/B/X/Y, dpad, sticks, triggers 0–255 |
| Dead-man's timer | 200 ms sem pacote → estado neutro |
| Autorização | Viewer sem grant não produz inject |
| Roster pad flag | `has_pad` + `authorized` |

## Arquitectura

```
Viewer pad OS → sample 125 Hz → DataChannel "pad" → Host → virtual XInput
Host video  → already P2P     → Viewer
Rendezvous  → Signaling only (e um bit no Roster: pad-present)
```

O DataChannel nasce no mesmo `RTCPeerConnection` / `webrtc-rs` da Media. Label `godrinking-pad`. Ordered, unreliable (ou reliable com mensagens < 64 B — decidir na implementação com um teste de jitter). Payload binário, não JSON.

## Pacote (16 bytes, little-endian)

```
u8  version = 1
u8  buttons_lo   // A B X Y LB RB Back Start
u8  buttons_hi   // LS RS dpad U D L R
u8  left_trigger
u8  right_trigger
i16 lx, ly, rx, ry   // -32768..32767
u8  viewer_slot      // 0..3
u8  seq
```

Zona morta no Viewer: sticks |v| < 0.12 não saem do zero (evita piscar o ícone).

## Virtual device

**Windows Host:** ViGEmBus (Xbox 360 target). Fallback documentado se o driver não estiver: a aba Comando diz “instala o driver de pad virtual” com link. Sem silent fail.

**macOS Host:** virtual HID gamepad via DriverKit ou um user-client se existir algo estável na data. Se não houver caminho assinável no Tauri, Play Together no Mac Host fica “unsupported” e a UI diz isso. Não fingir.

**Slots:** 4. O Host atribui slot no grant. Kick liberta o slot.

## UI

- Sidebar: ícone settings (já existe no set) abre overlay no mesmo estilo de logs (`logs-overlay`).
- Abas: **Geral** (Join mode default, Nickname) | **Comando** | **Sobre**.
- Aba Comando (Viewer): lista, Testar, rumble, “pedido enviado”.
- Host: toggle “Aceitar comandos neste PC”; por Viewer, Aceitar / Revogar; ícone pad no Roster; piscadela CSS 150 ms (`pad-blink`).

Não desenhar um HUD de jogo. É goDrinking, não um launcher.

## Segurança

- Grant é por Session. Nova Session → zero grants.
- O Rendezvous nunca recebe o pacote de 16 bytes.
- Ignore list / Password / Admission aplicam-se antes de existir DataChannel.
- Sem inject se o Share slot daquele Host não estiver Live (não mandar pad para um desktop desbloqueado sem o dono estar a partilhar — ou exigir o toggle + share activo; preferir **os dois**).

## Riscos

- Anti-cheat (Easy Anti-Cheat, BattlEye) pode recusar o pad virtual. Fora de âmbito.
- NAT simétrico: se o vídeo já falhou, o pad também. Sem TURN.
- macOS como Host de jogos com pad: compatibilidade pobre. Tratar Windows Host como o caminho “Parsec-like”.

## Plano de PRs (quando for altura)

1. DataChannel + pacote + testes de parse/dead-man (sem driver).
2. Viewer: leitura XInput/GC + aba Testar.
3. Host Windows: ViGEm + grant UI.
4. Roster ícone + blink.
5. macOS Host: só se 3 estiver sólido e existir driver assinável.
