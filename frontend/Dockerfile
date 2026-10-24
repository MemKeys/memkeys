# syntax=docker/dockerfile:1

# Multi-stage build for the MemKeys Inspector UI (ISI-6406, parent ISI-6395 B1):
# node builds the Vite SPA -> nginx:alpine serves dist/ and reverse-proxies
# /memory/ to the memory API, so the SPA works same-origin
# (frontend/src/services/api.ts hardcodes API_BASE_URL='/memory', relative).
#
# Build (context = the frontend/ directory):
#   docker build -f frontend/Dockerfile -t memkeys-inspector:dev ./frontend
# Run (upstream overridable at deploy time, ISI-6395 B2):
#   docker run -p 8080:80 -e MEMORY_API_UPSTREAM=memkeys:3000 memkeys-inspector:dev
#
# PR CI builds this image with buildx (load only, no push) via the
# docker-inspector job in .github/workflows/docker.yml. The GHCR push to
# ghcr.io/memkeys/memkeys-inspector runs on main/tags via ghcr-publish.yml.

# --- builder -------------------------------------------------------------
FROM node:22-alpine AS builder
WORKDIR /build

COPY package.json package-lock.json ./
# --include=dev: `npm run build` runs `tsc`, a devDependency. Do NOT set
# NODE_ENV=production for this step — npm ci would then skip devDependencies.
RUN npm ci --include=dev

COPY tsconfig.json tsconfig.node.json vite.config.ts index.html ./
COPY src ./src
RUN npm run build

# --- runtime -------------------------------------------------------------
FROM nginx:alpine AS runtime

# Patch base-image OS packages so the grype scan's `--fail-on critical` gate
# (docker.yml) stays green when a new Alpine CVE lands before the nginx:alpine
# tag is rebuilt — e.g. CVE-2026-111497 in brotli-libs (<1.2.1-r0). The base
# is already a floating tag, so this does not change the image's determinism.
RUN apk upgrade --no-cache

# SPA assets + the server config. nginx.conf is an envsubst template: the
# entrypoint renders it to /etc/nginx/conf.d/default.conf at container start,
# substituting MEMORY_API_UPSTREAM.
COPY --from=builder /build/dist /usr/share/nginx/html
COPY nginx.conf /etc/nginx/nginx.conf.template
COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod 0755 /docker-entrypoint.sh

# In-cluster default: the memkeys Service's /memory/* transport listener
# (charts/memkeys memoryTransport.port, default 3000). ISI-6395 B2 overrides
# this env var in the inspector Deployment if the Service lives in another
# namespace (e.g. memkeys.memkeys.svc.cluster.local:3000).
ENV MEMORY_API_UPSTREAM=memkeys:3000

EXPOSE 80
# ENTRYPOINT ["/docker-entrypoint.sh"] and CMD ["nginx", "-g", "daemon off;"]
# are inherited from nginx:alpine; our entrypoint renders the config first.
