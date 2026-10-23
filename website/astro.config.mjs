// @ts-check
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// GitHub Pages project-page deployment: https://memkeys.github.io/memkeys/
// (domain deferred to *.github.io until memkeys.dev/.io is registered).
// Override via the SITE / BASE env vars when a custom domain lands.
const site = process.env.SITE ?? 'https://memkeys.github.io';
const base = process.env.BASE ?? '/memkeys';
const ogImage = `${site}${base.replace(/\/$/, '')}/og.png`;
/**
 * @param {string} p
 * @returns {string}
 */
function withBase(p) {
  return `${base}${p}`;
}

// Curated information architecture (plan §2/§3.4). The docs pages are synced
// nested under src/content/docs/docs/ by scripts/sync-docs.mjs, so they
// publish at /docs/<slug>/. The landing (/), /why/ and /quickstart/ are
// top-level site pages, not sidebar docs.
const sidebar = [
  {
    label: 'Overview',
    items: [
      { label: 'What is MemKeys', slug: 'docs/what-is-memkeys' },
      { label: 'Architecture', slug: 'docs/architecture' },
    ],
  },
  {
    label: 'Guides',
    items: [
      { label: 'Connect your agents', slug: 'docs/connect-your-agents' },
      { label: 'MCP transport', slug: 'docs/agent-mcp-transport' },
      { label: 'Kubernetes', slug: 'docs/k8s-operator' },
    ],
  },
  {
    label: 'Reference',
    items: [
      { label: 'Retrieval contract', slug: 'docs/retrieval-contract' },
      { label: 'Configuration', slug: 'docs/configuration' },
    ],
  },
];

// Header nav (plan §2): Why MemKeys · Quickstart · Docs · GitHub. Starlight
// has no header-nav config option, so the inline integration below injects
// this nav into the emitted header of every built page.
const headerNav = [
  { href: withBase('/why/'), label: 'Why MemKeys' },
  { href: withBase('/quickstart/'), label: 'Quickstart' },
  { href: withBase('/docs/'), label: 'Docs' },
  { href: 'https://github.com/MemKeys/memkeys', label: 'GitHub' },
];

// IBM Plex Sans — the locked brand-kit face, self-hosted from
// branding/_fonts/IBMPlexSans.ttf as static Regular (400) / Bold (700)
// instances under src/assets/fonts/ (SIL OFL 1.1) and loaded by
// src/styles/brand.css via @font-face. The woff2 files are emitted under
// stable, hash-free names (see the vite config below) so this preload can
// reference the body-weight file.
const fontPreload = withBase(
  '/assets/fonts/IBMPlexSans-Regular.woff2',
);

// Baseline styles for the injected header nav. The brand look (palette,
// spacing, active states) lives in src/styles/brand.css.
const headerNavStyle = `
.mk-header-nav { display: flex; align-items: center; gap: 1.1rem; }
.mk-header-nav a {
  color: var(--sl-color-text);
  font-size: var(--sl-text-sm);
  font-weight: 500;
  text-decoration: none;
  white-space: nowrap;
}
.mk-header-nav a:hover,
.mk-header-nav a[aria-current='page'] { color: var(--sl-color-accent-high); }
`;

/**
 * @param {boolean} isDocsPage
 * @returns {string}
 */
function renderHeaderNav(isDocsPage) {
  const links = headerNav
    .map(({ href, label }) => {
      const current =
        label === 'Docs' && isDocsPage ? ' aria-current="page"' : '';
      return `<a href="${href}"${current}>${label}</a>`;
    })
    .join('');
  return `<nav class="mk-header-nav" aria-label="Site">${links}</nav>`;
}

/**
 * @param {string} dir
 * @returns {Promise<string[]>}
 */
async function listHtmlFiles(dir) {
  /** @type {string[]} */
  const out = [];
  let entries;
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await listHtmlFiles(full)));
    else if (entry.isFile() && entry.name.endsWith('.html')) out.push(full);
  }
  return out;
}

// Starlight renders the site header but offers no config option for custom
// header links, and its header component cannot be extended from config
// alone. This integration therefore injects the curated nav into the emitted
// header of every built page, right after the Starlight header's right-group
// opening tag (the marker is matched by prefix so Astro's scoped-style hash
// suffix does not matter). It runs at build time only — in `astro dev` the
// nav is not rendered; the sidebar and the hero CTAs provide navigation there.
/**
 * @returns {import('astro').AstroIntegration}
 */
function headerNavIntegration() {
  return {
    name: 'memkeys-header-nav',
    hooks: {
      'astro:build:done': async ({ dir, logger }) => {
        const distDir = fileURLToPath(dir);
        const pages = await listHtmlFiles(distDir);
        const marker =
          '<div class="sl-hidden md:sl-flex print:hidden right-group';
        let injected = 0;
        const skipped = [];
        for (const file of pages) {
          let html = await fs.readFile(file, 'utf8');
          // Redirect stubs are bare HTML with no Starlight header; real pages
          // carry the header. Skip both silently — only warn on a real page
          // whose header markup changed.
          if (html.includes('<nav class="mk-header-nav"')) continue; // idempotent
          if (!/<html[\s>]/i.test(html)) continue; // redirect stub
          const at = html.indexOf(marker);
          const openTagEnd = at === -1 ? -1 : html.indexOf('>', at);
          if (openTagEnd === -1) {
            skipped.push(path.relative(distDir, file));
            continue;
          }
          const isDocsPage = path
            .relative(distDir, file)
            .split(path.sep)
            .includes('docs');
          html =
            html.slice(0, openTagEnd + 1) +
            renderHeaderNav(isDocsPage) +
            html.slice(openTagEnd + 1);
          await fs.writeFile(file, html, 'utf8');
          injected += 1;
        }
        logger.info(
          `header-nav: injected site nav into ${injected}/${pages.length} built page(s)`,
        );
        if (skipped.length > 0) {
          logger.warn(
            `header-nav: no Starlight header found in: ${skipped.join(', ')}`,
          );
        }
      },
    },
  };
}

export default defineConfig({
  site,
  base,
  trailingSlash: 'ignore',
  // The docs section root lands on the first curated page; the pre-IA flat
  // doc routes (still linked from hand-authored pages, and previously
  // published URLs) redirect to the curated /docs/ locations.
  redirects: {
    '/docs': withBase('/docs/what-is-memkeys/'),
    '/what-is-memkeys': withBase('/docs/what-is-memkeys/'),
    '/architecture': withBase('/docs/architecture/'),
    '/connect-your-agents': withBase('/docs/connect-your-agents/'),
    '/agent-mcp-transport': withBase('/docs/agent-mcp-transport/'),
    '/k8s-operator': withBase('/docs/k8s-operator/'),
    '/retrieval-contract': withBase('/docs/retrieval-contract/'),
    '/configuration': withBase('/docs/configuration/'),
  },
  vite: {
    build: {
      rollupOptions: {
        output: {
          // Stable, hash-free names for the web fonts so the preload <link>
          // in the head can reference them; every other asset keeps the
          // content hash.
          assetFileNames: (assetInfo) => {
            const fileName = assetInfo.names[0] ?? '';
            return fileName.endsWith('.woff2')
              ? 'assets/fonts/[name][extname]'
              : 'assets/[name]-[hash][extname]';
          },
        },
      },
    },
  },
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
      // SEO metadata + font preload + header-nav baseline styles.
      head: [
        // IBM Plex Sans preload (see fontPreload above).
        {
          tag: 'link',
          attrs: {
            rel: 'preload',
            href: fontPreload,
            as: 'font',
            type: 'font/woff2',
            crossorigin: 'anonymous',
          },
        },
        // Header-nav baseline styles (the nav itself is injected into the
        // emitted pages by the memkeys-header-nav integration).
        { tag: 'style', content: headerNavStyle },
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
      // Curated sidebar (see `sidebar` above). Edit links are set per-page in
      // sync-docs.mjs so they point at the real source file under repo-root
      // docs/, not the generated copy.
      sidebar,
    }),
    headerNavIntegration(),
  ],
});
