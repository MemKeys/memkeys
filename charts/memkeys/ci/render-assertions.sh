#!/usr/bin/env bash
# Chart render assertions for the memory transport exposure (ISI-6233).
#
# helm template + grep assertions that the /memory/* transport is actually
# routable in a rendered release: container port, Service port, HTTPRoute
# backendRef, and the env the server needs to bind off loopback — plus the
# fail-fast cases. Run from the repo root:
#   charts/memkeys/ci/render-assertions.sh
# Wired into .github/workflows/helm.yml (kubeconform job).
set -euo pipefail

CHART=charts/memkeys
DB="--set database.url=postgres://u:p@h:5432/db"

fail() { echo "FAIL: $*" >&2; exit 1; }
assert_contains() { grep -qF -- "$2" <<<"$1" || fail "rendered output does not contain '$2'"; }

render() { helm template memkeys "$CHART" "$@" 2>/dev/null; }

# --- exposed surface (run-token secret via existingSecret) -------------------
out=$(render $DB --set httpRoute.enabled=true \
  --set 'httpRoute.parentRefs[0].name=gw' \
  --set memoryTransport.runTokenSecret.existingSecret=my-token)

svc=$(sed -n '/^kind: Service$/,/^---/p' <<<"$out")
assert_contains "$svc" "name: memory"
assert_contains "$svc" "- port: 3000"
assert_contains "$svc" "targetPort: memory"

dep=$(sed -n '/^kind: Deployment$/,/^---/p' <<<"$out")
assert_contains "$dep" "containerPort: 3000"
assert_contains "$dep" "MEMKEYS_MEMORY_HTTP_PORT"
assert_contains "$dep" 'value: "3000"'
assert_contains "$dep" "MEMKEYS_HTTP_HOST"
assert_contains "$dep" 'value: "0.0.0.0"'
assert_contains "$dep" "MEMKEYS_RUN_TOKEN_SECRET"
assert_contains "$dep" "name: my-token"
[[ $(grep -c "MEMKEYS_ALLOW_INSECURE" <<<"$dep") -eq 0 ]] || fail "allowInsecure env leaked with a run-token secret set"

route=$(sed -n '/^kind: HTTPRoute$/,$p' <<<"$out")
assert_contains "$route" "port: 3000"
[[ $(grep -cE "port: 8080" <<<"$route") -eq 0 ]] || fail "HTTPRoute still targets the health port"

# --- health surface stays probe-only but routable ----------------------------
assert_contains "$svc" "- port: 8080"
assert_contains "$dep" "containerPort: 8080"

# --- inline run-token secret renders <fullname>-run-token --------------------
out=$(render $DB --set memoryTransport.runTokenSecret.value=inline-issuer)
sec=$(sed -n '/^kind: Secret$/,/^---/p' <<<"$out")
assert_contains "$sec" "name: memkeys-run-token"
assert_contains "$sec" "MEMKEYS_RUN_TOKEN_SECRET: \"inline-issuer\""

# --- dev-only insecure opt-in ------------------------------------------------
out=$(render $DB --set memoryTransport.allowInsecure=true)
dep=$(sed -n '/^kind: Deployment$/,/^---/p' <<<"$out")
assert_contains "$dep" "MEMKEYS_ALLOW_INSECURE"
assert_contains "$dep" 'value: "1"'
[[ $(grep -c "MEMKEYS_RUN_TOKEN_SECRET" <<<"$dep") -eq 0 ]] || fail "run-token env leaked in insecure mode"

# --- disabled: health-only surface (M10a behaviour) ---------------------------
out=$(render $DB --set memoryTransport.enabled=false)
[[ $(grep -c "name: memory" <<<"$out") -eq 0 ]] || fail "memory port rendered while disabled"
[[ $(grep -c "MEMKEYS_HTTP_HOST" <<<"$out") -eq 0 ]] || fail "bind host env rendered while disabled"
svc=$(sed -n '/^kind: Service$/,/^---/p' <<<"$out")
assert_contains "$svc" "- port: 8080"

# --- fail-fast: enabled without credentials -----------------------------------
if helm template memkeys "$CHART" $DB >/dev/null 2>&1; then
  fail "render must fail when memoryTransport is enabled without credentials"
fi
err=$(helm template memkeys "$CHART" $DB 2>&1) || true
assert_contains "$err" "memoryTransport.enabled=true exposes the /memory/* transport"

# --- fail-fast: port collision with the health port ---------------------------
if helm template memkeys "$CHART" $DB --set memoryTransport.runTokenSecret.value=v \
  --set memoryTransport.port=8080 >/dev/null 2>&1; then
  fail "render must fail when memoryTransport.port == service.port"
fi

# --- fail-fast: portName collision --------------------------------------------
if helm template memkeys "$CHART" $DB --set memoryTransport.runTokenSecret.value=v \
  --set memoryTransport.portName=http >/dev/null 2>&1; then
  fail "render must fail when memoryTransport.portName == service.portName"
fi

echo "memory transport render assertions: all passed"
