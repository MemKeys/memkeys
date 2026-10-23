# MemKeys — Website Landing Copy & Positioning

**Issue:** ISI-6400 (Milestone M4b) · **Parent:** ISI-6393 · **Owner:** Content Writer · **Date:** 2026-10-21

> **Purpose.** This is the authoritative copy deck for the MemKeys landing page. It is
> ready for the **site build (M3, ISI-6398 — Astro + Starlight)** to drop into templates
> and for **design (M4a, ISI-6399)** to lay out. Nothing here is a code dependency; it is
> words, metadata, and asset pointers.
>
> **Sources (reused, not reinvented):** the `README.md` pitch, `docs/architecture.md`
> §0 (ecosystem positioning) and §1 (summary), and the approved taglines / hero paragraphs
> in `branding/_unpacked/memkeys-branding/docs/BRAND-WORDMARK.md` §3–§4. The "What is
> MemKeys" doc page (M1, ISI-6397) is the long-form companion; this page is its front door
> and must stay consistent with it.

---

## 0 · Positioning (the one story everything aligns to)

**Category:** attribute-based (keyed) memory server for AI agents.

**One-sentence positioning:**
> MemKeys is the open-source memory server that lets AI agents store each fact by the
> keys that describe it — and retrieve it by address, with a validity window — instead of
> dropping everything into a vector blob and hoping to find it again.

**The keyed-first story (the spine of all copy):**
Most agent "memory" today is a pile of embeddings: you write text, you get back whatever
is *nearest* at query time, and you cross your fingers. MemKeys inverts that. A fact is
stored with **the attributes that make it findable** — the project, the issue, the run,
the moment it was true — so retrieval starts from **what you know about the fact**, not
from a similarity guess. Keys bound the candidate set *before* any vector or graph work
runs. The keyed, temporal store is the **single source of truth**; graph and vector are
**derived, rebuildable views** on top of it, never a parallel source. That is the whole
product in one line: **memory your agents can actually find again.**

**What we are not.** MemKeys is **not** MemPalace (a separate internal MCP shared-memory
layer — different project, different design). Despite the shared "mem-" prefix, MemKeys is
not built on MemPalace and MemPalace is not MemKeys. Keep this distinction crisp anywhere
the two could be confused; do **not** feature it on the hero.

**Three proof pillars** (use as the three value props; order is deliberate):
1. **Keyed-first retrieval** — reach a fact by its address; keys bound the candidate set before vector/graph.
2. **Temporal validity** — every fact has a validity window, so memory reflects what was true *then*, not just what's cached *now*.
3. **Open & pluggable** — open retrieval contract, pluggable backends (Grail plugin as the reference); graph + vector are derived views you can rebuild.

**Voice.** Developer-native, infrastructure-serious, quietly confident. We explain by
contrast with the vector-blob status quo, never by dunking on it. Short sentences. No
hype adjectives ("revolutionary", "blazing-fast"). The product sounds like a catalog, a
key, an address — reach for those metaphors, not "AI magic."

---

## 1 · SEO & Open Graph metadata

Drop these into the Astro/Starlight site `<head>` (or the frontmatter the M3 build maps
to head tags). Canonical domain is the GH Pages default (`https://memkeys.github.io/`)
until a custom domain is wired; M3 should template the base URL so it is swappable.

### Primary SEO

| Field | Value |
|---|---|
| `<title>` | `MemKeys — Keyed memory for AI agents` |
| `meta name="description"` | `MemKeys is an open-source, attribute-based memory server for AI agents. Store each fact by the keys that describe it — with temporal validity — and retrieve it by address. Graph and vector are derived views. Open contract, pluggable backends.` |
| `meta name="keywords"` *(optional, low weight)* | `agent memory, AI agent memory, keyed memory, attribute-based memory, temporal memory, vector database alternative, knowledge graph, open source` |
| `link rel="canonical"` | `https://memkeys.github.io/` |
| `meta name="theme-color"` | `#17356F` (cobalt; brand palette rev2) |

> **Title discipline:** keep the `<title>` at ~45 characters so it isn't truncated in SERPs.
> Interior pages use the pattern `<Page> — MemKeys`.

### Open Graph (Facebook / LinkedIn / Slack unfurl)

| Property | Value |
|---|---|
| `og:type` | `website` |
| `og:site_name` | `MemKeys` |
| `og:title` | `MemKeys — Keyed memory for AI agents` |
| `og:description` | `Every fact has an address. An open-source, attribute-based memory server for AI agents — keyed, temporal, with graph + vector as derived views.` |
| `og:url` | `https://memkeys.github.io/` |
| `og:image` | `/og/memkeys-og-1200x630.png` |
| `og:image:width` | `1200` |
| `og:image:height` | `630` |
| `og:image:alt` | `MemKeys — keyed memory for agents. Every fact has an address.` |

### Twitter / X Card

| Property | Value |
|---|---|
| `twitter:card` | `summary_large_image` |
| `twitter:title` | `MemKeys — Keyed memory for AI agents` |
| `twitter:description` | `Every fact has an address. Open-source, attribute-based memory server for agents — keyed, temporal, graph + vector as derived views.` |
| `twitter:image` | `/og/memkeys-og-1200x630.png` |
| `twitter:image:alt` | `MemKeys — keyed memory for agents.` |

### JSON-LD (SoftwareSourceCode / SoftwareApplication)

```json
{
  "@context": "https://schema.org",
  "@type": "SoftwareApplication",
  "name": "MemKeys",
  "applicationCategory": "DeveloperApplication",
  "operatingSystem": "Linux, macOS, Docker, Kubernetes",
  "description": "Open-source, attribute-based memory server for AI agents. Facts keyed by agent-supplied attributes with temporal validity; graph and vector as derived views. Open contract, pluggable backends.",
  "url": "https://memkeys.github.io/",
  "license": "https://opensource.org/licenses/Apache-2.0",
  "offers": { "@type": "Offer", "price": "0", "priceCurrency": "USD" }
}
```

> **Reused brand assets (do not regenerate — point at these):**
> - OG / social card → `branding/_unpacked/memkeys-branding/kit/memkeys-og-1200x630.png`
>   (M3 should copy it to the site's `/og/` path). A GitHub-tuned 1280×640 variant exists at
>   `memkeys-social-github-1280x640.png` if a second ratio is wanted.
> - Favicons → `kit/favicon.ico`, `kit/favicon.svg`, plus the PNG mark ladder
>   (`memkeys-mark-{16,32,48,64,128,256,512}.png`) for `apple-touch-icon` / manifest.
> - Nav / hero logo → the horizontal lockup `memkeys-lockup-horizontal.svg` (use `-reverse`
>   on dark); the standalone `memkeys-mark.svg` for the favicon and compact nav.
> - Verify the licence tag in the JSON-LD against the repo `LICENSE` before launch; if it is
>   not Apache-2.0, M3 should correct the `license` URL.

---

## 2 · Hero

**Eyebrow (optional, small caps above headline):** `OPEN-SOURCE AGENT MEMORY`

**Headline (H1):**
> Every fact has an address.

**Subhead:**
> MemKeys is an open-source, attribute-based memory server for AI agents. Store each fact by
> the keys that describe it — with a validity window — and retrieve it by address. No vector
> guesswork.

**Primary CTA:** `Get started` → quickstart / install section (anchor `#quickstart`)
**Secondary CTA:** `View on GitHub` → `https://github.com/MemKeys/memkeys`

> **Design note (M4a):** the headline is the mark's own story ("@-as-address"). Pair it with
> the mark where the keyhole/`@` ring is visible. Palette: cobalt `#17356F` + one yellow
> accent `#F7C948` used **once** (the keyed thing). Yellow never touches the wordmark letters.

**Tight-layout hero variant (~40 words, for mobile / banner):**
> MemKeys gives AI agents memory with an address. Store each fact by the keys that describe
> it, with a validity window, and retrieve it exactly — no vector guesswork. Graph and vector
> are derived views. Open contract, pluggable backends.

---

## 3 · Value-prop row (three cards, under the hero)

**Card 1 — Keyed-first retrieval**
Reach a fact by its address, not by a similarity guess. Attribute keys — project, issue, run,
agent, thread — bound the candidate set *before* any vector or graph work runs. You get the
fact you meant, not the one that happened to be nearest.

**Card 2 — Memory with a validity window**
Every fact is bi-temporal: it knows when it was true and when it was recorded. Ask "what did
we believe on the 14th?" and get the answer from *then*, not whatever is cached *now*.
Superseded facts close automatically; history is never silently overwritten.

**Card 3 — Open contract, pluggable backends**
A small, open retrieval contract — four operations, one keyed core. Graph and vector are
**derived, rebuildable** views, not a second source of truth. Backends are pluggable, with
the Grail plugin as the reference implementation.

---

## 4 · Section — What it is

**Heading:** What is MemKeys?

Agents produce *facts*: a decision taken on a pull request, the outcome of a run, a learning
derived from telemetry, the context an agent flushes at the end of a session. Every fact is
*about something* — a project, an issue, a PR, an agent, a run, a thread. Those "somethings"
are the **entities**, the agent-supplied attributes, and they are the primary index into
memory.

Instead of dropping each fact into a vector blob and hoping to find it again, MemKeys stores
it with the keys that describe it and a window for when it was true. Retrieval starts from
**what you know** — the keys — and narrows from there. Graph and vector views are built on top
of that keyed core so you still get relationship traversal and semantic search, but they are
*derived projections* you can rebuild at any time, never a parallel store that can drift.

> **Pull quote:** "Store each fact with the keys that describe it, and retrieve it by address —
> the way you pull a card from a catalog."

**Where it fits.** MemKeys is a standalone, attribute-agnostic server. Any agent platform is a
*consumer* of its contract — it does not care whether your keys are `project/issue/pr` or your
own namespace convention. (If you know the MemKeys ecosystem: it is the memory contract that
platforms like AgentMesh and k8squad consume; it is **not** MemPalace, which is a separate
internal layer.)

---

## 5 · Section — Architecture

**Heading:** How it works

MemKeys is organized as **one source of truth and two derived views**, reached through **four
retrieval operations**.

**Storage — single source of truth + derived views:**
- **Keyed / temporal store (source of truth).** Every fact lives here once, append-only and
  bi-temporal. This is what the server writes at commit time.
- **Knowledge graph (derived, async).** Entity→entity relationships, supersedes-chains, and
  cross-fact links projected from the keyed store. Rebuildable by replay.
- **Vector index (derived, async).** Embeddings for semantic recall, scoped by the same keys —
  so similarity search is always *bounded* by the candidate set the keys define.

**Retrieval — four operations:**
- **Store** a fact with its keys and validity.
- **Fetch** by the keys you know (exact, keyed retrieval).
- **Search** semantically *within* a keyed scope.
- **Traverse** the graph from a start node to discover how entities connect.

**The invariant that holds it together (keyed-first):**
> Attribute keys bound the candidate set before any vector or graph work. The keyed / temporal
> store is the single source of truth; graph and vector are derived, rebuildable projections.

**Deploy it your way.** Run the server as a single binary for local dev, or install the Helm
chart (single-node or microservice profile) and the Kubernetes operator for a CRD-driven,
on-cluster deployment. Official images are published to GHCR; the chart ships to an OCI
registry.

> **Design note (M4a):** this section wants a simple diagram — *keyed core in the center,
> graph and vector as derived views branching off it, four operations as the surface*. A static
> SVG is enough; the arrows should make clear the views derive **from** the core (one
> direction), never the reverse. Hand the diagram spec to the Graphic Designer.

---

## 6 · Section — How to use

**Heading:** Get started  *(anchor `#quickstart`)*

Run the memory server locally:

```bash
# Run the MemKeys server on :3000 (loopback dev mode self-asserts the tenant)
cargo run -p memkeys-server
```

Browse your memory with the read-only **Inspector UI** — it talks only to the public read
surface, never to the database, and never writes:

```bash
cd frontend
npm install
npm run dev   # dev server on :3001, proxies /memory → :3000
```

Open `http://localhost:3001`, set the **Tenant** field in the top bar (and a bearer token if
the server runs with a verifier configured), then search facts or traverse the graph.

**Inspector at a glance:**
- **Facts Browser** — keyed retrieval by entity attributes (`project`, `issue`, `pr`, `agent`,
  `run`, `thread`, `service`) with `type` / `valid` / `as_of` filters and a fact-detail view.
- **Memory Graph** — a force-directed view of a traversal from a start node, with direction /
  depth / edge-type controls; truncation and dangling-edge gaps are surfaced, never hidden.

**Going to production.** Install the Helm chart (single-node or microservice profile) and the
operator for a CRD-driven deployment on Kubernetes. See the docs for the chart values, the
`MemKeysInstance` CRD, and the pgvector prerequisite.

**CTA row:**
- `Read the docs` → docs site home (M1 "What is MemKeys" + guides)
- `Connect an agent` → the connect-agents recipes page (M2, ISI-6401)
- `Star on GitHub` → `https://github.com/MemKeys/memkeys`

---

## 7 · Closing CTA band

**Heading:** Memory your agents can actually find again.

**Body:** MemKeys is open source and built in the open. Read the architecture, run it locally
in a minute, and bring your own backend.

**Primary CTA:** `Get started` → `#quickstart`
**Secondary CTA:** `Star on GitHub` → `https://github.com/MemKeys/memkeys`

---

## 8 · Footer microcopy

- **Tagline under the footer lockup:** `Keyed memory for agents.`
- **Repo description / social bio (reuse verbatim):** `Attribute-based memory server for agents — facts keyed by attribute, with temporal validity; graph + vector as derived views. Open contract, pluggable backends.`
- **Nav labels:** `Docs` · `Architecture` · `Inspector` · `GitHub`
- **Legal line:** `MemKeys is open source.` (M3: confirm the licence name against the repo `LICENSE` and link it.)

---

## 9 · Approved taglines (from BRAND-WORDMARK §3 — use as directed)

| Line | Role | Where |
|---|---|---|
| **Every fact has an address.** | Hero | H1 (recommended primary) |
| **Keyed memory for agents.** | Category descriptor | `<title>`, meta description, footer |
| **Memory you reach by key.** | Short active line | Slides, swag, one-liner under mark |
| **Address your agent's memory.** | Verb-forward CTA | Landing CTA (only where storage context is clear) |
| **The librarian of agent memory.** | Personality line | About / narrative copy — **not** the hero |

---

## 10 · Handoff notes

- **To M3 (ISI-6398, DevOps/site build):** metadata in §1 is ready to template; swap the base
  URL when a custom domain lands. Copy brand OG/favicon assets from `branding/.../kit/` into the
  site's public paths (`/og/`, favicons). All copy blocks (§2–§8) map 1:1 to landing sections.
- **To M4a (ISI-6399, Graphic Designer):** two diagram/visual asks are flagged inline — the hero
  mark pairing (§2) and the keyed-core architecture diagram (§5). Palette and the one-yellow-accent
  rule are noted; full brand spec in `BRAND-WORDMARK.md`.
- **Consistency gate:** this page is the front door to the M1 "What is MemKeys" doc (ISI-6397).
  If M1's wording of the keyed-first invariant changes, update §0 and §5 here to match.
- **Do not add fabricated metrics** (benchmarks, adoption numbers, logos) to the landing page
  without a sourced reference; the copy above is deliberately claim-clean.
