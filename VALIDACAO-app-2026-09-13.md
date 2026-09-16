# Auditoria automatizada do app — 13/09/2026

**Resultado: funcional aprovado localmente; fluidez com dois viewers ainda reprovada.** Não há validação nativa do Windows nesta execução.

Executado `python3 scripts/verify.py --desktop`. Relatório completo em `e2e-artifacts/verify-20260913-103327-608429/report.json`, com logs por etapa. O comando terminou com código **1**, preservando a reprovação da cadência. Nenhum limite foi relaxado nem o teste repetido até obter um PASS.

Após habilitar a exibição de skips nos logs, a suíte padrão `python3 scripts/verify.py` foi executada novamente e terminou com código **0**: `e2e-artifacts/verify-20260913-103852-202501/report.json`. Essa execução funcional não repete nem substitui o resultado de desempenho acima.

| Etapa | Resultado |
|---|---|
| Testes das ferramentas / analyzer | PASS — 6 testes / 14 autotestes |
| Servidor | PASS — todas as suítes configuradas |
| Web unitário / build | PASS — 101 testes e compilação de produção |
| Browser | PASS — 2 testes: navegação no mock e pixels/contexto do renderer real |
| Core | PASS — 92 unitários; binário de integração também passou (fixtures opcionais sem configuração não contam como cobertura) |
| Plataforma comum / gate WGC | PASS — 25 + 2 testes |
| Plataforma macOS | PASS — 14 testes |
| App | PASS — 106 testes de biblioteca, 4 smoke/integrados, testes de helper/contrato; 1 fixture interativa ignorada |
| Build desktop / geração de fonte | PASS |
| App real, 1 viewer 1080p60 | PASS — gap máximo 43,746 ms |
| App real, 2 viewers 1080p60 | FAIL — gaps 57,507 / 58,456 ms; limite 50 ms |

No ensaio de dois viewers, apresentação média de 59,881 e 59,901 FPS, zero erros/descartes registrados e um encoder compartilhado. A falha foi exclusivamente o intervalo entre apresentações. Essas medições são ACKs do desenho, não scanout, e não comprovam rede remota ou captura/áudio em máquinas Windows.

## Problemas encontrados e corrigidos nesta auditoria

O smoke test do app não compilava: seu `E2ePlan` não preenchia o campo `quality`. Isso passou despercebido porque a validação anterior executava app-lib. Corrigido, executado e incluído na suíte padrão. Um novo teste integrado usa o servidor e o shell reais para verificar dois espectadores, saída do primeiro, continuidade do segundo após reconfiguração, unwatch/rewatch e ausência de links órfãos.

O harness de desempenho agora reprova traces curtos/incompletos, erros, descartes, ausência de WebGL e duplicação do encoder, além de FPS baixo e pausas. Seus testes de regressão incluem média de 60 FPS com gap de 98 ms. O renderer tem teste automatizado de pixels em Chromium, reutilizando a fixture antes executada manualmente no WebKit. Chromium headless pode usar software para WebGL: esse teste valida correção gráfica, não capacidade de hardware.

## Terminal no Windows

Os dois executáveis Windows antigos disponíveis localmente reprovaram no teste PE: `subsystem=3` (console). O app principal e o helper agora declaram `windows_subsystem="windows"`. O verificador `scripts/check-windows-gui.py` exige GUI=2 no executável final; foi ligado ao workflow de release antes do upload e à suíte nativa Windows.

A checagem cruzada ainda falha na dependência Opus/SSE4.1; uma tentativa com flag apenas de processo também não superou o problema. Nenhuma flag permanente/dependência de codec foi alterada. **Não foi produzido nem executado um novo `.exe` Windows nesta auditoria.** BUG-005 permanece aberto até a validação nativa. O CI Windows foi configurado, mas não executado remotamente nesta sessão.

Preparação, comandos e limites da suíte reutilizável: [TESTING.md](TESTING.md).
