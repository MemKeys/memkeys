# Reproducible bench / dogfood deploy (ISI-6183 Set A)

Run notes for standing up a MemKeys server that the ISI-6183 Set A benchmark
and the dogfood OTel consumers can point at **from official published
artifacts** — no local kaniko/Docker build. This closes the reproducibility gap
from ISI-6678 (follow-up to ISI-6671).

## Released artifacts

| artifact | reference | carries |
|----------|-----------|---------|
| server image | `ghcr.io/memkeys/memkeys-server:0.1.0` | memory-semconv v0.1.0 dual-emit (ISI-6216) **and** the OpenAI-compatible external embedder + configurable fact-vector dim (ISI-6671) |
| Helm chart | `oci://ghcr.io/memkeys/charts/memkeys` version `0.5.0` | `image.repository` defaults to the published server image; embedder env exposed as values (ISI-6678) |

The image tag tracks the chart `appVersion` (`0.1.0`), so a chart install with
an empty `image.tag` pulls `ghcr.io/memkeys/memkeys-server:0.1.0`. The `v0.1.0`
git tag is what publishes both (`ghcr-publish.yml` → image, `helm-publish.yml`
→ chart, `release.yml` → GitHub Release + SBOMs).

## Deploy (external embedder wired, no manual env patch)

```sh
helm install memkeys oci://ghcr.io/memkeys/charts/memkeys --version 0.5.0 \
  --set database.url="postgres://…" \
  --set memoryTransport.runTokenSecret.existingSecret=memkeys-run-token \
  --set otel.endpoint="http://otel-collector:4317" \
  --set semantic.enabled=true \
  --set semantic.embedder.url="http://tei.embed.svc:80/embeddings" \
  --set semantic.embedder.model="bge-small-en-v1.5" \
  --set semantic.embedder.dim=384
```

- `otel.endpoint` turns on OTLP export; the server dual-emits native OTel **and**
  the `memory.*` memory-semconv signals (ISI-6216) — the bench OTel consumers
  read the latter.
- `semantic.*` wires the external embedder. Omit `embedder.url` to fall back to
  the built-in deterministic `HashEmbedder` (dim 256). `embedder.dim` **must**
  equal the width the endpoint returns, or the server refuses to embed.

No `--set image.repository` override is needed any more — the chart default was
corrected to the published image name in 0.5.0 (ISI-6678). Earlier deploys
(pre-0.5.0) carried a `ghcr.io/memkeys/memkeys` default that never matched the
published `…/memkeys-server` image and required a manual override.

## Cross-refs

- ISI-6183 — benchmark Set A (consumer of these artifacts).
- ISI-6216 / ISI-6222 — memory-semconv dual-emit + image.
- ISI-6671 — external embedder + configurable dim.
- ISI-6304 / ISI-6305 / ISI-6306 — GHCR publish, Helm OCI publish, release automation.
- ISI-6678 — this reproducibility pass (release tag + chart values + these notes).
