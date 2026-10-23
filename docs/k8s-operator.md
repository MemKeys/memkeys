# Kubernetes: Helm chart and operator

MemKeys runs on Kubernetes two ways: a **Helm chart** for one-shot templated
installs and a **Kubernetes operator** managing a `MemKeysInstance` custom
resource for continuous reconciliation. Both render identical workloads — a
server Deployment, a Service, an optional async-projection worker Deployment,
and a PodDisruptionBudget — and both expose the same `profile` knob
(`single-node` / `microservice`).

For a container or VM without Kubernetes, see the
[Quickstart — Docker](/memkeys/quickstart/#path-b-docker).

| | Helm chart | Operator |
|---|---|---|
| Model | One-shot templating | Continuous reconciliation |
| Day-2 | `helm upgrade` | CRD edit → auto-reconcile |
| Status | — | `MemKeysInstance` conditions (`Ready` / `Progressing` / `Degraded`) |
| Fleet | One release per install | Many `MemKeysInstance` objects per cluster |

Pick the operator for GitOps-style desired-state management and live status,
or when you run many instances per cluster.

## Prerequisites

- Kubernetes ≥ 1.27 (operator), Helm 3 (chart)
- **Postgres with [pgvector](https://github.com/pgvector/pgvector).** A boot
  migration runs `CREATE EXTENSION vector`; stock `postgres:*` images fail at
  startup with a clear message. Use `pgvector/pgvector:pg16` (also pg17/pg18)
  or a managed Postgres with pgvector enabled.

## Helm: install

The chart is published as a public OCI artifact — no `helm repo add` needed:

```sh
# Create the run-token issuer secret
kubectl create secret generic memkeys-run-token \
  --from-literal=MEMKEYS_RUN_TOKEN_SECRET=<issuer-secret>

# Install from GHCR
helm install memkeys oci://ghcr.io/memkeys/charts/memkeys \
  --set database.url="postgres://user:pass@host:5432/memkeys" \
  --set memoryTransport.runTokenSecret.existingSecret=memkeys-run-token
```

Or from a checkout: `helm install memkeys charts/memkeys …`. Inspect a
version with `helm show chart oci://ghcr.io/memkeys/charts/memkeys`.

### Profiles

| `profile` | Renders | Use for |
|-----------|---------|---------|
| `single-node` (default) | one all-in-one Deployment (server = writer + inline projection) | single node, laptops, dev, small installs |
| `microservice` | server Deployment **+** separate async-projection worker Deployment **+** PodDisruptionBudget | scaled clusters where projection rebuild is isolated from the request path |

```sh
helm install memkeys oci://ghcr.io/memkeys/charts/memkeys \
  --set profile=microservice \
  --set database.url="postgres://user:pass@host:5432/memkeys"
```

Workers run the same image in the **worker role** (`MEMKEYS_ROLE=worker`):
projection-only — health surface plus the reconciler, no memory transport.
Tune with `workers.replicaCount`, `workers.resources`, and the usual
scheduling keys. Server replicas default to 1 (single-node) or 2
(microservice); the PodDisruptionBudget renders only when replicas > 1, so a
single-replica install can always drain.

### Two listeners, one Service

The Service fronts **both** listeners: the health surface
(`/healthz`/`/readyz`, `service.port`, probes only) and the `/memory/*`
transport (`memoryTransport.port`, default 3000 — the port clients target).
The transport is exposed by default (`0.0.0.0` bind); the gate is fail-closed:
an enabled transport with neither a `runTokenSecret` nor `allowInsecure`
degrades with `MemoryTransportUnauthenticated` and rolls out nothing, because
the server itself would refuse the non-loopback bind. Set
`memoryTransport.enabled=false` for a health-only deploy.

Probe readiness on `service.port`; point clients at `memoryTransport.port`.
8080 answers `/healthz` and 404s `/memory/*` — that is correct behavior, not
a misroute.

### Inspector

Set `frontend.enabled=true` to deploy the read-only
[Inspector UI](https://github.com/MemKeys/memkeys/tree/main/frontend)
(`ghcr.io/memkeys/memkeys-inspector`) as an nginx-served SPA wired to the
in-cluster memory transport.

## Operator: install

```sh
# 1. Install the CRD.
kubectl apply -f operator/config/crd/bases/memkeys.io_memkeysinstances.yaml

# 2. Install the operator (Namespace, RBAC, controller manager).
kubectl apply -f operator/deploy/operator.yaml

# 3. Confirm the manager is running.
kubectl -n memkeys-system rollout status deploy/memkeys-operator
```

The operator image is `ghcr.io/memkeys/memkeys-operator`; override `image:`
in `config/manager/manager.yaml` for your registry.

### Create an instance

`single-node` (default) — inline Postgres URL for dev, existing Secret for
production:

```yaml
apiVersion: memkeys.io/v1alpha1
kind: MemKeysInstance
metadata:
  name: memkeys-sample
spec:
  profile: single-node
  image:
    repository: ghcr.io/memkeys/memkeys-server
    tag: "0.1.0"
  database:
    url: postgres://memkeys:memkeys@postgres:5432/memkeys
  memoryTransport:
    runTokenSecret:
      name: memkeys-run-token
  otel:
    deploymentEnv: dev
```

`microservice` — server (2 replicas by default) + workers + a drain-safe
PodDisruptionBudget:

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

```sh
kubectl get memkeysinstance     # short name: mki — Profile / Ready / Server
kubectl describe mki memkeys-sample
```

### Spec reference (`v1alpha1`)

| Field | Default | Notes |
|---|---|---|
| `profile` | `single-node` | `single-node` \| `microservice` |
| `replicas` | profile (1 / 2) | server replica count override |
| `image.repository` / `image.tag` / `image.pullPolicy` | `ghcr.io/memkeys/memkeys` / app version / `IfNotPresent` | server + worker image; published release images live at `ghcr.io/memkeys/memkeys-server` — set it explicitly as in the samples |
| `database.url` | — | inline URL → operator renders a `<name>-db` Secret |
| `database.existingSecret` / `existingSecretKey` | — / `MEMKEYS_DATABASE_URL` | reference a pre-provisioned Secret |
| `otel.endpoint` / `deploymentEnv` / `serviceNamespace` / `tracesSamplerRatio` | — / `dev` / — / — | OTel wiring, mirrors the chart |
| `workers.enabled` | follows profile | force the worker Deployment on/off |
| `workers.replicas` / `command` / `args` / `resources` | `1` | worker tuning; the role itself is `MEMKEYS_ROLE=worker` |
| `podDisruptionBudget.enabled` / `minAvailable` | follows profile / `1` | rendered only when server replicas > 1; `minAvailable` clamped to `replicas-1` (drain-deadlock guard) |
| `servicePort` | `8080` | Service + container port + health listener |
| `memoryTransport.enabled` | `true` | expose `/memory/*`: own container/Service port + `MEMKEYS_HTTP_HOST=0.0.0.0`; `false` renders the health-only surface |
| `memoryTransport.port` | `3000` | must differ from `servicePort`; degrades with `MemoryTransportPortCollision` otherwise |
| `memoryTransport.runTokenSecret.name` / `.key` | — / `MEMKEYS_RUN_TOKEN_SECRET` | Secret holding the run-token issuer secret; required unless `allowInsecure` |
| `memoryTransport.allowInsecure` | `false` | dev only: bind off-loopback with no verifier |
| `resources` | chart defaults | server container requests/limits |
| `imagePullSecrets` | — | list of `LocalObjectReference` names; secrets with registry credentials for pulling the server/worker image from private registries |

Exactly one of `database.url` / `database.existingSecret` must be set, else
the instance is marked `Degraded` and no pods roll out (fail-closed).

### Status conditions

Conditions follow standard `metav1.Condition` semantics and reflect **real**
Deployment availability: `Ready` (server — and workers, for microservice —
fully available), `Progressing` (rolling out), `Degraded`
(`ReplicaFailure`, `ProgressDeadlineExceeded`, or unusable DB config).

## Development

```sh
cd operator
make manifests generate   # regenerate CRD/RBAC + DeepCopy (controller-gen)
make vet build            # static checks + binary
make test                 # envtest e2e smoke
```

CI runs build + vet + the envtest smoke on every change under `operator/`
and fails if generated manifests are stale.
