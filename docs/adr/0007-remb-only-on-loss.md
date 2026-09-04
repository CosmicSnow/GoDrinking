# REMB só desce com perda

O WebView manda REMB pessimista (centenas de kbps) em caminhos saudáveis. O encoder seguia o piso de 1 Mbps e o alvo High de 8 Mbps era teatro.

**Decisão:** REMB só baixa o encoder se o RTCP Receiver Report mostrou perda recente. Sem perda, o alvo (preset ou Customize) manda. O piso automático é ¼ do alvo. Probe sobe quando o caminho está limpo.

Alternativa rejeitada: desligar REMB — uma LAN má ou Wi-Fi cheio precisava de descida.
