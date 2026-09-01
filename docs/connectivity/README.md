# Connectivity — pacote para implementação

Ler **nesta ordem**. Não implementar sem ler o pacote inteiro.

1. [`/CONTEXT.md`](../../CONTEXT.md) — vocabulário. Usar estes nomes no código e na UI.
2. [`/docs/adr/`](../adr/) — decisões que não se reabre no meio do PR.
3. [`PRD.md`](./PRD.md) — o que tem de existir, testável.
4. [`SPEC.md`](./SPEC.md) — como encaixa no app atual.
5. [`PROTOCOL.md`](./PROTOCOL.md) — bytes na rede.
6. [`SERVER.md`](./SERVER.md) — o Rendezvous Node.js (mínimo).
7. [`SECURITY.md`](./SECURITY.md) — regras que não se “simplifica”.
8. [`UI.md`](./UI.md) — ecrãs e campos.
9. [`IMPLEMENTATION.md`](./IMPLEMENTATION.md) — ordem de PRs / tarefas.

Código atual relevante (não partir):

- `src-tauri/src/media/room.rs` — LAN UDP 17424 + TCP offer/answer, um Viewer
- `src-tauri/src/media/peer_transport.rs` — webrtc-rs, `ice_servers` vazio
- `src-tauri/src/media/engine.rs` — um peer, poll do answer
- `src/App.tsx` — Host Start / Viewer Join por Room code

Invariantes:

- Viewer continua **browser** `RTCPeerConnection`. Host continua **webrtc-rs**.
- Media nunca atravessa o Rendezvous.
- LAN com Join mode `lan` tem de continuar a funcionar sem internet.
- Sem TURN nesta versão.
