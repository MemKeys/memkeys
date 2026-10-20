# MemKeys Helm chart

Deploys [memkeys-server](../../crates/memkeys-server) — the attribute-based
memory server — to Kubernetes. Covers **both deployment profiles** (M10a):
a single all-in-one node and a K8s microservice split with async-projection
workers. For a Kubernetes-free container/VM quickstart, skip to
[Single-container quickstart](#single-container-quickstart-no-kubernetes).

## TL;DR

```sh
kubectl create secret generic memkeys-run-token \
  --from-literal=MEMKEYS_RUN_TOKEN_SECRET=<issuer-secret>
helm install memkeys charts/memkeys \
  --set database.url="postgres://user:pass@host:5432/memkeys" \
  --set memoryTransport.runTokenSecret.existingSecret=memkeys-run-token
```

## Deployment profiles

The `profile` value selects which workloads render. An invalid value fails
`helm template`/`helm install` fast.

| `profile` | Renders | Use for |
|-----------|---------|---------|
| `single-node` (default) | one all-in-one `Deployment` (server = writer + inline projection) | single VM/container, laptops, dev, small installs |
| `microservice` | server `Deployment` **+** a separate async-projection worker `Deployment` **+** a `PodDisruptionBudget` | scaled K8s where projection rebuild is isolated from the request path |

```sh
# Microservice profile: server + workers + PDB.
helm install memkeys charts/memkeys \
  --set profile=microservice \
  --set database.url="postgres://user:pass@host:5432/memkeys"
```

Workers run the same image as the server in the **worker role**: the template
sets `MEMKEYS_ROLE=worker`, which makes `memkeys-server` start projection-only
(health surface + projection reconciler; no `/memory/*` HTTP transport and no
stdio MCP — see `crates/memkeys-server/src/main.rs`). `workers.command` /
`workers.args` remain available for custom entrypoints. Tune with
`workers.replicaCount`, `workers.resources`, and the usual scheduling keys
under `workers.*`. Force the worker Deployment on/off independently of the
profile with `--set workers.enabled=true|false` (same for
`podDisruptionBudget.enabled`).

The server `Service` selects `app.kubernetes.io/component=server`, so it never
routes to worker pods. The Service fronts **both** listeners: the **health
surface** (the `/healthz`/`/readyz` listener, `service.port`, probe-only) and
the **`/memory/*` transport** (`memoryTransport.port`, default 3000 — the port
clients target). `service.port` is pinned to the container's `MEMKEYS_HTTP_PORT`
so the Service port, container port, and probe listener always agree; the
transport has its own `MEMKEYS_MEMORY_HTTP_PORT` listener. See
[Memory transport](#memory-transport-memory).

**Replicas & the PDB.** `replicaCount` defaults per profile: **1** for
single-node, **2** for microservice (HA). The `PodDisruptionBudget` renders
only when the effective server replica count is **> 1** — a PDB with
`minAvailable: 1` on a single replica would make `kubectl drain` block forever.
So `--set profile=microservice --set replicaCount=1` yields no PDB (by design);
set `replicaCount >= 2` (the default) to get one.

> **Upgrading from 0.1.0.** The server `Service` selector gains
> `app.kubernetes.io/component=server`. Pods from a running 0.1.0 release lack
> that label, so during `helm upgrade` the Service has no endpoints until the
> first relabeled pod is Ready. On a single-replica install this can be a brief
> blip; run `replicaCount >= 2` (or a quick `helm uninstall`/`install` for the
> M1 bootstrap) to avoid it.

### Operator hooks

Every rendered resource carries `app.kubernetes.io/part-of=memkeys` so an
operator or GitOps controller can select the whole release. Merge extra
selection labels/annotations with `operator.labels` and `operator.annotations`
(labels go on every resource; annotations go on the workloads). No operator
ships with this chart yet — these are forward-looking extension points.

## Memory transport (/memory/*)

`memkeys-server` runs **two distinct listeners**: the health surface
(`/healthz`/`/readyz`) on `MEMKEYS_HTTP_PORT` (`service.port`) and the actual
product API, the `/memory/*` transport, on `MEMKEYS_MEMORY_HTTP_PORT`
(`memoryTransport.port`, default 3000). Sharing one port is impossible — the
second bind would collide — so the chart exposes the transport as its own
container/Service port and binds it `0.0.0.0` (`MEMKEYS_HTTP_HOST`), because
the server's loopback default is unreachable from other pods.

Exposing an unauthenticated admission surface off loopback is a cross-tenant
impersonation hole (ISI-6137 S1), and the server **refuses** a non-loopback
bind without a run-token verifier. The chart mirrors that gate and **fails
`helm template`/`helm install` fast** when `memoryTransport.enabled=true`
(the default) and none of these is set:

| Value | Effect |
|---|---|
| `memoryTransport.runTokenSecret.existingSecret` | reference a pre-provisioned Secret holding the run-token issuer secret (key `MEMKEYS_RUN_TOKEN_SECRET`, override with `.key`) — **preferred** |
| `memoryTransport.runTokenSecret.value` | inline issuer secret → the chart renders Secret `<release>-run-token` (dev convenience; ends up in Helm release storage) |
| `memoryTransport.allowInsecure=true` | dev-only: bind `0.0.0.0` with NO verifier (server warns; tenant is self-asserted) |
| `memoryTransport.enabled=false` | render the health-only surface (previous M10a behaviour) |

Other constraints, also validated fail-fast: `memoryTransport.port` must
differ from `service.port` (listener collision) and `memoryTransport.portName`
must differ from `service.portName`.

When `httpRoute.enabled=true`, the `HTTPRoute` `backendRefs` port targets the
**memory transport** (`memoryTransport.port`) — the health port stays
probe-only. Clients authenticate with bearer run tokens signed by the issuer
secret.

**Upgrading from 0.2.0.** The memory transport exposure is new and on by
default; a bare `helm upgrade` now fails fast until you provide a credential
(above) or set `memoryTransport.enabled=false`.

## Single-container quickstart (no Kubernetes)

For a single VM or laptop, [`deploy/docker-compose.yaml`](../../deploy/docker-compose.yaml)
brings up Postgres + memkeys-server (container-first, the single-node profile
without Kubernetes):

```sh
# From the repo root — builds the image from the Dockerfile and starts both.
docker compose -f deploy/docker-compose.yaml up --build
# memkeys-server listens on http://localhost:8080 ; Postgres on localhost:5432.
```

Set `MEMKEYS_IMAGE` (e.g. `ghcr.io/memkeys/memkeys:<tag>`) to run a prebuilt
image instead of building locally.

## What it renders

| Resource | Purpose |
|----------|---------|
| `Deployment` (server) | the memkeys-server workload (non-root, read-only rootfs, dropped caps) |
| `Deployment` (worker) | async-projection workers — **microservice profile only** |
| `PodDisruptionBudget` | server availability during voluntary disruptions — **microservice profile only** |
| `Service` (ClusterIP) | in-cluster access on `service.port` (health, default 8080) **and** `memoryTransport.port` (the `/memory/*` transport, default 3000); selects the server component |
| `ConfigMap` | non-secret config: `MEMKEYS_DEPLOYMENT_ENV`, OTLP endpoint, service namespace, sampler ratio |
| `Secret` | `MEMKEYS_DATABASE_URL` (only when `database.url` is set and no `existingSecret`); `MEMKEYS_RUN_TOKEN_SECRET` (only when `memoryTransport.runTokenSecret.value` is set) |
| `ServiceAccount` | dedicated identity (token automount off by default) |
| `HTTPRoute` | optional Gateway API ingress (disabled by default — **not** legacy Ingress) |

## Database wiring

The server requires `MEMKEYS_DATABASE_URL` (same `postgres://…` shape as CI's
`TEST_DATABASE_URL`). Provide it one of two ways:

- `--set database.url=postgres://user:pass@host:5432/db` — the chart renders a Secret.
- `--set database.existingSecret=my-secret` (key `MEMKEYS_DATABASE_URL`, override with
  `database.existingSecretKey`) — reference a pre-provisioned Secret.

### ⚠ Postgres requirements — pgvector is required

The target Postgres **must have the [pgvector](https://github.com/pgvector/pgvector)
extension available**. A boot migration runs `CREATE EXTENSION vector`, so a stock
`postgres:*` image fails at startup. The server now preflights this and exits with a
clear message, but it still cannot start without pgvector.

- **Self-hosted Postgres:** use an image with pgvector preinstalled, e.g.
  `pgvector/pgvector:pg16` (also `pg17`/`pg18`), or install the `pgvector` package
  into your Postgres (`CREATE EXTENSION vector` must succeed).
- **Managed Postgres:** enable pgvector (AWS RDS/Aurora, Azure, Cloud SQL, Neon, and
  Supabase all ship it — enable the extension on the instance).

Supported Postgres major versions: 14–18 (matching pgvector's support window).

## Observability

Set `otel.endpoint` to an OTLP/gRPC collector to enable exporters for all
signals (traces, metrics, logs). Empty → no-op exporters (M4 default).

## ⚠ Current status — M1 bootstrap, no transport yet

`memkeys-server` at M1 connects to Postgres, applies migrations, and **exits**
— it does not yet expose an HTTP transport. Consequences:

- The default HTTP `livenessProbe`/`readinessProbe` (`/healthz`, `/readyz`) and the
  `Service` port target the **forthcoming** health surface. They are forward-looking.
- A long-running `Deployment` of the current image will not reach `Ready` (and will
  restart) until the server gains a long-running transport + health endpoints.

Until then, disable probes when deploying the bootstrap image:

```sh
helm install memkeys charts/memkeys \
  --set database.url=... \
  --set livenessProbe.enabled=false --set readinessProbe.enabled=false
```

The `ct install` chart test (helm.yml) is gated on the P0 Dockerfile
(ISI-6097); full pod-readiness verification additionally depends on the server
transport milestone.

## Linting & validation

```sh
helm lint charts/memkeys
helm template memkeys charts/memkeys | kubeconform -strict -summary \
  -schema-location default \
  -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
```

(The CRD schema location covers Gateway API `HTTPRoute` when enabled.)
