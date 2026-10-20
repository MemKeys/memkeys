# MemKeys Kubernetes operator (M10b)

The MemKeys operator manages MemKeys deployments declaratively through a
`MemKeysInstance` custom resource. It reconciles the same workloads the
[M10a Helm chart](../charts/memkeys/README.md) renders — a server Deployment, a
Service, an optional async-projection worker Deployment, and a
PodDisruptionBudget — keeping the operator and the chart **coherent**: the CRD
expresses the same `profile` knob (`single-node` / `microservice`) and the
builders mirror `charts/memkeys/templates/*`.

Source lives in [`operator/`](../operator) as a standalone Go module
(kubebuilder / controller-runtime v0.19, `memkeys.io/v1alpha1`). It is kept out
of the Rust server tree.

## When to use the operator vs. the Helm chart

| | Helm chart (M10a) | Operator (M10b) |
|---|---|---|
| Model | One-shot templating | Continuous reconciliation |
| Day-2 | `helm upgrade` | CRD edit → auto-reconcile |
| Status | — | `MemKeysInstance` conditions (`Ready`/`Progressing`/`Degraded`) |
| Fleet | One release per install | Many `MemKeysInstance` objects per cluster |

Both render identical workloads; pick the operator when you want GitOps-style
desired-state management and live status, or run many instances per cluster.

## Install

Requires Kubernetes ≥ 1.27.

```sh
# 1. Install the CRD.
kubectl apply -f operator/config/crd/bases/memkeys.io_memkeysinstances.yaml

# 2. Install the operator (Namespace, RBAC, controller manager) in one shot.
kubectl apply -f operator/deploy/operator.yaml

# 3. Confirm the manager is running.
kubectl -n memkeys-system rollout status deploy/memkeys-operator
```

The split sources under `operator/config/` (`crd/`, `rbac/`, `manager/`) are the
canonical per-component manifests (`make manifests` regenerates `rbac/role.yaml`
and the CRD); `deploy/operator.yaml` is the generated all-in-one convenience
bundle of the same objects.

The container image is built as `ghcr.io/memkeys/memkeys-operator` (see
`operator/Dockerfile`); override the `image:` in `config/manager/manager.yaml`
for your registry/tag.

## Create an instance

> **⚠ Postgres must have pgvector.** The Postgres referenced by `database.url` /
> `database.existingSecret` must have the [pgvector](https://github.com/pgvector/pgvector)
> extension available — a boot migration runs `CREATE EXTENSION vector` (ISI-6319).
> A stock `postgres:*` image fails at startup (the server preflights this and exits
> with a clear message). Use a pgvector-enabled image (e.g. `pgvector/pgvector:pg16`,
> also `pg17`/`pg18`) or a managed Postgres with pgvector enabled.

### `single-node` (default)

One all-in-one server Deployment + Service. Supply the Postgres URL inline for
dev, or reference an existing Secret for production.

```yaml
apiVersion: memkeys.io/v1alpha1
kind: MemKeysInstance
metadata:
  name: memkeys-sample
spec:
  profile: single-node
  image:
    repository: ghcr.io/memkeys/memkeys
    tag: "0.1.0"
  database:
    url: postgres://memkeys:memkeys@postgres:5432/memkeys
  memoryTransport:
    runTokenSecret:
      name: memkeys-run-token
  otel:
    deploymentEnv: dev
```

### `microservice`

Server (2 replicas by default, for drain-tolerance) + a separate worker
Deployment + a drain-safe PodDisruptionBudget. Prefer `existingSecret` in prod.
Workers run the same image in the **worker role** (`MEMKEYS_ROLE=worker`):
projection-only — health surface + projection reconciler, no memory transport.

```yaml
apiVersion: memkeys.io/v1alpha1
kind: MemKeysInstance
metadata:
  name: memkeys-prod
spec:
  profile: microservice
  database:
    existingSecret: memkeys-db
    existingSecretKey: MEMKEYS_DATABASE_URL
  memoryTransport:
    runTokenSecret:
      name: memkeys-run-token
  otel:
    endpoint: otel-collector.observability:4317
    deploymentEnv: prod
  workers:
    replicas: 2
```

Ready-to-apply samples: [`operator/config/samples/`](../operator/config/samples/memkeys_v1alpha1_memkeysinstance.yaml).

## Spec reference (`v1alpha1`)

| Field | Default | Notes |
|---|---|---|
| `profile` | `single-node` | `single-node` \| `microservice` |
| `replicas` | profile (1 / 2) | server replica count override |
| `image.repository` / `image.tag` / `image.pullPolicy` | `ghcr.io/memkeys/memkeys` / `latest` / `IfNotPresent` | server + worker image |
| `database.url` | — | inline URL → operator renders `<name>-db` Secret |
| `database.existingSecret` / `existingSecretKey` | — / `MEMKEYS_DATABASE_URL` | reference a pre-provisioned Secret |
| `otel.endpoint` / `deploymentEnv` / `serviceNamespace` / `tracesSamplerRatio` | — / `dev` / — / — | mirrors the chart's OTel env wiring |
| `workers.enabled` | follows profile | force worker Deployment on/off |
| `workers.replicas` / `command` / `args` / `resources` | `1` | worker tuning; the worker role itself is selected by the `MEMKEYS_ROLE=worker` env (projection-only: no memory transport) |
| `podDisruptionBudget.enabled` / `minAvailable` | follows profile / `1` | only rendered when server replicas > 1; `minAvailable` is clamped to `replicas-1` (drain-deadlock guard) |
| `servicePort` | `8080` | Service + container port + `MEMKEYS_HTTP_PORT` (health listener) |
| `memoryTransport.enabled` | `true` | expose the `/memory/*` transport: own container/Service `memory` port + `MEMKEYS_HTTP_HOST=0.0.0.0`; `false` renders the health-only surface |
| `memoryTransport.port` | `3000` | `MEMKEYS_MEMORY_HTTP_PORT` + Service `memory` port; must differ from `servicePort` |
| `memoryTransport.runTokenSecret.name` / `.key` | — / `MEMKEYS_RUN_TOKEN_SECRET` | pre-provisioned Secret holding the run-token issuer secret; required unless `allowInsecure` |
| `memoryTransport.allowInsecure` | `false` | dev-only: bind off-loopback with NO verifier (tenant self-asserted — ISI-6137 S1) |
| `resources` | chart defaults | server container requests/limits |

Exactly one of `database.url` / `database.existingSecret` must be set, else the
instance is marked `Degraded` and no pods are rolled out (fail-closed). Switching
from an inline `url` to an `existingSecret` prunes the previously rendered
`<name>-db` Secret (unless `existingSecret` names that same Secret, in which
case it is user-managed and left alone).

### Memory transport exposure (ISI-6233)

The server runs two distinct listeners: the health surface (`servicePort`, probes
only) and the `/memory/*` transport (`memoryTransport.port`, default 3000). The
operator exposes the transport by default — a second container/Service `memory`
port plus `MEMKEYS_HTTP_HOST=0.0.0.0` — because a health-only deploy leaves the
product API unreachable off-pod. The gate is fail-closed, mirroring the server's
own bind refusal: an enabled transport with **neither** `runTokenSecret` (a
pre-provisioned Secret holding the run-token issuer secret) **nor**
`allowInsecure: true` is marked `Degraded` with reason
`MemoryTransportUnauthenticated` and nothing is rolled out — the server would
refuse the non-loopback bind and crash-loop. A `memoryTransport.port` equal to
`servicePort` degrades with `MemoryTransportPortCollision`. Set
`memoryTransport.enabled=false` to render the health-only surface instead.

## Status conditions

`status.conditions` follow the standard `metav1.Condition` semantics and reflect
**real** Deployment availability (never fabricated):

- `Ready` — `True` when the server (and, for microservice, workers) are fully
  available.
- `Progressing` — `True` while rolling out toward desired state.
- `Degraded` — `True` on a managed-Deployment `ReplicaFailure` /
  `ProgressDeadlineExceeded`, or on an unusable DB config.

```sh
kubectl get memkeysinstance        # short name: mki — shows Profile / Ready / Server
kubectl describe mki memkeys-sample
```

## Development

```sh
cd operator
make manifests generate   # regenerate CRD/RBAC + DeepCopy (controller-gen)
make vet build            # static checks + binary
make test                 # envtest e2e smoke (downloads a pinned apiserver/etcd)
```

CI runs build + vet + the envtest e2e smoke on every change under `operator/`
(`.github/workflows/operator.yml`), and fails if the generated manifests or
DeepCopy are stale.
