// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// GitHub Pages project-page deployment: https://memkeys.github.io/memkeys/
// (domain deferred to *.github.io until a custom domain lands).
// Override via the SITE / BASE env vars when a custom domain arrives.
const site = process.env.SITE ?? 'https://memkeys.github.io';
const base = process.env.BASE ?? '/memkeys';
const ogImage = `${site}${base.replace(/\/$/, '')}/og.png`;

export default defineConfig({
  site,
  base,
  trailingSlash: 'ignore',
  integrations: [
    starlight({
      title: 'MemKeys — Keyed memory for AI agents',
      description:
        'MemKeys is an open-source, attribute-based memory server for AI agents. Store each fact by the keys that describe it — with temporal validity — and retrieve it by address. Graph and vector are derived views. Open contract, pluggable backends.',
      logo: {
        light: './src/assets/lockup.svg',
        dark: './src/assets/lockup-reverse.svg',
        replacesTitle: true,
      },
      favicon: '/favicon.svg',
      customCss: ['./src/styles/brand.css'],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/MemKeys/memkeys' },
      ],
      // SEO metadata
      head: [
        { tag: 'meta', attrs: { name: 'theme-color', content: '#17356F' } },
        { tag: 'meta', attrs: { property: 'og:type', content: 'website' } },
        { tag: 'meta', attrs: { property: 'og:site_name', content: 'MemKeys' } },
        { tag: 'meta', attrs: { property: 'og:title', content: 'MemKeys — Keyed memory for AI agents' } },
        { tag: 'meta', attrs: { property: 'og:description', content: 'Every fact has an address. An open-source, attribute-based memory server for AI agents — keyed, temporal, with graph + vector as derived views.' } },
        { tag: 'meta', attrs: { property: 'og:url', content: site } },
        { tag: 'meta', attrs: { property: 'og:image', content: ogImage } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        { tag: 'meta', attrs: { property: 'og:image:alt', content: 'MemKeys — keyed memory for agents. Every fact has an address.' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:title', content: 'MemKeys — Keyed memory for AI agents' } },
        { tag: 'meta', attrs: { name: 'twitter:description', content: 'Every fact has an address. Open-source, attribute-based memory server for agents — keyed, temporal, graph + vector as derived views.' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: ogImage } },
        { tag: 'meta', attrs: { name: 'twitter:image:alt', content: 'MemKeys — keyed memory for agents.' } },
        // JSON-LD structured data
        {
          tag: 'script',
          attrs: { type: 'application/ld+json' },
          content: JSON.stringify({
            "@context": "https://schema.org",
            "@type": "SoftwareApplication",
            "name": "MemKeys",
            "applicationCategory": "DeveloperApplication",
            "operatingSystem": "Linux, macOS, Docker, Kubernetes",
            "description": "Open-source, attribute-based memory server for AI agents. Facts keyed by agent-supplied attributes with temporal validity; graph and vector as derived views. Open contract, pluggable backends.",
            "url": site,
            "license": "https://opensource.org/licenses/Apache-2.0",
            "offers": { "@type": "Offer", "price": "0", "priceCurrency": "USD" }
          })
        },
        // Canonical URL
        { tag: 'link', attrs: { rel: 'canonical', href: site } }
      ],
      // Curated information architecture (the sync allowlist + hand-authored
      // pages define what exists; this sidebar defines the story):
      //   header: Why MemKeys · Quickstart · Docs  (+ GitHub via social)
      //   docs sidebar: Overview → Guides → Reference
      // Why/Quickstart are splash pages listed as sidebar links so they also
      // surface in the header; the landing (index) page is not listed.
      sidebar: [
        { label: 'Why MemKeys', link: '/why/' },
        { label: 'Quickstart', link: '/quickstart/' },
        {
          label: 'Docs',
          items: [
            {
              label: 'Overview',
              items: [
                { label: 'What is MemKeys', link: '/what-is-memkeys/' },
                { label: 'Architecture', link: '/architecture/' },
              ],
            },
            {
              label: 'Guides',
              items: [
                { label: 'Connect your agents', link: '/connect-your-agents/' },
                { label: 'MCP transport', link: '/agent-mcp-transport/' },
                { label: 'Kubernetes: Helm + operator', link: '/k8s-operator/' },
              ],
            },
            {
              label: 'Reference',
              items: [
                { label: 'Retrieval contract', link: '/retrieval-contract/' },
                { label: 'Configuration', link: '/configuration/' },
              ],
            },
          ],
        },
      ],
      // Edit links are set per-page in sync-docs.mjs so they point at the real
      // source file under repo-root docs/, not the generated copy.
    }),
  ],
});
