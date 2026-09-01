# Rendezvous nunca vê Media

O servidor existe só para o modo Stunar apresentar Host e Viewer. Se ele reencaminhasse vídeo, deixaríamos de ser P2P e o VPS virava custo e risco.

**Decisão:** o Rendezvous guarda metadados de sala (código com hash, hash da Password, Heartbeat, Roster de nicknames, Tokens) e reencaminha blobs de Signaling **depois** de Password + Admission. Recusa qualquer payload que não seja JSON de sinalização com teto de tamanho. Sem TURN nesta versão.

Alguns NATs não furam. Esses pares falham. Documentado. Não se “resolve” com relay no Rendezvous.
