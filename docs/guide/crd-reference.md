---
title: CRD reference — MemKeysInstance
description: Every spec field of the MemKeysInstance custom resource (memkeys.io/v1alpha1), its status conditions, and when to pick the operator over the Helm chart.
sidebar:
  order: 5
---

# CRD reference — `MemKeysInstance`

The MemKeys operator manages deployments declaratively through a
`MemKeysInstance` custom resource (`memkeys.io/v1alpha1`). It reconciles the same
workloads the [Helm chart](../../charts/memkeys/README.md) renders — a server
Deployment, a Service, an optional async-projection worker Deployment, and a
PodDisruptionBudget — keeping the operator and the chart coherent. This page
documents every spec field, the status conditions, and when to reach for the
operator.

For install steps and worked examples, see
[`docs/k8s-operator.md`](../k8s-operator.md); this page is the field-by-field
reference.

## Operator vs. Helm chart

Both render **identical** workloads. Pick based on how you want to operate them.

| | Helm chart (M10a) | Operator (M10b) |
|---|---|---|
| Model | One-shot templating | Continuous reconciliation |
| Day-2 | `helm upgrade` | Edit the CR → auto-reconcile |
| Status | — | `MemKeysInstance` conditions (`Ready` / `Progressing` / `Degraded`) |
| Fleet | One release per install | Many `MemKeysInstance` objects per cluster |

Choose the **operator** when you want GitOps-style desired-state management with
live status, or you run many instances per cluster. Choose the **chart** for a
simple, one-shot install you upgrade by hand. The operator requires Kubernetes
≥ 1.27.

> **pgvector is required.** The Postgres referenced by `database` must have the
> [pgvector](https://github.com/pgvector/pgvector) extension available — a boot
> migration runs `CREATE EXTENSION vector`. A stock `postgres:*` image fails at
> startup (the server preflights and exits with a clear message). Use a
> pgvector-enabled image (e.g. `pgvector/pgvector:pg16`/`pg17`/`pg18`) or a managed
> Postgres with pgvector enabled.

## A minimal instance

```yaml
apiVersion: memkeys.io/v1alpha1
kind: MemKeysInstance
metadata:
  name: memkeys-sample
spec:
  profile: single-node
  database:
    url: postgres://memkeys:memkeys@postgres:5432/memkeys
  memoryTransport:
    runTokenSecret:
      name: memkeys-run-token
```

Ready-to-apply samples live in
[`operator/config/samples/`](../../operator/config/samples/memkeys_v1alpha1_memkeysinstance.yaml).

## `spec` fields

`database` is the only required field. Everything else has a default.

### `profile`

- **Type:** enum `single-node` | `microservice` — **Default:** `single-node`

Selects which workloads the operator reconciles:

- `single-node` — one all-in-one server Deployment (writer + inline projection).
  No worker Deployment, no PDB.
- `microservice` — the server Deployment (2 replicas by default, for
  drain-tolerance) **plus** a separate async-projection worker Deployment and a
  PodDisruptionBudget.

Mirrors the Helm chart's `profile` value so the operator and chart stay coherent.

### `replicas`

- **Type:** integer (≥ 0) — **Default:** follows the profile (1 for `single-node`,
  2 for `microservice`)

Server replica count. The `microservice` default is 2 so a `kubectl drain`
tolerates losing one pod under the PDB.

### `image`

The `memkeys-server` container image. The same image runs both the server and the
worker role; the role is selected via env (`MEMKEYS_ROLE`), not a different image.

| Field | Type | Default | Notes |
|---|---|---|---|
| `image.repository` | string | `ghcr.io/memkeys/memkeys` | Image repository (without tag). |
| `image.tag` | string | the operator's pinned `appVersion` | Image tag; empty uses the pinned default. |
| `image.pullPolicy` | enum `Always`\|`IfNotPresent`\|`Never` | `IfNotPresent` | Container image pull policy. |

### `database` (required)

Wires the required `MEMKEYS_DATABASE_URL` into the workloads. Provide **exactly
one** of `url` or `existingSecret`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `database.url` | string | — | Inline Postgres connection string. The operator renders a `<name>-db` Secret from it. Convenient for dev. |
| `database.existingSecret` | string | — | Name of a pre-provisioned Secret holding the URL. **Prefer this in production** so the URL is never stored in the CR object. |
| `database.existingSecretKey` | string | `MEMKEYS_DATABASE_URL` | Key within `existingSecret` holding the URL. |

Supplying **neither or both** of `url` / `existingSecret` marks the instance
`Degraded` with no pods rolled out (fail-closed). Switching from an inline `url` to
an `existingSecret` prunes the previously rendered `<name>-db` Secret (unless
`existingSecret` names that same Secret, in which case it is treated as
user-managed and left alone).

### `otel`

OpenTelemetry export. An empty `endpoint` selects the no-op exporters (telemetry
off). Mirrors the chart's OTel env wiring.

| Field | Type | Default | Notes |
|---|---|---|---|
| `otel.endpoint` | string | — (no-op) | OTLP/gRPC collector endpoint. Empty → no-op exporters. |
| `otel.deploymentEnv` | string | `dev` | Drives sampling + resource attributes (`dev` \| `staging` \| `prod`). |
| `otel.serviceNamespace` | string | — | Sets the `service.namespace` resource attribute. |
| `otel.tracesSamplerRatio` | string | — | Optionally overrides the head-sampling ratio (for `prod`). |

### `workers`

Configures the async-projection worker Deployment. Relevant to the `microservice`
profile (and anywhere you force workers on). Workers run the same image in the
**worker role** (`MEMKEYS_ROLE=worker`): projection-only — health surface +
projection reconciler, **no** memory transport.

| Field | Type | Default | Notes |
|---|---|---|---|
| `workers.enabled` | bool (nullable) | follows profile (on for `microservice`, off for `single-node`) | Force the worker Deployment on/off regardless of profile. |
| `workers.replicas` | integer (≥ 0) | `1` | Worker replica count. |
| `workers.command` | string[] | — | Override the container entrypoint. |
| `workers.args` | string[] | — | Args appended to the worker container command. |
| `workers.resources` | ResourceRequirements | — | Worker container requests/limits. |

### `podDisruptionBudget`

Configures the server PDB (`microservice` profile).

| Field | Type | Default | Notes |
|---|---|---|---|
| `podDisruptionBudget.enabled` | bool (nullable) | follows profile | Force the PDB on/off. A PDB is only rendered when effective server `replicas > 1` — a `minAvailable: 1` PDB on a single replica would block `kubectl drain`. |
| `podDisruptionBudget.minAvailable` | integer (≥ 0) | `1` | PDB `minAvailable`. Clamped to `replicas - 1` as a drain-deadlock guard. |

### `servicePort`

- **Type:** integer (1–65535) — **Default:** `8080`

The port the Service exposes and the container listens on for the **health** surface
(`/healthz`, `/readyz`, env `MEMKEYS_HTTP_PORT`). The `/memory/*` transport has its
own listener — see `memoryTransport`. Must differ from `memoryTransport.port`.

### `memoryTransport`

Configures exposure of the `/memory/*` HTTP transport (the product API). The server
binds it on its **own** listener (`MEMKEYS_MEMORY_HTTP_PORT`, default 3000), distinct
from the health surface on `servicePort`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `memoryTransport.enabled` | bool (nullable) | `true` | Expose the transport (own container/Service `memory` port + `MEMKEYS_HTTP_HOST=0.0.0.0`). `false` renders the health-only surface. |
| `memoryTransport.port` | integer (1–65535) | `3000` | `MEMKEYS_MEMORY_HTTP_PORT` + the Service `memory` port. Must differ from `servicePort`. |
| `memoryTransport.runTokenSecret.name` | string | — | Name of a pre-provisioned Secret holding the run-token **issuer** secret. Required unless `allowInsecure`. |
| `memoryTransport.runTokenSecret.key` | string | `MEMKEYS_RUN_TOKEN_SECRET` | Key within that Secret. |
| `memoryTransport.allowInsecure` | bool | `false` | **Dev-only:** bind off-loopback with **no** run-token verifier (tenant self-asserted). Prefer `runTokenSecret`. |

Exposing the transport off loopback requires a run-token verifier — the server
refuses a non-loopback bind without one. The operator mirrors that **fail-closed**:
an enabled transport with **neither** `runTokenSecret` **nor** `allowInsecure: true`
is marked `Degraded` (reason `MemoryTransportUnauthenticated`) and nothing is rolled
out. A `memoryTransport.port` equal to `servicePort` degrades with
`MemoryTransportPortCollision`.

### `resources`

- **Type:** ResourceRequirements — **Default:** chart defaults

Server container resource requests/limits.

### `imagePullSecrets`

- **Type:** `[]LocalObjectReference` — **Default:** —

Image pull secrets referenced by **both** the server and worker pods.

## Status

`status.conditions` follow the standard `metav1.Condition` semantics and reflect
**real** Deployment availability — never fabricated:

| Condition | True when |
|---|---|
| `Ready` | The server (and, for `microservice`, the workers) are fully available. |
| `Progressing` | The operator is still rolling out toward desired state (replicas not yet all available). |
| `Degraded` | A managed Deployment reports a failure (`ReplicaFailure` / `ProgressDeadlineExceeded`), or the DB / transport config is unusable (fail-closed). |

Alongside the conditions, the status reports observed counts:

| Field | Meaning |
|---|---|
| `observedGeneration` | The `.metadata.generation` the status reflects. |
| `serverReplicas` / `readyServerReplicas` | Desired vs. available server replicas. |
| `workerReplicas` / `readyWorkerReplicas` | Desired vs. available worker replicas (0 when disabled). |

Inspect an instance:

```sh
kubectl get memkeysinstance          # short name: mki — shows Profile / Ready / Server
kubectl describe mki memkeys-sample
```

The resource registers the short name `mki` and the category `memkeys`, and prints
`Profile`, `Ready`, `Server`, and `Age` columns.
