// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// GitHub Pages project-page deployment: https://memkeys.github.io/memkeys/
// (domain deferred to *.github.io until memkeys.dev/.io is registered — ISI-5919).
// Override via the SITE / BASE env vars when a custom domain lands.
const site = process.env.SITE ?? 'https://memkeys.github.io';
const base = process.env.BASE ?? '/memkeys';
const ogImage = `${site}${base.replace(/\/$/, '')}/og.png`;

export default defineConfig({
  site,
  base,
  trailingSlash: 'ignore',
  integrations: [
    starlight({
      title: 'MemKeys',
      description:
        'The attribute-based memory server for agents — facts keyed by attributes, temporal validity, graph + vector as derived views.',
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
      // Brand OG / social-preview card (branding/_unpacked/memkeys-branding/kit).
      head: [
        { tag: 'meta', attrs: { property: 'og:image', content: ogImage } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: ogImage } },
      ],
      // Sidebar is left to Starlight's automatic generation from the synced
      // docs/ tree for now. The content milestone (M1 · ISI-6397) curates the
      // final information architecture; the assemble milestone (M5 · ISI-6402)
      // wires the landing + nav.
      //
      // Edit links are set per-page in sync-docs.mjs so they point at the real
      // source file under repo-root docs/, not the generated copy.
    }),
  ],
});
