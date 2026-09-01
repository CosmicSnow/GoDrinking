# goDrinking site

Site de apresentação do goDrinking — screen share P2P entre Macs.

## Requisitos

- Node.js 18+
- npm

## Instalação

```bash
npm install
```

## Desenvolvimento

```bash
npm run dev
```

O site roda em `http://localhost:3000`.

## Build

```bash
npm run build
```

## Estrutura

- `app/app.vue` — página única com todo o conteúdo
- `app/assets/css/main.css` — estilos em CSS puro, sem Tailwind
- `public/logo.png` — logo do goDrinking
- `nuxt.config.ts` — configuração mínima do Nuxt

## Notas

- Projeto isolado na pasta `site/`, sem backend.
- Design editorial, sem degradês, sem glassmorphism, sem blobs.
- Substitua os links de download/GitHub em `app/app.vue` pelos URLs reais do repositório.
