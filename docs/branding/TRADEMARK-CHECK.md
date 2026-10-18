# MemKeys — Informal Trademark Clearance

**Mark assessed:** `MemKeys` (word mark; also `Mem Keys`)
**Context:** Software / developer infrastructure — an attribute-based memory server. Relevant Nice classes: **9** (software) and **42** (SaaS / software-as-a-service).
**Follow-up from:** naming decision (ISI-5803).
**Date:** 2026-10-17
**Author:** Research Engineer (Paperclip), with Claude.

> ⚠️ **This is an informal, web-level screen — not a legal opinion and not a substitute for professional clearance.** It was performed by reading publicly indexed sources. The authoritative trademark registers (USPTO and EUIPO) are served through interactive JavaScript search forms that could not be queried programmatically from this environment, so their live indexes were **not** directly searched. Before any public launch, paid marketing, or trademark filing, obtain a formal clearance opinion from a qualified trademark attorney and a live register search (see *Recommended next steps*).

---

## Verdict: 🟡 YELLOW — clear to use, caution on registrability

No registered trademark for `MemKeys` / `Mem Keys` in classes 9 or 42 surfaced at the web-screen level, and there is **no evidence of an active commercial competitor** using the exact name in our category. That supports going ahead with the name for use. The caution (yellow rather than green) is driven by two distinctiveness/crowding factors, not by a known conflict:

1. **The term is weak and crowded in the memcache-tooling niche.** Several identical-named open-source tools already exist (`MemKeys`/`memkeys`), all meaning roughly "memcache keys." A name that reads as descriptive in an adjacent software space is harder to register and harder to enforce.
2. **Near-identical singular marks exist in adjacent consumer software** (`MemKey`, a password manager and, separately, a meme keyboard). These are not in our lane and are small/indie, but they narrow the distinctiveness of the root.

Net: **low risk for *use*, moderate risk for *protectability*.** Proceeding is reasonable; a formal search + attorney opinion is advised before relying on the mark as an enforceable asset.

---

## 1. Register searches — USPTO (TESS) and EUIPO (TMview)

| Register | Tool | Classes | Result | Confidence |
|---|---|---|---|---|
| USPTO (US) | Trademark Search (`tmsearch.uspto.gov`, the successor to the retired TESS) | 9, 42 | No `MemKeys` / `Mem Keys` word mark found at web-screen level. Only an unrelated mark, `MEPADKEY`, was indexed. | Low — live register not queried directly |
| EUIPO (EU) | TMview / eSearch (`tmdn.org`, `euipo.europa.eu`) | 9, 42 | No `MemKeys` / `Mem Keys` word mark surfaced at web-screen level. | Low — live register not queried directly |

**Important limitation:** USPTO's TESS was retired and replaced by the Trademark Search system at `tmsearch.uspto.gov`; EUIPO search runs through TMview/eSearch. Both are interactive single-page apps that return nothing to a plain HTTP fetch, so these rows reflect *absence of a web-indexed hit*, not a confirmed clear register. A definitive answer requires running the two live searches (US classes 9 & 42; EU classes 9 & 42) by hand or via their official APIs.

Sources:
- USPTO trademark search entry point — https://www.uspto.gov/trademarks/search and https://tmsearch.uspto.gov/
- EUIPO search tools overview — https://trademarkfactory.com/blog/euipos-trademark-search-tools-and-databases/
- (unrelated indexed mark) MEPADKEY — https://www.trademarkia.com/mepadkey-97704714

## 2. Web & app-store landscape — products named "MemKeys" / "MemKey"

### 2.1 Exact name `MemKeys` (plural) — all are dormant developer/memcache tools

| Product | What it is | Status | Source |
|---|---|---|---|
| `bmatheny/memkeys` | A `top`-like CLI for inspecting memcache key values in realtime (C++, Apache-2.0, "Copyright 2013 Tumblr"). The tool named in the ISI-5920 brief. | Dormant (latest release v0.2; no recent activity). | https://github.com/bmatheny/memkeys |
| `tumblr/memkeys` | Upstream organisation copy of the same memcache tool. | Dormant. | https://github.com/tumblr/memkeys |
| `MikeRogers0/Memkeys` | Small Ruby tool to list keys in memcache. | Dormant. | https://github.com/MikeRogers0/Memkeys |
| `memkeys` (RubyGem) | Ruby gem, v0.1.2 — list memcache keys. | Dormant. | https://rubygems.org/gems/memkeys |

Every exact-match `MemKeys` result is an open-source, non-commercial, inactive memcache utility. The name functions **descriptively** there ("mem[cache] keys"). None appears to be a registered trademark or an active commercial product. This is good for conflict risk but is exactly what weakens the mark's inherent distinctiveness in software tooling.

### 2.2 Near-match `MemKey` (singular) — two small consumer apps

| Product | What it is | Relevance | Source |
|---|---|---|---|
| **MemKey** — offline password manager | 100%-offline consumer password vault (PIN/fingerprint, encrypted import/export). Developer `com.ms.utileprog.memkey`. | Closest *live* collision: a security/consumer app, arguably class 9/42, but a different market and singular spelling. Small/indie. | https://play.google.com/store/apps/details?id=com.ms.utileprog.memkey |
| **MemKey — the memes keyboard** | Meme-search keyboard by Mario Varona / bmsalamanca; Google Play + App Store. | Adjacent consumer category, last updated ~2019, likely dormant. | https://play.google.com/store/apps/details?id=com.MarioVarona.MemKey · https://www.bmsalamanca.com/MemKey/ |
| `mymmrac/memkey` | Go type-safe in-memory key-value store (OSS library). | Developer tool, singular spelling, dormant/small. | https://github.com/mymmrac/memkey |

### 2.3 Not relevant
Memsource (translation SaaS), Prodikeys, Netkey, MEMZ — surfaced by fuzzy matching only; no naming overlap.

## 3. Risk analysis

**Factors lowering risk (toward green):**
- No registered `MemKeys` word mark found in classes 9/42 at the web-screen level.
- No active commercial product uses the exact name in our category (attribute-based memory server / AI memory infrastructure).
- The identical-name prior uses are inactive, non-commercial OSS utilities in a *different* technical niche (memcache inspection), which rarely form the basis of an enforceable senior commercial right.

**Factors raising risk (toward yellow/red):**
- **Descriptiveness / crowding:** multiple identical OSS names meaning "memcache keys" suggest the term is suggestive-to-descriptive in software tooling, reducing registrability and enforceability.
- **Adjacent singular marks** (`MemKey` password manager; `MemKey` keyboard) dilute the root and could draw an examiner/opposition citation, especially the security app in class 9.
- **Developer-channel confusion:** package registries, GitHub search, and `memkeys` as a common term could cause SEO/namespace collisions (npm/crates/pypi/GitHub org handles).
- Register searches were **not** run live — a genuine conflict could exist that is not web-indexed.

## 4. Recommended next steps (before relying on the mark)

1. **Run live register searches** in US classes 9 & 42 at `tmsearch.uspto.gov` and in EU classes 9 & 42 via TMview/eSearch (or their official APIs). Record serial/registration numbers of any hits.
2. **Engage a trademark attorney** for a formal clearance opinion if the name will anchor branding, fundraising, or a filing.
3. **Lock down namespaces now** (low cost, high value): domain(s), GitHub/GitLab org, and package handles on npm / crates.io / PyPI to avoid the `memkeys` developer-channel collision.
4. **Strengthen distinctiveness:** pair the word mark with distinctive stylization/logo and always use it as a proper noun (`MemKeys™ memory server`) so it reads as a brand, not a description of "memcache keys."
5. **Re-screen** if the product category or go-to-market geography changes.

## 5. Methodology & limitations

- Screen performed 2026-10-17 using publicly indexed web search and direct reads of GitHub, RubyGems, and Google Play pages (cited inline).
- Live USPTO Trademark Search and EUIPO TMview indexes were **not** directly queried (interactive JS apps not reachable from this environment). Register rows above reflect absence of a *web-indexed* hit only.
- No common-law / unregistered-rights investigation beyond the product landscape above.
- This document is informational and **not legal advice.**

---

*Deliverable for ISI-5920 (follow-up to naming decision ISI-5803).*
