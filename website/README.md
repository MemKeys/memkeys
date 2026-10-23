# MemKeys website

The MemKeys documentation + landing site, built with [Astro](https://astro.build)
and [Starlight](https://starlight.astro.build). One property hosts both the docs
experience and the marketing landing page (ISI-6398, plan §4).

Published to **GitHub Pages** at <https://memkeys.github.io/memkeys/> and
republished on every merge to `main` (`.github/workflows/site.yml`). The domain
is deferred to `*.github.io` until `memkeys.dev` / `.io` is registered
(ISI-5919); override `SITE` / `BASE` env vars when a custom domain lands.

## Docs are sourced from `docs/`

The engineering docs under the repo-root [`docs/`](../docs) tree are the single
source of truth. `scripts/sync-docs.mjs` ingests them into
`src/content/docs/` (git-ignored) at dev/build time, injecting the minimal
Starlight frontmatter (`title` derived from the first `# H1`, plus an `editUrl`
back to the real source file). **Edit the docs under `docs/`, not the generated
copies.** New Markdown added to `docs/` appears automatically.

The landing page (`src/content/docs/index.mdx`) is the one hand-authored page
that lives in the site itself.

## Develop

```bash
npm install
npm run dev      # runs sync-docs then astro dev on :4321
```

| Command         | Action                                                     |
| --------------- | ---------------------------------------------------------- |
| `npm run sync`  | Ingest `docs/` → `src/content/docs/`                       |
| `npm run dev`   | Sync, then start the dev server                            |
| `npm run build` | Sync, then build the static site to `dist/`                |
| `npm run preview` | Preview the built `dist/` locally                        |
| `npm run check` | Sync, then run `astro check` (type + content validation)   |

## Brand

Theme and assets come from the locked brand kit
([`branding/_unpacked/memkeys-branding/`](../branding/_unpacked/memkeys-branding)):
palette in `src/styles/brand.css`, logo lockups + hero mark in `src/assets/`,
favicon + OG/social card in `public/`. No new identity work — visual/UX
refinement is M4a (ISI-6399) and copy is M4b (ISI-6400).
