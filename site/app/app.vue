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
            <a href="#faq">FAQ</a>
            <a href="#download" class="btn btn-primary">Download</a>
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
                <span class="nowrap">Divide a tela.</span><br />
                <span class="nowrap">Não seus dados!<span class="cursor" aria-hidden="true"></span></span>
              </h1>
              <p class="hero-sub">
                Host captura. Viewer assiste. O vídeo vai direto de um PC para o outro. Sem nuvem, sem login, sem a tela passando por um datacenter.
              </p>
              <div class="hero-actions">
                <a :href="primaryHref" class="btn btn-primary btn-large" :download="primaryDownload" rel="noopener noreferrer">
                  {{ primaryLabel }}
                </a>
                <a href="#download" class="btn btn-large">Baixar outros</a>
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

            <figure class="shot-frame">
              <button
                class="shot-zoom"
                type="button"
                aria-haspopup="dialog"
                aria-label="Ampliar captura de tela do aplicativo"
                @click="openLightbox"
              >
                <img
                  src="/app-sample.png"
                  alt="Sala do goDrinking com 3 pessoas, 2 compartilhando"
                  class="shot-img"
                  width="1534"
                  height="861"
                />
              </button>
            </figure>
          </div>
        </div>
      </section>

      <section id="o-que-e" class="section">
        <div class="wrap">
          <div class="section-label">Guia do app</div>
          <h2 class="section-title font-display">Screen share. Ponto a ponto.</h2>
          <div class="def-grid">
            <div class="def-box">
              <h3>O que é</h3>
              <ul class="def-list">
                <li>Salas com tela compartilhada: todo mundo pode compartilhar e escolher o que assistir.</li>
                <li>Salas protegidas por senha.</li>
                <li>Apelidos personalizáveis.</li>
                <li>Escolha quais apps têm o áudio compartilhado.</li>
                <li>H.264 com aceleração de hardware e fallback para software.</li>
                <li>Windows e macOS suportados.</li>
                <li>Código aberto e auditável no GitHub: o app é exatamente o que o código mostra.</li>
                <li>Licença PolyForm Noncommercial.</li>
              </ul>
            </div>
            <div class="def-box">
              <h3>O que não é</h3>
              <ul class="def-list">
                <li>O servidor não enxerga sua transmissão.</li>
                <li>Sem captura de dados.</li>
                <li>Sem servidor TURN: sem relay, a mídia nunca passa por servidor nenhum.</li>
                <li>Pode usar no trabalho ou em atividades comerciais; mas revender ou usar trechos em ferramentas pagas só com autorização expressa.</li>
                <li>Não é feito em Golang.</li>
                <li>Não é aplicativo de bar ou de bebida.</li>
                <li>Também não incentivamos o consumo de bebidas.</li>
                <li>O nome é uma piada, não leve ao pé da letra.</li>
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
              <h3>Iniciar sala</h3>
              <p>Escreva uma senha e clique em "Criar sala".</p>
            </article>
            <article class="step">
              <h3>Copia o código e compartilha!</h3>
              <p>A sala ganha um código de 6 caracteres. Aperte copiar e envie para seus amigos, ou grite ele pela casa.</p>
            </article>
            <article class="step">
              <h3>Todo mundo participa</h3>
              <p>Entre com o código e a senha. Todos podem compartilhar ou assistir, e cada um é host da própria transmissão.</p>
            </article>
          </div>
          <p class="mt-md" style="color: var(--gray); font-size: 0.9rem;">
            Uma pessoa é a líder da sala, mas todos podem compartilhar: não importa quem criou a sala, quem compartilha a própria tela é o host da própria transmissão. O servidor não recebe nada do que é compartilhado, ele só apresenta as pessoas e mantém a sala aberta para que elas se conectem.
          </p>
        </div>
      </section>

      <section id="features" class="section">
        <div class="wrap">
          <div class="section-label">Qual a mágica?</div>
          <h2 class="section-title font-display">O que o app faz.</h2>
          <div class="features-list">
            <div class="feature">
              <span class="feature-tag">Sala</span>
              <h3>Só salas, sem palco</h3>
              <p>Nada de um transmite e o resto só assiste. Toda sala é de hosts e viewers, e cada pessoa escolhe quem assistir, com grelha, pin e zoom.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Quality</span>
              <h3>Low / Medium / High</h3>
              <p>Codec H.264 e controle fino de resolução, framerate e bitrate.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">System audio</span>
              <h3>Áudio com exclusão</h3>
              <p>Compartilha o som do PC com mixagem por aplicativo: exclua o chat de voz ou qualquer app da transmissão.</p>
            </div>
            <div class="feature">
              <span class="feature-tag">Papéis</span>
              <h3>Todo mundo pode ser tudo</h3>
              <p>Salas de hosts e viewers: todos podem ter todos os papéis, compartilhar e assistir ao mesmo tempo. Com a admission ligada, a líder aprova cada entrada e o roster mostra quem está dentro.</p>
            </div>
          </div>
        </div>
      </section>

      <section id="download" class="section">
        <div class="wrap">
          <div class="download-box">
            <div class="section-label">Download</div>
            <h2 class="section-title font-display">Windows e macOS. Instale no seu PC.</h2>
            <div class="hero-actions">
              <a :href="primaryHref" class="btn btn-primary btn-large" :download="primaryDownload" rel="noopener noreferrer">
                {{ primaryLabel }}
              </a>
              <a v-if="secondaryHref" :href="secondaryHref" class="btn btn-large" :download="secondaryDownload" rel="noopener noreferrer">
                {{ secondaryLabel }}
              </a>
            </div>
            <p class="section-intro" style="margin-bottom: 1.25rem;">
              {{ latestTag ? `Última versão: ${latestTag}` : 'Puxa a última release do GitHub.' }}
            </p>
            <div class="requirements">
              <span>Windows 10/11</span>
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
          <aside class="sign-note">
            <h3>Sem assinatura digital... Mas sem drama!</h3>
            <p>O macOS e o Windows podem avisar que o app não tem assinatura digital. O motivo é simples: assinar custa dinheiro e este é um projeto independente.</p>
            <p>Isso não significa que o app é inseguro. O código é aberto, ou seja, você pode revisar tudo no GitHub, ou até mesmo compilar você mesmo. E pode até pedir para sua IA de estimação revisar o código.</p>
            <div class="sign-how">
              <div>
                <h4>No macOS</h4>
                <p>Tente abrir o app. Depois abra Ajustes do Sistema, Privacidade e Segurança, e clique em Abrir Mesmo Assim. Confirme em Abrir. <a href="https://support.apple.com/en-us/102445" target="_blank" rel="noopener noreferrer">Passo a passo oficial da Apple</a></p>
              </div>
              <div>
                <h4>No Windows</h4>
                <p>Na tela O Windows protegeu o PC, clique em Mais informações e depois em Executar mesmo assim. <a href="https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation" target="_blank" rel="noopener noreferrer">Documentação oficial da Microsoft</a></p>
              </div>
            </div>
            <p class="sign-donate">Doações para que possamos assinar digitalmente o app são bem-vindas e vão direto para melhorar o projeto. Melhorias e colaboradores também são bem-vindos!</p>
            <a :href="githubUrl" class="btn" target="_blank" rel="noopener noreferrer">Ver o código no GitHub</a>
          </aside>
        </div>
      </section>

      <section id="faq" class="section">
        <div class="wrap">
          <div class="section-label">FAQ</div>
          <h2 class="section-title font-display">Perguntas frequentes.</h2>
          <div class="faq-list">
            <details>
              <summary>Preciso de internet?</summary>
              <div class="faq-answer">
                No momento, sim. A menos que você hospede o servidor do projeto na sua rede local e use o endereço dele no campo "Servidor" da tela principal. De um jeito ou de outro, o vídeo vai direto entre vocês.
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
                Não. O servidor só troca a sinalização entre os participantes para que eles enviem a mídia via WebRTC P2P, e os participantes mantêm conexão com o servidor via websocket para se enxergarem na sala. Ou seja: o servidor não vê suas mídias e não sabe quem é você, só sabe o nick que você colocou e enquanto a sala estiver aberta!
              </div>
            </details>
            <details>
              <summary>Posso usar no trabalho?</summary>
              <div class="faq-answer">
                Pode. Reunião, suporte, o que for. O que não pode é pegar o código e revender (fork comercial, white-label, SaaS do mesmo app).
              </div>
            </details>
            <details>
              <summary>O app pode ter bugs?</summary>
              <div class="faq-answer">
                Sim. O app ainda pode ter alguns bugs e pode não funcionar bem para quem está atrás de CGNAT (algumas provedoras de internet). Os bugs encontrados serão corrigidos conforme houver tempo livre: o projeto é sem fins lucrativos.
              </div>
            </details>
            <details>
              <summary>Encontrei um bug, o que fazer?</summary>
              <div class="faq-answer">
                Abra um issue no GitHub contando o que aconteceu, o que você esperava que acontecesse, seu sistema (Windows ou macOS) e o passo a passo para reproduzir. Se puder, anexe prints ou trechos do log. Issues bem descritos são corrigidos mais rápido, e pull requests são bem-vindos! <a :href="githubUrl + '/issues'" target="_blank" rel="noopener noreferrer">Abrir um issue</a>
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

    <Teleport to="body">
      <Transition name="lightbox">
        <div
          v-if="lightboxOpen"
          class="lightbox"
          role="dialog"
          aria-modal="true"
          aria-label="Captura de tela ampliada do aplicativo goDrinking"
          @click.self="closeLightbox"
        >
          <button
            ref="lightboxCloseBtn"
            class="lightbox-close"
            type="button"
            aria-label="Fechar imagem ampliada"
            @click="closeLightbox"
          >
            ×
          </button>
          <img
            src="/app-sample.png"
            alt="Sala do goDrinking com 3 pessoas, 2 compartilhando"
            class="lightbox-img"
            width="1534"
            height="861"
            @click="closeLightbox"
          />
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<script setup>
const REPO = 'CosmicSnow/GoDrinking'
const githubUrl = `https://github.com/${REPO}`
const releasesUrl = `${githubUrl}/releases`
const licenseUrl = `${githubUrl}/blob/main/LICENSE`

const os = ref('mac')
const latest = ref(null)

const tickerOnce = [
  'Não é um app de bar',
  'Não é Golang',
  'Windows e macOS',
  'Sem datacenter na Virginia',
  'Sua tela, suas regras',
  'Sous les pavés, la plage',
  'Sem nuvem no meio',
  'Direto de PC para PC',
  'P2P criptografado',
  'Sem login, sem conta',
  'O servidor só apresenta',
  'Fácil de usar',
  'Não é streaming',
  'Sem algoritmo',
  'A nuvem é o PC dos outros',
  'LAN, Direct ou Stunar',
  'Ninguém no meio do caminho',
]
const ticker = [...tickerOnce, ...tickerOnce]

const taglines = [
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
    // Fallback genérico: cobre o nome real publicado (ex. `goDrinking.exe`).
    || list.find((a) => a.name.toLowerCase().endsWith('.exe'))
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
// Hero sem sufixo de versão; a versão aparece na seção de download.
const primaryLabel = computed(() => (primaryIsWin.value ? 'Baixar para Windows' : 'Baixar para macOS'))
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
    // Se a API falhar (rate-limit/rede), o primário cai na página de releases.
  }
})

// Lightbox da captura de tela do hero
const lightboxOpen = ref(false)
const lightboxCloseBtn = ref(null)
let lastOpener = null

function openLightbox(event) {
  lastOpener = event?.currentTarget ?? document.activeElement
  lightboxOpen.value = true
  document.body.classList.add('no-scroll')
  nextTick(() => lightboxCloseBtn.value?.focus())
}

function closeLightbox() {
  if (!lightboxOpen.value) return
  lightboxOpen.value = false
  document.body.classList.remove('no-scroll')
  nextTick(() => {
    if (lastOpener instanceof HTMLElement) lastOpener.focus()
    lastOpener = null
  })
}

function onLightboxKeydown(event) {
  if (event.key === 'Escape') closeLightbox()
}

onMounted(() => window.addEventListener('keydown', onLightboxKeydown))
onUnmounted(() => {
  window.removeEventListener('keydown', onLightboxKeydown)
  document.body.classList.remove('no-scroll')
})

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
