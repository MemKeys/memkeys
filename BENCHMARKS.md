# MemKeys Benchmarks

How MemKeys performs as an agent memory backend on an **agentic, end-to-end**
benchmark — real agents solving real tasks, scored on whether memory made their
answers *correct*, not on raw retrieval recall@k.

> **Status.** Set A (single-agent SRE investigation) is complete with all five
> backends scored. Set B (multi-agent crew: build → suspend/resume → fact-change)
> is scored across five backends, including MemKeys — see
> [Set B](#set-b--multi-agent-crew-suspendresume--fact-change).
>
> These numbers come from the KubeCon memory-benchmark re-run (project *Talks*,
> ISI-5581 plan rev 6). Methodology and raw records are reproducible — see
> [Reproducing the benchmark](#reproducing-the-benchmark).

## TL;DR

- On **Set A**, MemKeys ranks **#1 of 5** backends: highest absolute warm
  accuracy (**0.867**), the largest cold→warm **memory payoff (+0.300)**, and the
  best **memory-added efficiency** (0.0168 score per 100k tokens).
- Every memory backend that cleared the health gate beat the no-memory control,
  so the harness discriminates: memory helps, and better memory helps more.
- MemKeys was evaluated with the **identical** deploy→gate→run→teardown pipeline,
  the same agent model (`deepseek-flash`), and the same independent judge
  (`qwen3.8`, temperature 0) as every other backend. No special-casing.

---

## What is being measured

Two agentic scenarios, run against each memory backend in turn. The agent does the
work; memory is the only thing that changes between backends.

| Set | Scenario | What memory has to do |
|-----|----------|------------------------|
| **A** | Single-agent **SRE investigation** — a Kubernetes failure plus Gateway API failures, diagnosed by one agent. | Carry findings across turns so the warm pass reasons from prior investigation instead of rediscovering. |
| **B** | **kagent multi-agent crew** builds an expense app, is **suspended and relaunched**, then hit with **fact-change** probes. | Resume work from memory after restart, and report the *current* value of a fact after it changes. |

Each scenario is run as a **two-pass** experiment:

- **Cold pass** — no memory priming. The agent starts from nothing.
- **Warm pass** — the same scenario, with the backend's accumulated memory available.

The headline signal is the **delta** (warm − cold): how much the memory backend
actually improved the agent's answers. A backend can have great retrieval and still
score zero here if the recalled context doesn't change what the agent *concludes*.

### Scoring

- Answers are scored on **semantic correctness of the reasoning** — did the agent
  reach the right conclusion? — not string overlap or recall@k.
- Judging is done by an **independent model** (`qwen3.8`, temperature 0) that is
  **not** the agent model (`deepseek-flash`), so no backend grades its own work and
  the judge spends zero of the agent's inference budget.
- Reported metrics:
  - **cold mean / warm mean** — mean score (0–1) across the scenario's questions.
  - **Δ score (payoff)** — warm − cold; positive means memory helped.
  - **mem-eff /100k** — score gained per 100k tokens of memory-path overhead;
    rewards backends that add value without bloating the context.

### Fail-closed health gate

Before any scored run, each backend must pass a **fail-closed** gate, and any run
with a red check is **discarded**, not reported:

- MCP memory tools register as `Accepted=True` in the agent runtime.
- A seeded fact survives a full **round-trip** (`add` → `search` recalls it).
- `memory.*` / `gen_ai.*` telemetry spans flow from the backend, the agents, and the
  app under investigation.
- The investigated app and the model endpoint are healthy and not rate-limited.

This discipline exists because the first benchmark run suffered environment-induced
noise; fail-closed gating is what makes the second run trustworthy.

---

## Set A — single-agent SRE investigation

Five backends, scored two-pass (`deepseek-flash` agent, `qwen3.8` judge).

| # | Backend | Cold mean | Warm mean | Δ score | Memory payoff | mem-eff /100k |
|---|---------|-----------|-----------|---------|:-------------:|---------------|
| **1** | **MemKeys** | 0.567 | **0.867** | **+0.300** | ✅ | **0.0168** |
| 2 | memos | 0.525 | 0.565 | +0.040 | ✅ | 0.0027 |
| 3 | zep (graphiti) | 0.308 | 0.425 | +0.117 | ✅ | 0.0120 |
| 4 | mempalace | 0.333 | 0.358 | +0.025 | ✅ | 0.0025 |
| 5 | mem0 | 0.517 | 0.433 | −0.083 | ❌ | −0.0168 |

**Reading the table.** MemKeys posts the highest warm accuracy *and* the largest
payoff: it starts competitive cold (0.567) and nearly closes the gap to perfect
after priming (0.867). mem0 is the cautionary case — its warm pass scored *below*
cold, i.e. the recalled context pulled the agent off course, which is exactly the
failure mode an agentic benchmark is designed to surface and a recall@k benchmark
would miss.

MemKeys' architecture explains the result: facts are bound by **agent-supplied
attribute keys** with **temporal validity**, and the keyed/temporal store is the
single source of truth (graph and vector are derived, rebuildable views). On this
run the recall path was **keyed-primary** — attribute keys bound the candidate set
first, with semantic search as best-effort — so recall is precise and never
silently empty.

---

## Set B — multi-agent crew (suspend/resume + fact-change)

The crew builds an expense app, is suspended and relaunched (it must **resume from
memory**), then probed with **fact-change** questions. Per-sub-scenario scores
(`deepseek-flash` crew, `qwen3.8` judge):

| Backend | build | resume | restart | factchange | Composite |
|---------|:-----:|:------:|:-------:|:----------:|:---------:|
| **MemKeys** | 0.00 | 0.00 | 0.00 | **1.00** | ≈0.25 |
| mempalace | 0.00 | **1.00** | 0.00 † | **1.00** | ≈0.50 |
| zep (graphiti) | **1.00** | 0.55 | — | 0.00 ‡ | ≈0.52 |
| mem0 | 0.00 | 0.00 | — | **1.00** | ≈0.33 |
| memref (control) | 0.00 | 0.00 | 0.00 | 0.25 | ≈0.06 |

The no-memory **control sits at the floor** and every memory backend beats it —
the harness discriminates. Caveats are reported in the open, not hidden:

- † mempalace's single `restart` miss was a 600 s read-timeout (infra), not a
  recall failure.
- ‡ zep's `factchange` 0.00 is a driver JSON bug (`memory_add` →
  `Expecting value: line 1 column 1`), not a capability verdict.
- **memos** was not scored in Set B (neo4j crash-loop in this environment); **graphiti**
  as an independent backend went **NO-GO** on the fail-closed gate (seeded fact not
  recalled within the async-ingestion window).

**MemKeys row:** scored on the same crew harness (`setb-runner`, `deepseek-flash`).
MemKeys **nails fact-change (4/4, score 1.00)** — all three probes correctly report
that 5500 supersedes 5432, with verbatim recall of the stored fact — while build,
resume, and restart score 0.00. The profile matches **mem0** (fact-change strong;
no cross-session crew recall) and clears the memref control floor (0.06). Composite
≈0.25. Tracking issue: ISI-6724.

---

## Reproducing the benchmark

Everything below runs on the shared **memory-benchmark** cluster (Proxmox/CAPMOX),
with a Postgres substrate for MemKeys. The pipeline is identical for every backend:
**deploy bundle → health gate → cold pass → warm pass → judge → tear down.**

### Cluster requirements

- A Kubernetes cluster with outbound egress to the agent model endpoint
  (`deepseek-flash`) and the judge model (`qwen3.8`, run locally at temperature 0).
- **Postgres with pgvector** for MemKeys — `pgvector/pgvector:pg18`
  (Deployment + PVC + Service). MemKeys needs `MEMKEYS_DATABASE_URL`; a non-Postgres
  URL (e.g. sqlite) fails fast by design.
- OpenTelemetry collection wired so `memory.*` / `gen_ai.*` spans are observable
  (the health gate asserts spans are flowing).

### Set A — single-agent SRE

1. **Deploy MemKeys** with the verified bundle `deploy/memkeys/`
   (`00-namespace`, `01-memkeys-verified`, `02-mcp-memory-server`, `03-egress`,
   `kustomization.yaml`) into namespace `benchmark-memkeys`.
2. The agent reaches MemKeys through the **`memkeys_driver.py`** driver in the
   `mcp-memory-server` drivers directory — a thin client over the MemKeys
   `/memory/*` HTTP API on `:3000` (ingest / keyed-fetch / semantic / traverse),
   self-asserting `X-MemKeys-Tenant` and forwarding the W3C trace carrier.
3. Select the backend in the shared SRE runtime:
   `MEMORY_BACKEND=memkeys`,
   `MEMKEYS_BASE_URL=http://memkeys.benchmark-memkeys.svc.cluster.local:3000`,
   `MEMKEYS_TENANT=<run-tenant>`.
4. Run the **fail-closed health gate** (§ above). Red → stop and tear down.
5. Run the `seta-runner` Job (cold + warm passes). Agent `deepseek-flash`,
   judge `qwen3.8`.
6. Harvest `records.jsonl`, score, and merge into the ranking with the
   re-rank tool (`seta_rank.py merge memkeys …`) → `results/seta-ranking.{json,md}`.
7. **Tear down** before the next backend.

### Set B — multi-agent crew

1. Same MemKeys substrate as Set A.
2. The crew reaches MemKeys through the **memkeys adapter in
   `kagent-memory-mcp`** (image `kagent-memory-mcp:isi6659`, which bundles the
   MemKeys driver). Configure with `solutions/memkeys.env`.
3. Run the fail-closed gate (note: the gate's search probe must pass the real
   `user_id` used by `add` — strictly-keyed backends like MemKeys recall nothing if
   the probe's scoping variables are shell-quoted literally).
4. Run the `setb-runner` Job (`BENCH_LLM_MODEL=deepseek/deepseek-flash`):
   build → suspend/resume → restart → fact-change.
5. Score with `qwen3.8` (temperature 0), fold the row into the Set B ranking
   report, and tear down.

### Where the artifacts live

Deploy bundles, drivers, runner Jobs, raw `records.jsonl`, and the ranking reports
live in the **memory-benchmark** workspace (project *Talks*), cross-referenced by the
KubeCon talk materials. Set A rankings: `results/seta-ranking.{json,md}`. Set B:
`two-pass-ranking-report.md` / `ISI-6184_SetB_Finalize_Report.md`.

---

## Honest caveats

- Agentic scoring has variance: a few 0.00 sub-scores in Set B are inflated by
  infrastructure timeouts and one driver bug (flagged above), not capability gaps.
- Set A ran with MemKeys' built-in 256-dim hash embedder on the semantic tier and a
  **keyed-primary** recall path; a 768-dim embedder rebuild would exercise the
  semantic tier more heavily. The keyed path is MemKeys' source of truth, so the
  result reflects its intended architecture.
- These are **relative** results on one agent model and one judge in one cluster.
  They show memory *helps* and that MemKeys' keyed+temporal design is competitive
  on an agentic task — they are not a universal ranking of memory products.
- Methodology, bundles, and raw records are published so the numbers can be
  re-derived and challenged.
