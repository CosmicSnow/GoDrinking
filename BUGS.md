# Bugs — goDrinking

> Regra: bug 100% corrigido **e verificado** SAI desta lista (ver
> AGENTS.md, seção Bugs). Nunca marcar como feito no lugar — remover a
> linha. A lista contém só bugs abertos.

## Reprodução intermitente: transmissões param de ser assistidas

**Status:** em investigação, causa não confirmada. Participantes relatam que
transmissões de pessoas diferentes podem congelar em computadores diferentes
enquanto o compartilhamento parece continuar; parar de assistir e voltar a
assistir às vezes recupera a imagem. Não há traço do momento da falha.
Diagnósticos agregados do servidor estão preparados localmente, mas não foram
publicados; o servidor só observa sinalização, não o fluxo P2P. Correlacionar
uma próxima ocorrência com estado ICE, quadros enviados/recebidos e eventos
de desconexão antes de escolher uma correção.
