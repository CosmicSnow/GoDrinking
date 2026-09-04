# Cap de captura por orçamento de pixels

Ultrawide 3440×1440 dentro de uma caixa 1920×1080 virava 1920×804 e o High “1080p” desperdiçava ~25% do nível H.264 4.2.

**Decisão:** o teto de um preset é a *área* (ex. 1920×1080 pixels), não um letterbox 16:9. Aspecto preservado, sem upscale, dimensões pares. Presets continuam dentro do Baseline 4.2.

Alternativa rejeitada: cap por altura (1080 de alto, largura livre) — 2560×1080 rebenta MaxFS 8704.
