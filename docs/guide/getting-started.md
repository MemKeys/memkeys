---
title: Getting started
description: Bring up Postgres + MemKeys with Docker Compose, make your first store / fetch / traverse / search calls against /memory/*, and browse memory with the Inspector UI.
sidebar:
  order: 3
---

# Getting started

This walkthrough brings up a single-node MemKeys on your laptop with Docker
Compose, stores a fact, reads it back four ways, and browses it in the Inspector
UI. It is a **dev** setup — it runs the open, self-asserted-tenant posture on
loopback. For a hardened deployment see the [CRD reference](./crd-reference.md) (or
the [Helm chart](../../charts/memkeys/README.md)).

## Prerequisites

- Docker with Compose v2 (`docker compose`).
- A clone of the MemKeys repo (the compose file and server `Dockerfile` live here).

> **pgvector is required — not optional.** A boot migration runs
> `CREATE EXTENSION vector`. A stock `postgres:*` image does **not** ship pgvector,
> so the server preflights the extension at startup and exits with a clear message
> rather than failing mid-migration. The compose file below uses
> `pgvector/pgvector:pg18` for exactly this reason. For a managed Postgres, make sure
> pgvector is available there.

## 1. Bring up Postgres + MemKeys

The repo ships [`deploy/docker-compose.yaml`](../../deploy/docker-compose.yaml),
which starts Postgres and the all-in-one server (the Helm chart's
`single-node` profile). That file exposes only the **health** surface on `:8080`.
To exercise the product API (`/memory/*`) from your host, use this small
dev override, saved as `deploy/docker-compose.dev.yaml`:

```yaml
# deploy/docker-compose.dev.yaml — dev-only: exposes /memory/* with NO auth on loopback.
services:
  memkeys:
    environment:
      # Expose the /memory/* transport off loopback (inside the container network).
      MEMKEYS_HTTP_HOST: "0.0.0.0"
      MEMKEYS_MEMORY_HTTP_PORT: "3000"
      # Dev-only: bind off-loopback with no run-token verifier. The tenant is
      # self-asserted via the X-MemKeys-Tenant header. NEVER do this in production.
      MEMKEYS_ALLOW_INSECURE: "1"
      # Turn on the semantic module so /memory/search works.
      MEMKEYS_SEMANTIC: "1"
    ports:
      - "3000:3000"   # /memory/* transport
```

Start both files together:

```bash
docker compose \
  -f deploy/docker-compose.yaml \
  -f deploy/docker-compose.dev.yaml \
  up --build
```

Wait for the server to log `memkeys-server: HTTP listening on 0.0.0.0:3000`, then
confirm it is healthy:

```bash
curl -s localhost:8080/healthz   # liveness
curl -s localhost:8080/readyz    # readiness — pings the DB
```

## 2. Environment essentials

The knobs you will actually set. All are read by the server at startup.

| Variable | Default | What it does |
|---|---|---|
| `MEMKEYS_DATABASE_URL` | — (required) | Postgres connection string. Must be a Postgres URL with pgvector available. |
| `MEMKEYS_ROLE` | *(all-in-one)* | Role selector. Unset = server + inline projection. `worker` = projection-only (no transport). `mcp` = stdio MCP only (no HTTP, no workers). |
| `MEMKEYS_SEMANTIC` | off | `1`/`on`/`true` enables the vector module (embedding on write + `/memory/search`). Off → `/memory/search` returns `501 feature_disabled`. |
| `MEMKEYS_HTTP_HOST` | `127.0.0.1` | Bind address for the `/memory/*` transport. Loopback by default; a non-loopback bind requires a verifier or `MEMKEYS_ALLOW_INSECURE=1`. |
| `MEMKEYS_MEMORY_HTTP_PORT` | `3000` | Port for the `/memory/*` transport. Distinct from the health port. |
| `MEMKEYS_HTTP_PORT` | `8080` | Port for the health surface (`/healthz`, `/readyz`). |
| `MEMKEYS_RUN_TOKEN_SECRET` | — | HS256 issuer secret for the per-request bearer verifier. **Set this to leave dev mode:** the transport then verifies `Authorization: Bearer <jwt>` and derives the tenant from the token claims (`company_id` → tenant). |
| `MEMKEYS_ALLOW_INSECURE` | off | Dev-only escape hatch: bind off-loopback with **no** verifier (tenant self-asserted). Logs a loud warning. |

In the dev setup above there is no verifier, so every request **must** carry an
`X-MemKeys-Tenant` header naming the tenant it operates in. We use `demo` below.

## 3. Your first `/memory/*` calls

### Store a fact

```bash
curl -s localhost:3000/memory/facts \
  -H 'content-type: application/json' \
  -H 'x-memkeys-tenant: demo' \
  -d '{
    "source": { "kind": "telemetry_learning", "emitted_at": "2026-10-21T10:00:00Z" },
    "entity": { "project": "demo-project" },
    "fact": {
      "type": "learning",
      "body": "pgvector is required — the boot migration runs CREATE EXTENSION vector.",
      "subject": "pgvector-requirement",
      "confidence": 0.9
    }
  }'
```

The response echoes the server-resolved keys and the new `fact_id` — note the id,
you will traverse from it below:

```jsonc
{ "fact_id": "<FACT_ID>", "keyed": true, "entity_keys_resolved": { "project": "demo-project", "agent": "…" } }
```

### Fetch by keys

Everything currently known about the project, newest first:

```bash
curl -s 'localhost:3000/memory/facts?entity.project=demo-project' \
  -H 'x-memkeys-tenant: demo'
```

At least one `entity.*` key is required; add `&type=learning`, `&valid=all` (include
superseded history), or `&as_of=<RFC3339>` (point-in-time replay) to narrow.

### Traverse the graph

Walk out from the fact you just stored to the entity nodes it links to:

```bash
curl -s localhost:3000/memory/graph/traverse \
  -H 'content-type: application/json' \
  -H 'x-memkeys-tenant: demo' \
  -d '{
    "start": { "kind": "fact", "id": "<FACT_ID>" },
    "direction": "both",
    "depth": 2,
    "hydrate": true
  }'
```

Omit `edges` to walk every edge type, or pass a subset (e.g.
`["entity_fact","supersedes"]`) to filter. `hydrate: false` returns graph topology
only. (Graph projection is async — if the traversal comes back empty, give the
reconciler a moment and retry.)

### Search semantically

Requires `MEMKEYS_SEMANTIC=1` (set in the dev override):

```bash
curl -s localhost:3000/memory/search \
  -H 'content-type: application/json' \
  -H 'x-memkeys-tenant: demo' \
  -d '{
    "query": "do I need a special Postgres image?",
    "scope": { "entity.project": "demo-project" },
    "k": 5
  }'
```

Search is always bounded by your tenant. Every hit resolves down to its
authoritative fact record, so the `body` you get back is ground truth, never a
summary.

## 4. Browse with the Inspector UI

The Inspector is a read-only React SPA for browsing memory. It talks **only** to the
public read surface (`GET /memory/facts`, `POST /memory/graph/traverse`) — never to
the database, and it never writes. It lives in [`frontend/`](../../frontend/).

```bash
cd frontend
npm install
npm run dev     # dev server on :3001, proxies /memory → :3000
```

Open <http://localhost:3001>, set the **Tenant** field in the top bar to `demo` (and
a bearer token if you later run the server with a verifier), then:

- **Facts Browser** — keyed retrieval by entity attributes (`project`, `issue`,
  `pr`, `agent`, `run`, `thread`, `service`), with `type` / `valid` / `as_of`
  filters and a fact-detail view.
- **Memory Graph** — a d3 force-directed view of a traversal from a start node, with
  direction / depth / edge-type controls. Truncation and dangling-edge gaps are
  surfaced, never silently hidden.

Build static assets for production with `npm run build` (output in `frontend/dist/`)
and serve them from the **same origin** as the API so the relative `/memory` base
resolves.

## Next steps

- **[Architecture](./architecture.md)** — the envelope, the write pipeline, and the
  four-op contract in depth.
- **[CRD reference](./crd-reference.md)** — run MemKeys on Kubernetes with the
  operator and a hardened, run-token-verified transport.
