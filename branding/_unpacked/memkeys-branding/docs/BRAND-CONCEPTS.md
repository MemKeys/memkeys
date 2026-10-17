# MemKeys — Logo Concept Brainstorm

**Issue:** ISI-5828 · **Status: RESOLVED — REV3 LOCKED (2026-10-14)** · **Date:** 2026-10-14

> **FINAL PICK (Henrik-approved):** the evolution of concept-2 "The Address" won —
> **"M3 gradient @ + keyhole"** (`memkeys-m3-keyhole*.svg`, commit `6ab8e0d`):
> gradient ring #17356F→#3E6FD9, golden keyhole #F7C948 (circle + narrowing shaft).
> Production exports live in `kit/` (PNG ladder, favicon.ico, OG, address-kit-rev3.pdf);
> wordmark lockups are re-cut to this mark. The concepts below are the historical
> brainstorm record.

> Logo work proceeds on the **concept**, not the final name. The name is still
> in selection (ISI-5803), so every mark here is a **standalone glyph** — none
> depend on a wordmark, and all survive a name change. Pick the *idea* first;
> the wordmark lockup is drafted after the name locks.

![Contact sheet](./contact-sheet.svg)

---

## Brand essence these marks encode

- **Keys / attributes** are the primary access path to memory — you retrieve a
  fact by *what it is attached to* (project, issue, PR, agent, run).
- **Temporal memory** — facts have validity windows; truth changes over time.
- **Graph + vector are derived views** — structure underneath, discovery on top.
- **Open contract, pluggable backends** — portable, contributor-friendly.
- Personality: precise, structured, trustworthy — the *librarian* of agent
  memory, not the *wilderness* of pure vector search.

## Palette

> **Revised 2026-10-14 (rev2, per Henrik):** after the board picked concept-2
> ("The Address"), the brand palette moved **off teal→blue and amber→yellow** for
> stronger differentiation. The locked palette is below; the shipped mark
> (`memkeys-address*.svg`) and the wordmark lockups (ISI-5838) use these values.
> The concept descriptions further down keep the original teal/amber wording for
> history — read "amber" there as today's yellow accent.

| Token | Hex | Role |
|-------|-----|------|
| **Ink** | `#17356F` | deep cobalt-blue — trustworthy, technical, "librarian" calm |
| **Accent** | `#F7C948` | golden yellow — the single *keyed* fact / value / "now" |
| Reverse ink | `#EAF1FF` | light ink on dark grounds (e.g. `#0B1830`) |
| Paper | `#F7F4EC` | optional light ground (never required; marks sit on any bg) |

One deep tone + one accent, per brief. The accent is used sparingly — it marks
*the one keyed thing* in each glyph (the addressed value, the keyhole, the keyed
node). Every mark also ships **monochrome** (single ink) and **reversed** (light
ink on dark) so it holds in README banners, dark mode, print, and etching.

---

## The five concepts

Each is delivered as `concept-N-<slug>.svg` (colour) and `-mono.svg`
(monochrome), 64×64 artboard, 4px grid, bold strokes tuned for a 16px favicon.

### 1 · Keyed Attribute  — `concept-1-keyed-attribute.svg`
A key whose **bow is a bracket pair `[ ]`** enclosing the keyed value (amber
node). Keys + attributes fused into one glyph.
*"Memory you retrieve by what it's attached to."*
- **Strength:** the most *on-concept* mark — literally a key made of an
  attribute. Square brackets read as "attribute/field" better than curly braces
  at small sizes.
- **Watch:** keep the bit (teeth) chunky so it still reads as a *key* at 16px.

### 2 · The Address  — `concept-2-address.svg`
An **`@`-sign re-read as memory addressing** — the inner glyph is a keyhole, so
`@attribute` becomes the access path to a fact.
*"Every fact has an address; `@` is how you reach it."*
- **Strength:** instant "addressing" read; developer-native symbol.
- **Watch:** the open mouth + tail is what disambiguates it from a generic
  padlock at favicon size — do a 16px squint test before committing.

### 3 · Catalog Card  — `concept-3-catalog.svg`
A **library catalog card** (ruled header, punch-hole on a rod) whose content is
a keyhole. Memory that is *filed*, not floating.
*"Filed, not floating — the librarian of agent memory."*
- **Strength:** owns the librarian metaphor; warmest, most distinctive story.
- **Watch:** most detailed of the five — the punch-hole + rule must survive
  16px; if not, drop the rule and keep hole + keyhole.

### 4 · Keyed Lattice  — `concept-4-lattice.svg`
A small **graph lattice** (diamond of nodes + spokes) with **one node keyed**
(amber, knocked-out keyhole dot in mono). Structure underneath; one fact
addressed within it.
*"One fact, keyed, inside the structure underneath."*
- **Strength:** most abstract/techy, scales perfectly, speaks directly to
  "graph + vector as derived views." Lowest name-dependency risk.
- **Watch:** abstract — relies on the keyed node to carry the "memory" meaning;
  pairs best with a strong wordmark later.

### 5 · Time-Lock  — `concept-5-time-lock.svg`
A key **fused with a clock face** (bow = clock, amber hands = the temporal
element), shaft + bit below. Temporal validity baked into the mark.
*"Facts have validity windows; truth changes over time."*
- **Strength:** the only mark that foregrounds *temporal memory* — a genuine
  differentiator vs. every other memory product.
- **Watch:** most elements of the five; verify the hands don't muddy at 16px.

---

## Collision check (vs. mem0 / memos / memU / Zep / Letta)

Reasoned assessment against the known marks of adjacent memory products:

| Competitor | Typical mark | Our overlap |
|-----------|--------------|-------------|
| mem0 | rounded/infinity-style glyph, teal-leaning | none after rev2 — the palette moved to **cobalt-blue + yellow**, so the earlier teal-proximity concern is resolved; our key/@/card/lattice forms were already distinct |
| Zep | letter **Z** monogram | none — no concept is a letterform |
| Letta | letter/envelope monogram | none |
| memos | note / speech-bubble | none — closest is card (3), but a catalog card ≠ a note bubble |
| memU | letterform / wordmark | none (we are deliberately non-wordmark) |

**Most defensible:** 1 (Keyed Attribute) and 4 (Keyed Lattice) — neither leans
on a letterform or a generic rounded blob. **Flag:** concept 2 should be
squinted against generic padlock/keyhole icons (addressed in the rev2 mark: the
open @ mouth + longer tail disambiguate it). The old "avoid mem0's teal" concern
is moot after rev2 — the palette is now cobalt-blue `#17356F` + yellow `#F7C948`.

> This is a reasoned check from known marks, **not** an exhaustive visual
> trademark search. Recommend a formal image search + USPTO/EUIPO lookup once
> Henrik shortlists 1–2 directions.

---

## Deliverables in this directory

```
gen_marks.py                 reproducible generator (authoritative source)
build_contact_sheet_svg.py   composes contact-sheet.svg from the concept bodies
contact-sheet.svg            single-image overview (colour/mono/reversed/16px)
concept-1-keyed-attribute.svg  (+ -mono)
concept-2-address.svg          (+ -mono)
concept-3-catalog.svg          (+ -mono)
concept-4-lattice.svg          (+ -mono)
concept-5-time-lock.svg        (+ -mono)
```

To regenerate: `python3 gen_marks.py && python3 build_contact_sheet_svg.py`.

## Recommendation & next step

Designer's lean: **1 (Keyed Attribute)** as the primary — it is the most
on-concept and most ownable — with **4 (Keyed Lattice)** as the abstract/techy
alternate. **5 (Time-Lock)** is the wildcard if we want *temporal memory* to be
the headline differentiator.

**Board pick needed (Henrik):** choose 1–2 directions to refine. On pick +
name close (ISI-5803), I'll deliver the final vector set (full favicon/README/
banner kit) and hand off to Content Writer for the wordmark lockup, tagline,
and one-paragraph brand story.
