# MemKeys website — UX & visual design handoff (M4a · ISI-6399)

Design of the **landing/marketing site** (not the docs app). This folder is the
handoff for the site build (**M3 · ISI-6398**, DevOps, Astro + Starlight) and the
copy pass (**M4b · ISI-6400**, Content Writer). Everything here derives from the
locked brand kit in [`../branding/_unpacked/memkeys-branding/`](../branding/_unpacked/memkeys-branding/).

> Open [`mockup.html`](./mockup.html) in a browser for the full, responsive,
> clickable reference. It is self-contained (loads IBM Plex from Google Fonts and
> the SVGs in `assets/`). The Astro build should reproduce it — this HTML is the
> source of truth for layout, spacing, and section order.

---

## 1 · What's in here (asset manifest)

| File | Role | Where it goes on the page | Notes |
|------|------|---------------------------|-------|
| `assets/hero-visual.svg` | Hero illustration | Hero, right column | **Transparent** — designed to sit on the `#0B1830` dark ground. 720×560, scales fluidly. |
| `assets/architecture.svg` | Architecture explainer | "How it works" section | 1200×640, light `#F7F4EC` ground. The keyed-first / graph+vector-derived story. |
| `assets/how-step-1-run.svg` | Step 1 icon | How-to-use card 1 | 96×96, transparent. |
| `assets/how-step-2-connect.svg` | Step 2 icon | How-to-use card 2 | 96×96, transparent. |
| `assets/how-step-3-browse.svg` | Step 3 icon | How-to-use card 3 | 96×96, transparent. |
| `mockup.html` | Full page reference | — | Desktop + mobile (resize to ≤860px). Not shipped; M3 rebuilds in Astro. |

**Reused directly from the brand kit** (no new asset needed — link, don't copy):
- Nav / footer wordmark → `branding/.../lockups/memkeys-lockup-horizontal-reverse.svg` (light ink for dark bars).
- Favicon → `branding/.../kit/favicon.svg` + `favicon.ico`.
- OG / social preview → `branding/.../kit/memkeys-og-1200x630.png` and `memkeys-social-github-1280x640.png`.

---

## 2 · Design tokens

Lift these into the Astro theme (`:root` CSS variables; see `mockup.html` for the
canonical block). Palette is the **rev3 locked brand palette** — do not add a
second accent.

```css
--ink:      #17356F;  /* deep cobalt — headings, body ink, primary strokes       */
--ink-2:    #3E6FD9;  /* bright cobalt — gradient end, links, hovers              */
--accent:   #F7C948;  /* golden keyhole — THE one keyed thing; use sparingly      */
--ground:   #0B1830;  /* dark ground — hero, final-CTA, footer bands             */
--ground-2: #13294F;  /* raised surface on the dark ground (cards)               */
--paper:    #F7F4EC;  /* warm light ground — the architecture band               */
--reverse:  #EAF1FF;  /* light ink on dark grounds                               */
--muted:    #4A618C;  /* secondary text on light                                 */
--muted-d:  #8AA6D8;  /* secondary text on dark                                  */
```

Brand gradient (ring / primary buttons optional): `linear-gradient(135deg,#17356F,#3E6FD9)`.

**Accent discipline (hard rule from the brand kit):** `#F7C948` marks *the one
keyed thing* — the keyhole in the mark, the keyed value in a fact card, the primary
CTA, the keyed node in a diagram. **Yellow never touches the wordmark letters.**
One accent, used once per view.

### Type
- **Display / headings / wordmark:** IBM Plex Sans **Bold (700)** — the locked brand face.
- **UI / body:** IBM Plex Sans 400/500/600.
- **Code / keys / addresses:** IBM Plex Mono 400/600 — used for the `@address`
  motif, chips, and inline `code`. (Fonts ship in `../branding/_fonts/`; mockup
  pulls them from Google Fonts for convenience. Deliberately **not** Inter.)

### Shape & elevation
- Corner-radius scale: **10 / 16 / 24px** (`--r-s/-m/-l`). Pick from this ladder only.
- Elevation: one soft shadow for figures/cards (`0 24px 48px -32px rgba(23,53,111,.35)`),
  flat everywhere else. No heavy drop shadows.

### Taste dials used
`DESIGN_VARIANCE 4` (asymmetric hero, not centered) · `MOTION_INTENSITY 2`
(hover lifts + 80ms button press only) · `VISUAL_DENSITY 2`. No AI-purple
gradients, no beige+brass, single locked accent, one radius scale.

---

## 3 · Page structure & UX (per plan §3)

Order top→bottom. Max content width **1120px**, 24px side gutters.

1. **Nav** (dark `--ground`) — horizontal reverse lockup left; `How it works ·
   Get started · Docs · GitHub` + a primary "Get started" button right. Collapses
   to just the lockup < 860px (M3: add a hamburger → the same links).

2. **Hero** (dark `--ground`) — two columns (≈ 52 / 48), **left-aligned, not
   centered**:
   - Eyebrow: `KEYED MEMORY FOR AGENTS`
   - H1: **"Every fact has an `address`."** (the word *address* in `--accent`)
   - Lede (~30ch): one-line pitch.
   - CTAs: **Get started →** (primary, accent fill, ink text) + **View on GitHub** (ghost).
   - Trust row: `Open contract · Pluggable backends · MCP · REST`.
   - Right: `hero-visual.svg`.
   - Mobile: art stacks under the copy, capped ~460px wide.

3. **Architecture — "How it works"** (light `--paper`) — kicker + H2
   *"Keyed first. Graph and vector are derived views."* + lede, then
   `architecture.svg` in a white rounded figure card, then a 3-up caption rail
   (Keyed core / Graph view / Vector view). This is the non-expert skim: read the
   diagram, read three captions, done.

4. **How to use** (white) — kicker + H2, then a **3-step card grid** (01 Run it →
   02 Connect an agent → 03 Browse memory). Each card: step icon, title, one-line
   explanation, a `mono` command chip, and a **deep link into the docs**
   (Quickstart / Connect an agent / Inspector). Grid → single column < 860px.

5. **Final CTA** (dark `--ground`, centered) — *"Memory your agents can actually
   find again."* + Get started / Star on GitHub.

6. **Footer** (`#071124`) — reverse lockup + one-line description; **Product**
   (How it works, Get started, Docs, Inspector) and **Project** (GitHub, Releases,
   License, Brand assets) columns; legal line with the tagline. Covers the issue's
   footer requirement (GitHub, docs, license, brand assets).

### Deep-link targets for M4b/M3 (fill when docs land — ISI-6397/6401)
`Get started → /docs/quickstart` · `Connect an agent → /docs/connect-an-agent`
· `Browse memory → /docs/inspector` · `Docs → /docs` · `License → LICENSE` ·
`Brand assets → the branding kit`.

---

## 4 · The architecture visual — what it says

A non-expert should read it left→right and come away with one idea: **store once,
keyed; the other views are derived.**

- **Left — your agents** (coder / reviewer / researcher) write and recall through
  the **4-op contract** (store · fetch · search · traverse).
- **Center — the keyed core** (dark panel): fact cards, each with a golden
  **keyed value + keyhole**, **attribute chips** (`project:`, `issue:`, `agent:`…),
  and a **validity-window** bar ("true *then*, not just cached *now*").
- **Right — derived views** (dashed, lighter, tagged `derived`): a **graph view**
  (node-link, one keyed node golden) and a **vector view** (query point + similarity
  radius). Arrows run **from** the core **to** the views to show direction of
  derivation.
- Caption: *"Store once, keyed. Graph and vector are views on the same facts — not
  three copies to keep in sync."*

---

## 5 · Responsive & a11y notes for M3

- Single breakpoint at **860px** in the mockup; add an intermediate tablet tweak if
  desired. Hero, steps, and footer all reflow to one column.
- SVGs are `width:100%; height:auto` inside their containers — no fixed pixel sizes.
- Every informational SVG has a `<title>` and `role="img"` + `aria-label`; decorative
  step icons use empty `alt=""` in the `<img>` (already set in the mockup).
- Contrast: reverse ink `#EAF1FF` on `#0B1830` and ink `#17356F` on `#F7F4EC`/white
  all clear WCAG AA. The accent is used for fills/marks, not body text.
- Respect `prefers-reduced-motion` — the only motion is hover lift + button press;
  gate those behind the media query in the Astro build.

---

## 6 · Handoff status

Delivered: layouts (full HTML mockup), the hero visual, the architecture explainer,
three step icons, tokens, and responsive/a11y specs — **ready for M3 (ISI-6398) to
assemble** and **M4b (ISI-6400) to drop final copy into**. Copy in the mockup is
placeholder/indicative; Content Writer owns the final words.
