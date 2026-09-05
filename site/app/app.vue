<template>
  <div>
    <a href="#conteudo" class="skip-link">Pular para conteúdo</a>

    <header class="site-header">
      <div class="wrap">
        <a href="#" class="brand" aria-label="goDrinking — início">
          <img src="/logo.png" alt="" width="32" height="32" />
          goDrinking
        </a>
        <div class="header-end">
          <nav class="nav" aria-label="Navegação principal">
            <a href="#o-que-e">O que é</a>
            <a href="#como-funciona">Como funciona</a>
            <a href="#modos">Modos</a>
            <a href="#faq">FAQ</a>
            <a href="#download" class="btn btn-primary">Baixar</a>
          </nav>
          <a
            class="github-mark"
            :href="githubUrl"
            target="_blank"
            rel="noopener noreferrer"
            aria-label="Código no GitHub"
            title="GitHub"
          >
            <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true">
              <path fill="currentColor" d="M12 .3a12 12 0 0 0-3.8 23.4c.6.1.8-.3.8-.6v-2.1c-3.3.7-4-1.6-4-1.6-.5-1.4-1.3-1.8-1.3-1.8-1.1-.7.1-.7.1-.7 1.2.1 1.8 1.2 1.8 1.2 1.1 1.8 2.8 1.3 3.5 1 .1-.8.4-1.3.8-1.6-2.7-.3-5.5-1.3-5.5-6 0-1.3.5-2.4 1.2-3.2-.1-.3-.5-1.5.1-3.2 0 0 1-.3 3.3 1.2a11.5 11.5 0 0 1 6 0c2.3-1.5 3.3-1.2 3.3-1.2.6 1.7.2 2.9.1 3.2.8.8 1.2 1.9 1.2 3.2 0 4.7-2.8 5.7-5.5 6 .4.4.8 1.1.8 2.2v3.2c0 .3.2.7.8.6A12 12 0 0 0 12 .3z"/>
            </svg>
          </a>
        </div>
      </div>
    </header>

    <main id="conteudo">
      <div class="ticker" aria-hidden="true">
        <div class="ticker-track">
          <span class="ticker-item" v-for="(item, i) in ticker" :key="i">{{ item }}</span>
        </div>
      </div>

      <section class="hero">
        <div class="wrap">
          <div class="hero-grid">
            <div>
              <div class="hero-kicker">
                <span class="dot" aria-hidden="true"></span>
                Screen share P2P · Windows e macOS
              </div>
              <h1 class="hero-title font-display">
                Divide a tela.<br />
                Não os dados<span class="cursor" aria-hidden="true"></span>
              </h1>
              <p class="hero-sub">
                Host captura. Viewer assiste. O vídeo vai direto de um PC para o outro. Sem nuvem, sem login, sem a tela passando por um datacenter.
              </p>
              <div class="hero-actions">
                <a :href="primaryHref" class="btn btn-primary btn-large" :download="primaryDownload" rel="noopener noreferrer">
                  {{ primaryLabel }}
                </a>
                <a href="#download" class="btn btn-large">Windows e macOS</a>
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
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
                  Windows 10/11 · macOS 14.2+
                </span>
              </div>
            </div>

            <div class="mock-window" role="img" aria-label="Janela do Host com código de sessão">
              <div class="mock-titlebar">
                <span class="mock-dot"></span>
                <span class="mock-dot"></span>
                <span class="mock-dot"></span>
                <span class="mock-title">goDrinking — Host</span>
              </div>
              <div class="mock-body">
                <div class="mock-row">
                  <div>
                    <div class="mock-label">Session code</div>
                    <div class="mock-code" aria-live="polite">{{ demoCode }}</div>
                  </div>
                  <button class="mock-button" @click="copyCode" type="button">
                    {{ copied ? 'Copiado!' : 'Copiar código' }}
                  </button>
                </div>
                <div class="mock-status">
                  P2P · Windows e macOS · Stunar só apresenta
                </div>
                <pre class="mock-ascii" aria-hidden="true">HOST  ───────┐
             │  WebRTC P2P
VIEWER ──────┘  (sem nuvem)</pre>
              </div>
            </div>
          </div>
        </div>
      </section>

      <section id="o-que-e" class="section">
        <div class="wrap">
          <div class="section-label">Guia de campo</div>
          <h2 class="section-title font-display">Screen share. Ponto a ponto.</h2>
          <div class="def-grid">
            <div class="def-box">
              <h3>É</h3>
              <ul class="def-list">
                <li>Broadcast (1 host, N viewers) ou Sala (todo mundo pode compartilhar).</li>
                <li>Windows 10/11 e macOS 14.2+.</li>
                <li>Código de 6 caracteres. LAN, Direct ou Stunar.</li>
                <li>H.264. Áudio de sistema opcional, com exclusão de apps.</li>
                <li>Licença PolyForm Noncommercial.</li>
              </ul>
            </div>
            <div class="def-box">
              <h3>Não é</h3>
              <ul class="def-list">
                <li>Não é Go. O nome é uma piada que saiu do controle.</li>
                <li>Não é app de bar nem tracker de drink.</li>
                <li>Não há servidor vendo ou reencaminhando o vídeo.</li>
                <li>Não é Open Source OSI. Usar no trabalho pode. Revender o código, não.</li>
              </ul>
            </div>
          </div>
        </div>
      </section>

      <section id="como-funciona" class="section">
        <div class="wrap">
          <div class="section-label">Fluxo</div>
          <h2 class="section-title font-display">Três passos. Nenhuma nuvem.</h2>
          <p class="section-intro">
            O Host abre a sessão e ganha um código. O Viewer entra com o código. O vídeo liga direto entre os dois.
          </p>
          <div class="steps">
            <article class="step">
              <h3>Host clica em Share</h3>
              <p>Tela ou janela, qualidade, áudio se quiser. Broadcast ou Sala. Start.</p>
            </article>
            <article class="step">
              <h3>Copia o código</h3>
              <p>Seis caracteres. Manda no chat. Ou grita pela casa.</p>
            </article>
            <article class="step">
              <h3>Viewer cola e entra</h3>
              <p>Se Admission estiver ligada, o Host aceita. A conexão P2P sobe sozinha.</p>
            </article>
          </div>
          <p class="mt-md" style="color: var(--gray); font-size: 0.9rem;">
            O Rendezvous (Stunar) só apresenta os lados. RTP não passa por ele.
          </p>
        </div>
      </section>

      <section id="modos" class="section">
        <div class="wrap">
          <div class="section-label">Join modes</div>
          <h2 class="section-title font-display">Três jeitos de se achar.</h2>
          <div class="cards-grid">
            <article class="card">
              <div class="card-icon">LAN</div>
              <h3>LAN</h3>
              <p>Mesma Wi-Fi. Código de 6 caracteres. Sem internet.</p>
            </article>
            <article class="card">
              <div class="card-icon">DIR</div>
              <h3>Direct</h3>
              <p>IP:porta que o Host mostra. Sem broadcast, sem Rendezvous.</p>
            </article>
            <article class="card">
              <div class="card-icon">STN</div>
              <h3>Stunar</h3>
              <p>Redes diferentes. O Rendezvous só troca recado. Não é STUN. Não vê o vídeo.</p>
            </article>
          </div>
        </div>
      </section>

      <section id="features" class="section">
        <div class="wrap">
          <div class="section-label">O que tem</div>
          <h2 class="section-title font-display">O que o app faz.</h2>
          <div class="features-list">
            <div class="feature">
              <span class="feature-tag">Broadcast / Sala</span>
              <h3>Um host, ou todo mundo</h3>
              <p>Broadcast: você compartilha, eles assistem. Sala: cada um escolhe quem assistir, grelha, pin, zoom.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Quality</span>
              <h3>Low / Medium / High</h3>
              <p>720p30, 1080p30, 1080p60. H.264. Ultrawide mantém o formato.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">System audio</span>
              <h3>Áudio com exclusão</h3>
              <p>Compartilha o som do PC e tira apps da mix, tipo o chat de voz.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Admission</span>
              <h3>Quem entra, entra</h3>
              <p>Admission ligada: o Host aceita cada Viewer. Roster mostra quem está dentro.</p>
            </div>
          </div>
        </div>
      </section>

      <section id="download" class="section">
        <div class="wrap">
          <div class="download-box">
            <div class="section-label">Download</div>
            <h2 class="section-title font-display">Windows e macOS. Instala no seu PC.</h2>
            <p class="section-intro" style="margin-bottom: 1.25rem;">
              {{ latestTag ? `Última versão: ${latestTag}` : 'Puxa a última release do GitHub.' }}
            </p>
            <div class="hero-actions">
              <a :href="primaryHref" class="btn btn-primary btn-large" :download="primaryDownload" rel="noopener noreferrer">
                {{ primaryLabel }}
              </a>
              <a v-if="secondaryHref" :href="secondaryHref" class="btn btn-large" :download="secondaryDownload" rel="noopener noreferrer">
                {{ secondaryLabel }}
              </a>
            </div>
            <div class="requirements">
              <span>Windows 10/11 · GTX 1050+</span>
              <span>macOS 14.2+ · M1+</span>
              <span>PolyForm Noncommercial</span>
            </div>
            <ul v-if="otherAssets.length" class="download-others">
              <li v-for="asset in otherAssets" :key="asset.id">
                <a :href="asset.browser_download_url" :download="asset.name" rel="noopener noreferrer">{{ asset.name }}</a>
              </li>
            </ul>
            <p class="download-note">
              <a :href="githubUrl" target="_blank" rel="noopener noreferrer">Código no GitHub</a>
              ·
              <a :href="releasesUrl" target="_blank" rel="noopener noreferrer">Todas as releases</a>
            </p>
          </div>
        </div>
      </section>

      <section id="faq" class="section">
        <div class="wrap">
          <div class="section-label">FAQ</div>
          <h2 class="section-title font-display">Perguntas curtas.</h2>
          <div class="faq-list">
            <details>
              <summary>Preciso de internet?</summary>
              <div class="faq-answer">
                No LAN, não. No Stunar, os dois precisam alcançar o Rendezvous. O vídeo ainda vai direto entre vocês.
              </div>
            </details>
            <details>
              <summary>Windows e macOS?</summary>
              <div class="faq-answer">
                Sim. Windows 10/11 e macOS 14.2+ (Apple Silicon). Host e Viewer podem ser de sistemas diferentes.
              </div>
            </details>
            <details>
              <summary>Por que o nome é goDrinking?</summary>
              <div class="faq-answer">
                Em português a gente fala "vai tomando". Alguém traduziu, ficou goDrinking. Não é Go. Não é bar.
              </div>
            </details>
            <details>
              <summary>O servidor vê a minha tela?</summary>
              <div class="faq-answer">
                Não. O Rendezvous só troca recado de sinalização. A mídia é WebRTC P2P.
              </div>
            </details>
            <details>
              <summary>Posso usar no trabalho?</summary>
              <div class="faq-answer">
                Pode. Reunião, suporte, o que for. O que não pode é pegar o código e revender (fork comercial, white-label, SaaS do mesmo app).
              </div>
            </details>
          </div>
        </div>
      </section>
    </main>

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
              <li><a href="#modos">Modos</a></li>
              <li><a href="#download">Download</a></li>
            </ul>
          </div>
          <div class="footer-col">
            <h4>Links</h4>
            <ul>
              <li><a :href="githubUrl" target="_blank" rel="noopener noreferrer">GitHub</a></li>
              <li><a :href="releasesUrl" target="_blank" rel="noopener noreferrer">Releases</a></li>
              <li><a :href="licenseUrl" target="_blank" rel="noopener noreferrer">Licença PolyForm</a></li>
              <li><a href="#faq">FAQ</a></li>
            </ul>
          </div>
        </div>
        <div class="footer-bottom">
          <span>© {{ new Date().getFullYear() }} goDrinking. Não é um app de bar.</span>
          <span>PolyForm Noncommercial 1.0.0</span>
        </div>
      </div>
    </footer>
  </div>
</template>

<script setup>
const REPO = 'CosmicSnow/GoDrinking'
const githubUrl = `https://github.com/${REPO}`
const releasesUrl = `${githubUrl}/releases`
const licenseUrl = `${githubUrl}/blob/main/LICENSE`

const demoCode = ref('A3B9K7')
const copied = ref(false)
const os = ref('mac')
const latest = ref(null)

const tickerOnce = [
  'Não é um app de bar',
  'Não é Golang',
  'Windows e macOS',
  'Sem datacenter na Virginia',
  'Rage against the machine',
  'A luta continua',
  'Sous les pavés, la plage',
]
const ticker = [...tickerOnce, ...tickerOnce]

const taglines = [
  'Rage against the machine',
  'A luta continua',
  'Sous les pavés, la plage',
  'Não é Go. Não é bar.',
  'Sua tela não passa por Virginia',
]
const randomTagline = taglines[Math.floor(Math.random() * taglines.length)]

const latestTag = computed(() => latest.value?.tag_name || '')
const assets = computed(() => latest.value?.assets || [])

function pickMac(list) {
  return list.find((a) => a.name.toLowerCase().endsWith('.dmg'))
    || list.find((a) => /macos/i.test(a.name) && a.name.toLowerCase().endsWith('.zip'))
}
function pickWin(list) {
  return list.find((a) => /setup/i.test(a.name) && a.name.toLowerCase().endsWith('.exe'))
    || list.find((a) => a.name.toLowerCase().endsWith('.msi'))
    || list.find((a) => /portable/i.test(a.name) && a.name.toLowerCase().endsWith('.exe'))
}

const macAsset = computed(() => pickMac(assets.value))
const winAsset = computed(() => pickWin(assets.value))
const primaryIsWin = computed(() => os.value === 'win')
const primaryAsset = computed(() => (primaryIsWin.value ? winAsset.value : macAsset.value))
const secondaryAsset = computed(() => (primaryIsWin.value ? macAsset.value : winAsset.value))
const fallbackLatest = `${githubUrl}/releases/latest`

const primaryHref = computed(() => primaryAsset.value?.browser_download_url || fallbackLatest)
const secondaryHref = computed(() => secondaryAsset.value?.browser_download_url || '')
const primaryDownload = computed(() => primaryAsset.value?.name || undefined)
const secondaryDownload = computed(() => secondaryAsset.value?.name || undefined)
const primaryLabel = computed(() => {
  if (primaryIsWin.value) return latestTag.value ? `Baixar Windows ${latestTag.value}` : 'Baixar para Windows'
  return latestTag.value ? `Baixar macOS ${latestTag.value}` : 'Baixar para macOS'
})
const secondaryLabel = computed(() => (primaryIsWin.value ? 'Baixar macOS' : 'Baixar Windows'))
const otherAssets = computed(() => {
  const skip = new Set([primaryAsset.value?.id, secondaryAsset.value?.id].filter(Boolean))
  return assets.value.filter((a) => !skip.has(a.id))
})

onMounted(async () => {
  const ua = navigator.userAgent
  os.value = /Windows/i.test(ua) ? 'win' : 'mac'
  try {
    const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: 'application/vnd.github+json' },
    })
    if (!res.ok) return
    latest.value = await res.json()
  } catch {
    // Repo ainda privado: o botão cai na página de releases.
  }
})

function generateCode() {
  const chars = 'ABCDEFGHJKLMNPQRSTUVWXYZ23456789'
  let code = ''
  for (let i = 0; i < 6; i++) code += chars.charAt(Math.floor(Math.random() * chars.length))
  return code
}

async function copyCode() {
  demoCode.value = generateCode()
  try {
    await navigator.clipboard.writeText(demoCode.value)
    copied.value = true
    setTimeout(() => { copied.value = false }, 1600)
  } catch { /* ignore */ }
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
.skip-link:focus { top: 0; }

.github-mark {
  display: inline-grid;
  place-items: center;
  width: 2.35rem;
  height: 2.35rem;
  border: 1.5px solid var(--line);
  border-radius: 999px;
  background: var(--ink);
  color: var(--bg);
  text-decoration: none;
  transition: transform 0.15s ease, background 0.15s ease, color 0.15s ease;
}
.github-mark:hover {
  background: var(--lime);
  color: var(--ink);
  transform: rotate(-8deg) scale(1.06);
  text-decoration: none;
}

.download-others {
  list-style: none;
  padding: 0;
  margin: 1.1rem 0 0;
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem 1.1rem;
  font-size: 0.85rem;
  font-family: 'DM Mono', monospace;
}
</style>
