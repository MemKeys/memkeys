# Configuration

MemKeys is configured by environment variables. This page is the full
reference for the server; the [Helm chart](/memkeys/k8s-operator/) and the
[Kubernetes operator](/memkeys/k8s-operator/#spec-reference-v1alpha1) map
their values onto these. For what an instance needs provisioned end to end —
Postgres + pgvector, which model endpoints gate which feature — see the
[instance requirements](/memkeys/instance-requirements/) page. To get a server running first, see the
[Quickstart](/memkeys/quickstart/).

## Required

| Variable | Purpose |
|---|---|
| `MEMKEYS_DATABASE_URL` | Postgres connection string (`postgres://user:pass@host:5432/db`). **The Postgres server must have the [pgvector](https://github.com/pgvector/pgvector) extension available** — a boot migration runs `CREATE EXTENSION vector` and the server preflights for it. Stock `postgres:*` images fail at startup with a clear message; use `pgvector/pgvector:pg16` (or pg17/pg18) or a managed Postgres with pgvector enabled. Only `postgres://` and `postgresql://` schemes are accepted — the server rejects other schemes up front instead of hanging on connect. |

## Roles

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_ROLE` | *(all-in-one)* | Which surfaces this process runs. Unset or unrecognized → **all-in-one**: health + `/memory/*` HTTP + stdio MCP + the projection worker stack. `worker` → projection-only (health + reconciler; no memory transport) — the role the microservice profile's worker Deployment runs. `mcp` → stdio MCP **only**: no listeners, no boot reconcile, no maintenance loop — the per-session agent transport; pair it with a small pool (below). |

## Listeners

The server runs **two distinct listeners** — never confuse them:

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_HTTP_HOST` | `127.0.0.1` | Bind host for both listeners. Non-loopback binds **require** `MEMKEYS_RUN_TOKEN_SECRET` or the explicit `MEMKEYS_ALLOW_INSECURE=1`, and are refused at startup otherwise. |
| `MEMKEYS_HTTP_PORT` | `8080` | Health/ops surface: `/healthz`, `/readyz`. Probes only. |
| `MEMKEYS_MEMORY_HTTP_PORT` | `3000` | The memory API: `/memory/*`. This is the port clients target. |

On boot each listener logs its own line (`memkeys.server.health.listen` /
`memkeys.server.memory.listen`) so the two surfaces are never confused.

## Authentication

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_RUN_TOKEN_SECRET` | — | Shared HS256 secret for run tokens (the server verifies; your issuer mints). When set, every bearer / launch token is verified and the **tenant floor is derived server-side** from the token claims. Absent → open loopback dev mode (tenant self-asserted via the `x-memkeys-tenant` header). |
| `MEMKEYS_RUN_TOKEN` | — | A launch-bound HS256 principal (JWT). Claims: `company_id` → tenant, `sub` → `entity.agent` / provenance principal, `exp` → expiry, re-checked on every call. Used by the MCP stdio transport; see [MCP transport](/memkeys/agent-mcp-transport/). |
| `MEMKEYS_STDIO_TENANT` | `default` | Tenant for the unverified dev principal when no run token is configured (loopback dev only). |
| `MEMKEYS_ALLOW_INSECURE` | — | `1` permits a non-loopback bind with no verifier, with a loud warning. Dev only; do not ship. |

## Semantic module

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_SEMANTIC` | off | `1` (or `on` / `true`) enables the vector tier. Off → `/memory/search` and the `memory_search` tool return `feature_disabled` (501 over HTTP) rather than empty results. |
| `MEMKEYS_SEMANTIC_DIGEST` | off | `1` additionally enables the digest tier (scope rollup summaries, projected today; they join the search path as the digest read path matures). See [Architecture — vector index](/memkeys/architecture/#vector-index-two-tiers-optional-module). |
| `MEMKEYS_EMBED_URL` | — | OpenAI-compatible `/embeddings` endpoint (`POST` `{"model","input"}` → `{"data":[{"embedding":[…]}]}`; Ollama / vLLM / text-embeddings-inference all speak this). Set → the HTTP embedder; unset → the built-in deterministic 256-dim `HashEmbedder`. Both tiers must use the same embedder as the process that wrote the vectors. |
| `MEMKEYS_EMBED_MODEL` | — | Embedder model id. Sent as the `/embeddings` request `model` **and** stamped as the derivation version (a change re-embeds prior vectors). Unset → the `model` field is omitted (single-model servers) and the stamp is derived from the URL. |
| `MEMKEYS_EMBED_DIM` | `256` | Embedder output width. Must match the served model (e.g. `nomic-embed-text` = `768`). The fact-vector column is reconciled to this width at startup: widened in place on a fresh table, or a fail-fast error on a populated mismatch (truncate to let the reconciler re-embed). Max 2000 (HNSW ceiling). |
| `MEMKEYS_GEN_URL` | — | Generative endpoint (summarizer) for digest rollups. |
| `MEMKEYS_GEN_MODEL` | — | Summarizer model id. |

Reference defaults are small open models; no proprietary endpoint is
required. Every derived artifact is stamped with
`(model_id, prompt_version)` so changing models triggers a clean rebuild of
projections, never a silent mismatch.

### Recency re-rank

The fact-vector read path re-ranks the cosine candidates by a hybrid of
semantic similarity and recency decay
(`score = α·cosine + (1−α)·exp(−λ·age_seconds)`), so the most *recent* relevant
fact is preferred over a stale-but-similar one — the signal that drove the
keyed-path benchmark win. See
[ADR-0002](https://github.com/MemKeys/memkeys/blob/main/docs/adr/ADR-0002-hybrid-recency-semantic-rerank.md).
These knobs only take effect when `MEMKEYS_SEMANTIC` is on; an unparseable
value falls back to the default rather than failing boot.

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_RECENCY_ALPHA` | `0.6` | Semantic weight α ∈ [0, 1]. **α = 1.0 reproduces pure-cosine order exactly** — the graceful kill-switch, no separate feature flag. Lower α weights recency more. |
| `MEMKEYS_RECENCY_LAMBDA` | `1e-6` | Decay rate λ per second in `exp(−λ·age_seconds)`. Default e-folding ≈ 11.6 days; larger λ decays faster (e.g. `1.1e-6` ≈ 10.5 d, `2.7e-6` ≈ 4.3 d). |
| `MEMKEYS_RECENCY_CANDIDATES` | `200` | Oversample pool size N: how many cosine-ranked candidates the HNSW pass returns before the re-rank truncates to `k`. N is a recall ceiling — a genuinely-recent fact outside the cosine top-N can't be recovered; raise it for large tenants. |

Age is measured from the read's point-in-time viewpoint
(`reference_time = as_of` when set, else now), so an `as_of` query scores
"recent" as of that historical time — not relative to wall-clock now.

## Database pool

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_PG_MAX_CONNECTIONS` | `8` | Pool size, tunable per role. Set `2` for per-session `MEMKEYS_ROLE=mcp` processes: each serves one sequential stdio session, and a fleet at the default pool would exhaust Postgres `max_connections` (default 100). |

## Telemetry

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_DEPLOYMENT_ENV` | — | OTel deployment environment tag (`dev`, `prod`, …). |
| `MEMKEYS_SERVICE_NAMESPACE` | — | OTel service.namespace. |
| `MEMKEYS_TRACES_SAMPLER_RATIO` | — | Trace sampling ratio (0.0–1.0). |

Logs and traces never target stdout — stdout belongs to the MCP JSON-RPC
stream when the stdio transport is on.

## Deployment

The [Helm chart](/memkeys/k8s-operator/) and the
[operator](/memkeys/k8s-operator/#spec-reference-v1alpha1) render these
variables for you. Container images:

| Image | Contents |
|---|---|
| `ghcr.io/memkeys/memkeys-server` | The server (root `Dockerfile`) |
| `ghcr.io/memkeys/memkeys-operator` | The Kubernetes operator |
| `ghcr.io/memkeys/memkeys-inspector` | The read-only Inspector UI (nginx + static SPA) |

The chart is published as an OCI artifact:
`oci://ghcr.io/memkeys/charts/memkeys`. Kubernetes ≥ 1.27 for the operator;
the chart requires pgvector-enabled Postgres either way.
