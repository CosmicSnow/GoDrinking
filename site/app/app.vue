<template>
  <div>
    <a href="#conteudo" class="skip-link">Pular para conteúdo</a>

    <header class="site-header">
      <div class="wrap">
        <a href="#" class="brand" aria-label="goDrinking — início">
          <img src="/logo.png" alt="" width="32" height="32" />
          goDrinking
        </a>
        <nav class="nav" aria-label="Navegação principal">
          <a href="#o-que-e">O que é</a>
          <a href="#como-funciona">Como funciona</a>
          <a href="#modos">Modos</a>
          <a href="#manifesto">Manifesto</a>
          <a href="#faq">FAQ</a>
          <a href="#download" class="btn btn-primary">Baixar</a>
        </nav>
      </div>
    </header>

    <main id="conteudo">
      <!-- Ticker -->
      <div class="ticker" aria-hidden="true">
        <div class="ticker-track">
          <span class="ticker-item">Não é um app de bar</span>
          <span class="ticker-item">Não é Golang</span>
          <span class="ticker-item">Ninguém aqui está "going drinking"</span>
          <span class="ticker-item">Discord de pobre</span>
          <span class="ticker-item">Sem datacenter na Virginia</span>
          <span class="ticker-item">Rage against the machine</span>
          <span class="ticker-item">A luta continua</span>
          <span class="ticker-item">Sous les pavés, la plage</span>
          <span class="ticker-item">Não é um app de bar</span>
          <span class="ticker-item">Não é Golang</span>
          <span class="ticker-item">Ninguém aqui está "going drinking"</span>
          <span class="ticker-item">Discord de pobre</span>
          <span class="ticker-item">Sem datacenter na Virginia</span>
          <span class="ticker-item">Rage against the machine</span>
          <span class="ticker-item">A luta continua</span>
          <span class="ticker-item">Sous les pavés, la plage</span>
        </div>
      </div>

      <!-- Hero -->
      <section class="hero">
        <div class="wrap">
          <div class="hero-grid">
            <div>
              <div class="hero-kicker">
                <span class="dot" aria-hidden="true"></span>
                Screen share P2P para macOS
              </div>
              <h1 class="hero-title font-display">
                Divide a tela.<br />
                Não os dados<span class="cursor" aria-hidden="true"></span>
              </h1>
              <p class="hero-sub">
                Dois Macs, uma rede, nenhum servidor no meio. O Host captura. O Viewer assiste. A mídia viaja direto — sem nuvem, sem STUN público, sem a sua tela passando por um datacenter em Virginia.
              </p>
              <div class="hero-actions">
                <a href="#download" class="btn btn-primary btn-large">
                  Baixar para macOS
                </a>
                <a href="#como-funciona" class="btn btn-large">
                  Ver como funciona
                </a>
              </div>
              <div class="hero-meta">
                <span>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>
                  P2P criptografado
                </span>
                <span>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg>
                  Sem login, sem conta
                </span>
                <span>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>
                  macOS only (por enquanto)
                </span>
              </div>
            </div>

            <div class="mock-window" role="img" aria-label="Demonstração da janela do Host com código de 6 caracteres">
              <div class="mock-titlebar">
                <span class="mock-dot"></span>
                <span class="mock-dot"></span>
                <span class="mock-dot"></span>
                <span class="mock-title">goDrinking — Host</span>
              </div>
              <div class="mock-body">
                <div class="mock-row">
                  <div>
                    <div class="mock-label">Room code</div>
                    <div class="mock-code" aria-live="polite">{{ demoCode }}</div>
                  </div>
                  <button class="mock-button" @click="copyCode" type="button">
                    {{ copied ? 'Copiado!' : 'Copiar código' }}
                  </button>
                </div>
                <div class="mock-status">
                  Session ativa · 1 Viewer no Roster
                </div>
                <pre class="mock-ascii" aria-hidden="true">HOST  ───────┐
             │  WebRTC P2P
VIEWER ──────┘  (sem nuvem)</pre>
              </div>
            </div>
          </div>
        </div>
      </section>

      <!-- O que é / O que não é -->
      <section id="o-que-e" class="section">
        <div class="wrap">
          <div class="section-label">Guia de campo</div>
          <h2 class="section-title font-display">
            Não é Go. Não é bar. Não é Discord.
          </h2>
          <div class="def-grid">
            <div class="def-box">
              <h3>É sim</h3>
              <ul class="def-list">
                <li>Screen share P2P entre um Host e um ou mais Viewers.</li>
                <li>Código de 6 caracteres para entrar na Session.</li>
                <li>Captura nativa do macOS com ScreenCaptureKit + VideoToolbox.</li>
                <li>Áudio de sistema opcional, com exclude list para apps sensíveis.</li>
                <li>Quality presets: Low 720p30, Medium 1080p30, High 1080p60.</li>
                <li>Controle de Admission e Roster para quem entra.</li>
              </ul>
            </div>
            <div class="def-box">
              <h3>É não</h3>
              <ul class="def-list">
                <li>Não é escrito em Go. O nome é uma piada que fugiu do controle.</li>
                <li>Não é tracker de shots, receitas de drink ou app de bar.</li>
                <li>Não é Discord: não há servidor processando a sua tela.</li>
                <li>Não é "sobe um TURN e resolve". Fora de âmbito.</li>
                <li>Não funciona no Windows. Ainda é um comentário sarcástico no futuro.</li>
                <li>Não é Open Source OSI. Licença PolyForm Noncommercial.</li>
              </ul>
            </div>
          </div>
        </div>
      </section>

      <!-- Como funciona -->
      <section id="como-funciona" class="section">
        <div class="wrap">
          <div class="section-label">Fluxo</div>
          <h2 class="section-title font-display">Três passos. Nenhuma nuvem.</h2>
          <p class="section-intro">
            O Host inicia uma Session, escolhe o que compartilhar e recebe um Room code. O Viewer usa o código, o Host aceita se Admission estiver ligada, e a mídia liga direto entre os dois.
          </p>

          <div class="steps">
            <article class="step">
              <h3>Host clica em Share</h3>
              <p>Escolhe tela inteira ou janela, define a qualidade e decide se inclui áudio de sistema. Aí dá Start.</p>
            </article>
            <article class="step">
              <h3>Copia o Room code</h3>
              <p>Um código de 6 caracteres aparece. Manda no WhatsApp da família. Ou grita pela casa. Funciona dos dois jeitos.</p>
            </article>
            <article class="step">
              <h3>Viewer cola e entra</h3>
              <p>Com o Room code, o Viewer pede acesso. Se Admission estiver ligada, o Host aceita. A conexão P2P liga sozinha.</p>
            </article>
          </div>

          <div class="diagram" aria-label="Diagrama simplificado da conexão">
            <div class="diagram-row">
              <div class="diagram-node">
                Host
                <small>ScreenCaptureKit · H.264</small>
              </div>
              <div class="diagram-arrow">
                <span>→</span>
                Signaling
                <span>(offer/answer)</span>
              </div>
              <div class="diagram-node">
                Viewer
                <small>WKWebView · WebRTC</small>
              </div>
            </div>
          </div>
          <p class="mt-md" style="color: var(--gray); font-size: 0.9rem;">
            A mídia nunca passa pelo Rendezvous. Ele só apresenta os lados. Depois disso, os pacotes viajam direto entre Host e Viewer.
          </p>
        </div>
      </section>

      <!-- Join modes -->
      <section id="modos" class="section">
        <div class="wrap">
          <div class="section-label">Join modes</div>
          <h2 class="section-title font-display">Três jeitos de encontrar o Host.</h2>
          <p class="section-intro">
            Dependendo de onde você está, escolhe como o Viewer encontra a Session: LAN, Direct ou Stunar. A terminologia importa — leia com atenção.
          </p>

          <div class="cards-grid">
            <article class="card">
              <div class="card-icon">LAN</div>
              <h3>LAN</h3>
              <p>Viewer usa o Room code e descobre o Host por broadcast na rede local. É o modo clássico: funciona na mesma Wi-Fi, sem internet.</p>
              <small>Quando usar: mesma rede, roteador não é um tijolo.</small>
            </article>
            <article class="card">
              <div class="card-icon">DIR</div>
              <h3>Direct</h3>
              <p>Viewer liga direto ao endereço IP:porta que o Host mostra. Sem broadcast, sem Rendezvous. Só o endereço e, se houver, a Password.</p>
              <small>Quando usar: você sabe o IP e o firewall deixa passar.</small>
            </article>
            <article class="card">
              <div class="card-icon">STN</div>
              <h3>Stunar</h3>
              <p>Host e Viewer falam com o Rendezvous por HTTPS usando Room code (e Password, se houver). O Rendezvous só apresenta os lados. Não é STUN.</p>
              <small>Quando usar: redes diferentes, com um Rendezvous próprio.</small>
            </article>
          </div>
        </div>
      </section>

      <!-- Features -->
      <section id="features" class="section">
        <div class="wrap">
          <div class="section-label">O que tem de verdade</div>
          <h2 class="section-title font-display">Features que não são marketing.</h2>

          <div class="features-list">
            <div class="feature">
              <span class="feature-tag">Quality presets</span>
              <h3>Low / Medium / High</h3>
              <p>720p30, 1080p30 e 1080p60. Encode H.264 Baseline no VideoToolbox. Você escolhe se quer economizar banda ou mostrar cada pixel.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">System audio</span>
              <h3>Áudio com exclude list</h3>
              <p>Compartilha o som do sistema, mas tira Discord, Spotify ou aquele Zoom do seu pai do mix. Se o Core Audio acordar do lado certo da cama.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Admission</span>
              <h3>Roster e controle de entrada</h3>
              <p>Com Admission ligada, o Host aceita cada Viewer antes da sinalização. O Roster mostra quem pediu e quem já está dentro.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Ignore list</span>
              <h3>Resfriamento automático</h3>
              <p>IPs que falham repetidamente vão para a Ignore list por um tempo. Não é banimento, é uma pausa para pensar na vida.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Heartbeat</span>
              <h3>Sala viva enquanto durar</h3>
              <p>No Stunar, o Host manda um Heartbeat para o Rendezvous. Sem sinal por 5 minutos, a sala some. Como deve ser.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Token</span>
              <h3>Password substituída na conversa</h3>
              <p>Depois que o Viewer é aceito, o Rendezvous entrega um Token opaco. A Password fica para trás. A conversa continua segura.</p>
            </div>
          </div>
        </div>
      </section>

      <!-- Manifesto -->
      <section id="manifesto" class="section manifesto">
        <div class="wrap">
          <div class="section-label">Manifesto</div>
          <h2 class="section-title font-display">Por que sem servidor?</h2>
          <p class="section-intro">
            Porque "gratuito" na internet geralmente significa "a gente vende seus metadados". O goDrinking não coleta, não processa e não enxerga a sua tela. A mídia sai do seu Mac e entra no de quem você escolheu.
          </p>

          <div class="manifesto-grid">
            <div class="manifesto-item">
              <h3>Privacidade real</h3>
              <p>A tela não passa por Virginia, por Frankfurt ou por lugar nenhum. Ela passa pelo ar da sua casa.</p>
            </div>
            <div class="manifesto-item">
              <h3>Sem conta</h3>
              <p>Não precisa de e-mail, telefone, nickname único ou senha eterna. Só um Nickname qualquer para o Roster.</p>
            </div>
            <div class="manifesto-item">
              <h3>Código aberto, uso honesto</h3>
              <p>Código visível sob PolyForm Noncommercial. Pessoal, hobby e organizações não-comerciais: ok. Empresa querendo lucrar: fala com a gente.</p>
            </div>
          </div>

          <div class="mt-lg" style="display:flex;gap:0.75rem;flex-wrap:wrap;align-items:center;">
            <span class="sticker sticker-lime">Rage against the machine</span>
            <span class="sticker">A luta continua</span>
            <span class="sticker sticker-ink">Sous les pavés, la plage</span>
          </div>
        </div>
      </section>

      <!-- Download -->
      <section id="download" class="section">
        <div class="wrap">
          <div class="download-box">
            <div class="section-label">Download</div>
            <h2 class="section-title font-display">Instala no seu Mac. Não no servidor de ninguém.</h2>
            <a href="#" class="btn btn-primary btn-large" target="_blank" rel="noopener noreferrer">
              Baixar para macOS
            </a>
            <div class="requirements">
              <span>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
                macOS 14.2+
              </span>
              <span>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>
                Permissão de Screen Recording
              </span>
              <span>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="16" y1="13" x2="8" y2="13"/><line x1="16" y1="17" x2="8" y2="17"/></svg>
                Licença PolyForm Noncommercial
              </span>
            </div>
            <p class="download-note">
              <a href="#" target="_blank" rel="noopener noreferrer">Código no GitHub</a>
              · build local com <code>npm install && npm run macos:app</code>
            </p>
          </div>
        </div>
      </section>

      <!-- FAQ -->
      <section id="faq" class="section">
        <div class="wrap">
          <div class="section-label">FAQ</div>
          <h2 class="section-title font-display">Perguntas que a gente já ouviu.</h2>

          <div class="faq-list">
            <details>
              <summary>Preciso de internet para usar?</summary>
              <div class="faq-answer">
                No modo LAN, não. Basta que Host e Viewer estejam na mesma rede local. No modo Stunar, os dois precisam alcançar o Rendezvous por HTTPS — mas a mídia ainda viaja direto entre eles.
              </div>
            </details>
            <details>
              <summary>Funciona no Windows? E no Linux?</summary>
              <div class="faq-answer">
                Não. A captura usa ScreenCaptureKit e VideoToolbox, que são específicos do macOS. Windows é, no momento, um comentário sarcástico no futuro.
              </div>
            </details>
            <details>
              <summary>Por que o nome é goDrinking?</summary>
              <div class="faq-answer">
                Em português a gente fala "vai tomando". Alguém traduziu para o inglês, ficou goDrinking, e agora o GitHub acha que somos um clube da cachaça escrito em Go. Somos nenhuma das duas coisas.
              </div>
            </details>
            <details>
              <summary>O Rendezvous vê a minha tela?</summary>
              <div class="faq-answer">
                Não. O Rendezvous só guarda salas vivas e troca recados de sinalização. Ele não vê, não guarda e não reencaminha mídia. A mídia é WebRTC P2P entre Host e Viewer.
              </div>
            </details>
            <details>
              <summary>Posso usar no trabalho?</summary>
              <div class="faq-answer">
                Depende. A licença PolyForm Noncommercial permite uso pessoal, hobby e organizações não-comerciais. Uso comercial exige autorização explícita. Não é Open Source OSI.
              </div>
            </details>
            <details>
              <summary>Quantos Viewers cabem numa Session?</summary>
              <div class="faq-answer">
                Dois ou mais, se a LAN aguentar o drama. No limite, é WebRTC mesh host-only: o Host manda para quem está no Roster.
              </div>
            </details>
          </div>
        </div>
      </section>
    </main>

    <!-- Footer -->
    <footer class="site-footer">
      <div class="wrap">
        <div class="footer-grid">
          <div>
            <div class="footer-brand">
              <img src="/logo.png" alt="" width="28" height="28" />
              goDrinking
            </div>
            <p class="footer-tagline">{{ randomTagline }}</p>
          </div>
          <div class="footer-col">
            <h4>Navegar</h4>
            <ul>
              <li><a href="#o-que-e">O que é</a></li>
              <li><a href="#como-funciona">Como funciona</a></li>
              <li><a href="#modos">Join modes</a></li>
              <li><a href="#features">Features</a></li>
              <li><a href="#manifesto">Manifesto</a></li>
            </ul>
          </div>
          <div class="footer-col">
            <h4>Links</h4>
            <ul>
              <li><a href="#download">Download</a></li>
              <li><a href="#" target="_blank" rel="noopener noreferrer">GitHub</a></li>
              <li><a href="#" target="_blank" rel="noopener noreferrer">Licença PolyForm</a></li>
              <li><a href="#faq">FAQ</a></li>
            </ul>
          </div>
        </div>
        <div class="footer-bottom">
          <span>© {{ new Date().getFullYear() }} goDrinking. Não é um app de bar.</span>
          <span>Licenciado sob PolyForm Noncommercial 1.0.0</span>
        </div>
      </div>
    </footer>
  </div>
</template>

<script setup>
const demoCode = ref('A3B9K7')
const copied = ref(false)

const taglines = [
  'Rage against the machine',
  'A luta continua',
  'Sous les pavés, la plage',
  'Não é Go. Não é bar.',
  'Discord de pobre',
  'Sua tela não passa por Virginia'
]

const randomTagline = taglines[Math.floor(Math.random() * taglines.length)]

function generateCode() {
  const chars = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789'
  let code = ''
  for (let i = 0; i < 6; i++) {
    code += chars.charAt(Math.floor(Math.random() * chars.length))
  }
  return code
}

async function copyCode() {
  demoCode.value = generateCode()
  try {
    await navigator.clipboard.writeText(demoCode.value)
    copied.value = true
    setTimeout(() => {
      copied.value = false
    }, 1600)
  } catch {
    // Fallback: apenas gera novo código
  }
}
</script>

<style scoped>
.skip-link {
  position: absolute;
  top: -40px;
  left: 0;
  background: var(--ink);
  color: var(--bg);
  padding: 0.5rem 1rem;
  z-index: 200;
  text-decoration: none;
  font-weight: 600;
}

.skip-link:focus {
  top: 0;
}
</style>
