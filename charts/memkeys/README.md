# MemKeys Helm chart

Deploys [memkeys-server](../../crates/memkeys-server) — the attribute-based
memory server — to Kubernetes.

## TL;DR

```sh
helm install memkeys charts/memkeys \
  --set database.url="postgres://user:pass@host:5432/memkeys"
```

## What it renders

| Resource | Purpose |
|----------|---------|
| `Deployment` | the memkeys-server workload (non-root, read-only rootfs, dropped caps) |
| `Service` (ClusterIP) | in-cluster access on `service.port` (default 8080) |
| `ConfigMap` | non-secret config: `MEMKEYS_DEPLOYMENT_ENV`, OTLP endpoint, service namespace, sampler ratio |
| `Secret` | `MEMKEYS_DATABASE_URL` (only when `database.url` is set and no `existingSecret`) |
| `ServiceAccount` | dedicated identity (token automount off by default) |
| `HTTPRoute` | optional Gateway API ingress (disabled by default — **not** legacy Ingress) |

## Database wiring

The server requires `MEMKEYS_DATABASE_URL` (same `postgres://…` shape as CI's
`TEST_DATABASE_URL`). Provide it one of two ways:

- `--set database.url=postgres://user:pass@host:5432/db` — the chart renders a Secret.
- `--set database.existingSecret=my-secret` (key `MEMKEYS_DATABASE_URL`, override with
  `database.existingSecretKey`) — reference a pre-provisioned Secret.

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
