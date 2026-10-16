# MemKeys — Wordmark Lockup, Tagline & Brand Story

**Issue:** ISI-5838 · **Pairs with:** "The Address" mark (ISI-5828, concept-2) · **Name:** locked to **MemKeys** (ISI-5803) · **Date:** 2026-10-14

> This is the **verbal/typographic layer** that pairs with the picked mark —
> type direction, lockup geometry, taglines, and the brand-story paragraph for
> the README / site hero.
>
> **Status (2026-10-14): type direction LOCKED — board picked the _thick IBM
> face_ over Space Grotesk → wordmark is now _IBM Plex Sans Bold (700)_.** Final
> mark+wordmark lockup vectors are re-cut and committed by the Graphic Designer:
> `memkeys-lockup-horizontal/stacked.svg` (+ `-mono`, `-reverse`),
> `memkeys-lockup-sheet.pdf`, `memkeys-typecompare.pdf`. This doc is the
> authoritative verbal/type spec; the SVGs are the authoritative finals.
>
> **Palette is rev2** (per the Address mark revision on ISI-5828): cobalt
> `#17356F` + yellow `#F7C948`, replacing the earlier pine-teal/amber.

---

## 1 · Type selection

**The name is a developer-native compound: `MemKeys`.** Set it as one word with
**camelCase preserved** (capital `M`, capital `K`). The internal capital is doing
real work — it reads like a code identifier / a key, which is exactly the product
(facts retrieved *by key*). Never lowercase it to "memkeys" in the wordmark, and
never split it to "Mem Keys".

### Locked face — **IBM Plex Sans, Bold (700)**  *(board pick, 2026-10-14)*

The board picked the **thick IBM face** ("the letter in thick ibm style is
nicer") over Space Grotesk. The wordmark is now set in **IBM Plex Sans Bold
(700)** — a confident, high-ink weight that reads as systems/infrastructure.

| Why | Detail |
|-----|--------|
| Systems/infrastructure heritage | IBM Plex is the engineering-grade face — corporate-trustworthy, built for technical products; it says "serious backend." |
| Thick & confident | Bold (700) gives the camelCase real presence and weight; the chunky stems stand up to the round `@` ring of the mark. |
| Freely embeddable | SIL Open Font License → ships in the README, the site, and the repo with no licensing friction (matches the open-contract brand value). |
| camelCase legibility | Plex's clear, upright letterforms keep the internal capital `K` reading cleanly even at the bold weight. |

**Optional weight play (default OFF):** set `Mem` in SemiBold (600) and `Keys` in
Bold (700) for a subtle camelCase hinge without a color change. Default lockup is
**single weight (Bold 700)** for maximum impact; use the weight play only where
the wordmark appears large and alone (hero, banner).

### Alternates (if the board wants a different tone)
- **IBM Plex Sans, SemiBold (600)** — one notch lighter than the locked Bold; use if the Bold reads too heavy at a given size. Same face, calmer.
- **Space Grotesk, Medium (500)** — the prior direction; geometric grotesque whose circular bowls rhyme with the `@` ring. More "modern startup," less "infrastructure."
- **Inter, SemiBold** — safest, most neutral; choose only if "blend in with the dev-tool crowd" is the goal. (Designer's note: least ownable.)

> **Color rule (hard):** the wordmark is **single-ink cobalt** (`#17356F`), or
> reversed (`#EAF1FF` on dark `#0B1830`). **Yellow never touches the letters.**
> Yellow (`#F7C948`) is reserved for the one keyed thing — the keyhole in the
> mark. One accent, used once. This is the discipline that keeps the whole system
> legible.

---

## 2 · Lockup geometry (authoritative ratios)

Measurement unit is the **mark height** (the full `@` artboard). The Graphic
Designer sets the final optical kerning; these ratios define the system.

### Horizontal (primary)
- **Mark height = 1.5 × wordmark cap-height.** The `@` ring sits a touch taller
  than the caps so it reads as a peer glyph, not a bullet.
- **Vertical alignment:** mark optically centered on the wordmark's cap-height
  midline (not the baseline — the ring is round, so center it optically).
- **Gap (mark → `M`) = 0.6 × mark width.**
- Use horizontally wherever width is available: README header, site nav, slides.

### Stacked
- Mark **centered above** the wordmark.
- **Vertical gap = 0.5 × mark height** (mark baseline → wordmark cap line).
- Wordmark centered under the mark. Use for square avatars, social cards, swag,
  the GitHub org avatar.

### Clearspace
- **≥ 1 × mark-height** of clear space on all four sides of the whole lockup
  (dashed box in the wireframe). For the standalone mark, the kit's rule holds:
  padding ≥ the keyhole height.

### Minimum sizes
- Horizontal lockup: **wordmark cap-height ≥ 12px.** Below that, drop the
  wordmark and use the mark alone.
- Stacked lockup: **mark ≥ 24px.**
- Standalone mark (favicon): **16px floor**; below 16px drop the tail, keep ring
  + keyhole (per `address-kit.svg`).

---

## 3 · Tagline options

The mark's story is **addressing + keyed memory**. Five options, by role:

1. **"Every fact has an address."** — *hero line.* Straight from the mark; poetic
   but precise; owns the "@-as-address" idea. **Recommended primary.**
2. **"Keyed memory for agents."** — *category descriptor.* Plain, search-friendly,
   says exactly what the product is. Best for README subtitle / meta description.
3. **"Memory you reach by key."** — *short active line.* Good for slides, swag, the
   one-liner under the mark.
4. **"Address your agent's memory."** — *verb-forward / CTA.* Developer-native double
   meaning (to *address* = to reach + to deal with). Punchy for landing CTA; note
   the "deal with a problem" reading — use where context is clearly about storage.
5. **"The librarian of agent memory."** — *personality line.* Warm, differentiating
   vs. vector-search "wilderness"; use in About / narrative copy, not as the hero.

**Recommended pairing:** hero **#1** with descriptor **#2** beneath it —
> *Every fact has an address.*
> MemKeys — keyed memory for agents.

---

## 4 · Brand story

### Hero paragraph (README / site hero, ~110 words)

> **MemKeys** is an attribute-based memory server for AI agents. Instead of
> dropping facts into a vector blob and hoping to find them again, agents store
> each fact with the keys that describe it — the project, the issue, the run, the
> moment it was true — and retrieve it *by address*, the way you pull a card from
> a catalog. Every fact carries a validity window, so memory reflects what was
> true *then*, not just what happens to be cached *now*. Graph and vector views
> are derived on top of that keyed core, not bolted on beside it. The contract is
> open and the backends are pluggable. **Memory your agents can actually find
> again.**

### Short hero variant (~40 words, tight layouts)

> **MemKeys** gives AI agents memory with an address. Store each fact by the keys
> that describe it, with a validity window, and retrieve it exactly — no vector
> guesswork. Graph and vector are derived views. Open contract, pluggable
> backends.

### One-liner (repo description / social bio)

> Attribute-based memory server for agents — facts keyed by attribute, with
> temporal validity; graph + vector as derived views. Open contract, pluggable
> backends.

---

## 5 · Finals (delivered)

Type direction is **locked to IBM Plex Sans Bold (700)** (board picked the thick
IBM face over Space Grotesk, 2026-10-14). The Graphic Designer re-cut the final
vectors against the rev2 palette:
- `memkeys-lockup-horizontal.svg` (+ `-mono`, `-reverse`)
- `memkeys-lockup-stacked.svg` (+ `-mono`, `-reverse`)
- `memkeys-lockup-sheet.svg` / `.pdf` — board lockup page (clearspace, min sizes, palette chips)
- `memkeys-typecompare.svg` / `.pdf` — the 3-way visual typeface comparison that drove the pick
- `gen_lockup.py` — generator (reads mark geometry from `memkeys-address.svg`)

Wordmark is set as **outlined glyph paths** (font-independent), camelCase,
single-ink cobalt, yellow only in the mark. Ratios per §2.

> **✅ Pairing note RESOLVED (REV3 kit, 2026-10-14):** `gen_lockup.py` now reads
> the mark geometry from **`memkeys-m3-keyhole.svg`** — all six lockup finals
> (`memkeys-lockup-{horizontal,stacked}{,-mono,-reverse}.svg`), the lockup sheet
> and `memkeys-lockup-sheet.pdf` are **re-cut with the REV3 M3 gradient mark**
> (gradient ring in primary, flat `#0B1F3A` mono, lightened gradient reverse).
> Production exports (PNG ladder, favicon.ico, OG, address-kit-rev3.pdf) are in
> `kit/` via `gen_rev3_kit.py`.
