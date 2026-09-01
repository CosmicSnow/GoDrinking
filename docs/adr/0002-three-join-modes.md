# Três Join modes, um pipeline de Media

Há três formas de se encontrarem. A captura, o encode e o WebRTC de Media são os mesmos.

**Decisão:** `lan` | `direct` | `stunar` escolhido na UI antes do Start/Join.

- **LAN** — Room code + broadcast UDP. ICE host-only. Não depende de internet. É o comportamento atual.
- **Direct** — Viewer escreve IP+porta. Host mostra IPv4, IPv6 e porta. ICE com STUN. Password no Host. Ignore list no Host.
- **Stunar** — Room code (+ Password) via Rendezvous HTTPS. ICE com STUN. O Rendezvous não vê Media.

Misturar modos na mesma Session é proibido. Host e Viewer têm de estar no mesmo Join mode.
