# Agent MCP transport + per-run token policy (ISI-6337)

Status: **ACCEPTED** (Architect, 2026-10-20) · Parent: ISI-6328 Ph2 cutover ·
Owner split: Architect owns this recipe (MemKeys config); ProxOps owns the
agent/cluster wiring and the canary→fleet rollout.

This is the blessed, fleet-safe recipe for pointing Claude Code agents at
MemKeys over MCP, replacing the `mempalace` MCP server in `~/.claude.json`.

---

## 1. Decision — transport shape

ProxOps proved a working remote MCP stdio path (ISI-6333): `kubectl exec` into
the primary MemKeys pod and spawn a **second full `memkeys-server`** (workers +
HTTP on alt-ports) per MCP session. That is fine for a one-agent canary but
fragile at fleet scale, for two reasons:

1. the second server binds the health (`:8080`) and memory-HTTP (`:3000`)
   listeners, so every concurrent session needs **unique alt-ports** or it dies
   with "address already in use";
2. every session **double-runs** the projection/LLM worker + maintenance stack
   inside the primary pod — wasted CPU and redundant reconcile scans.

Three options were on the table:

| Option | What | Verdict |
|---|---|---|
| (a) `--mcp-only` launch mode (repo change) | Server runs ONLY stdio MCP: no binds, no workers | **CHOSEN** |
| (b) stdio↔HTTP bridge | New shim translating MCP stdio → the proven `/memory/*` bearer path | Rejected: a brand-new component + protocol translation layer; more code, second codepath to keep in sync |
| (c) accept the exec recipe + randomized ports + resource caps | No code; keep spawning a full second server | Interim only (see §6) — still binds ports and double-runs workers |

**Chosen: (a).** It is the smallest change that removes *both* failure modes at
the source. `MEMKEYS_ROLE=mcp` is the symmetric sibling of the existing
`MEMKEYS_ROLE=worker` gate: the process connects to the shared Postgres, binds
the single stdio principal from the run token, and runs `mcp::run_stdio` inline
until stdin closes — **nothing else**:

- **no health or `/memory/*` HTTP bind** → concurrent sessions can never collide
  on a port (no alt-port juggling);
- **no maintenance/worker loop and no boot reconcile** → no double worker stack;
  the primary all-in-one pod remains the sole owner of projection + maintenance;
- **stdout carries only JSON-RPC** — all the all-in-one startup `println!`s are
  gated off, and tracing/OTel logs never target stdout, so the protocol stream
  stays clean (a latent hazard in the exec-a-full-server recipe).

Option (a) *dominates* option (c): (c) keeps binding two listeners the MCP
session never uses and keeps double-running the worker stack; randomized ports
only paper over the collision (a) deletes outright.

### Read-path parity (do not skip)

The MCP `memory_search` op runs the semantic read path, so the MCP-only process
**must have the semantic module wired** (`MEMKEYS_SEMANTIC=1`) and **must use the
exact same embedder** as the primary pod — otherwise query vectors land in a
different ANN space than the stored vectors (the M7 model-stamp lesson). Because
the MCP-only process is `exec`'d *inside* the primary pod it inherits the pod's
`MEMKEYS_EMBED_URL` / embedder env automatically; keep it that way.

---

## 2. Per-run token policy (F12)

- **Issuer secret:** HS256, from `MEMKEYS_RUN_TOKEN_SECRET` (k8s secret
  `memkeys-run-token`). The server's `JwtVerifier` validates the launch token
  once and **re-checks the expiry window on every tool call** (F12), so a token
  that expires mid-session hard-fails `unauthenticated` rather than lingering.
- **Claims:** `company_id` = tenant floor (`Principal.tenant_id`), `sub` = agent
  id (`Principal.agent_id`), plus `exp`. `run_id` / `trace_id` optional. The
  caller **cannot** assert a tenant — it is derived server-side from the verified
  principal (ISI-6148 / ISI-6137 S1). Agents' `entity.agent` is stamped from
  `sub`.
- **No static long-lived token in `~/.claude.json`.** The MCP `command` **mints a
  short-lived token at session launch** from the in-pod secret and injects it as
  `MEMKEYS_RUN_TOKEN` for that one process. The token lives only for the session
  and is never written to disk/config.
- **Target end-state (follow-up, not a cutover blocker):** Paperclip injects a
  per-run short-lived token into the agent env so even the mint step leaves the
  pod. For the canary, in-pod mint from the secret is the accepted posture.

---

## 3. The `~/.claude.json` MCP server block

Swap the `mempalace` entry for `memkeys` (keep `mempalace` present but unused for
rollback). `type: "stdio"` with `kubectl exec` as the command; the inline script
mints a short-TTL token from the in-pod secret, exports the read-path env, then
`exec`s the server in MCP-only mode so stdin/stdout are handed straight to the
MCP server process (the `exec` is essential — no wrapper left holding the pipe).

```json
{
  "mcpServers": {
    "memkeys": {
      "type": "stdio",
      "command": "kubectl",
      "args": [
        "-n", "memkeys",
        "exec", "-i", "deploy/memkeys-server", "--",
        "sh", "-lc",
        "TOKEN=$(memkeys-token mint --secret-env MEMKEYS_RUN_TOKEN_SECRET --tenant \"$MEMKEYS_CANARY_TENANT\" --sub \"$MEMKEYS_AGENT_ID\" --ttl 3600); exec env MEMKEYS_ROLE=mcp MEMKEYS_RUN_TOKEN=\"$TOKEN\" memkeys-server"
      ],
      "env": {
        "KUBECONFIG": "/home/hrexed/.config/capmox/k8squad-test.kubeconfig",
        "MEMKEYS_AGENT_ID": "<paperclip-agent-uuid>",
        "MEMKEYS_CANARY_TENANT": "615e9564-65df-4465-9b19-a5afb73446c6"
      }
    }
  }
}
```

Notes:
- `MEMKEYS_ROLE=mcp`, `MEMKEYS_RUN_TOKEN`, `MEMKEYS_RUN_TOKEN_SECRET`,
  `MEMKEYS_DATABASE_URL`, `MEMKEYS_SEMANTIC=1`, and the embedder env are what the
  server reads. `MEMKEYS_RUN_TOKEN_SECRET` + `MEMKEYS_DATABASE_URL` + the
  embedder env already live in the pod (inherited by the `exec`'d process); only
  `MEMKEYS_ROLE` and the freshly minted `MEMKEYS_RUN_TOKEN` are set per session.
- `MEMKEYS_SEMANTIC=1` must be set on the **pod** (it already is on k8squad-test,
  per ISI-6327) so the inherited env enables `memory_search`.
- `memkeys-token mint` is the in-pod minting helper (see §4); if it is not yet
  packaged in the image, use the inline `sh` mint from §4 instead.
- No `MEMKEYS_HTTP_PORT` / `MEMKEYS_MEMORY_HTTP_PORT` appear — MCP-only binds
  nothing, so there are **no per-session ports to allocate**. This is the whole
  point of option (a).

---

## 4. Token-minting recipe (in-pod, per session)

The token is an HS256 JWT over the `memkeys-run-token` secret. If the image ships
a `memkeys-token mint` subcommand, prefer it. Otherwise mint inline in the `sh`
command with a tiny HS256 signer (secret already in the pod env as
`$MEMKEYS_RUN_TOKEN_SECRET`):

```sh
# header.payload.signature, HS256, exp = now + TTL seconds
b64() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
now=$(date +%s); exp=$((now + 3600))
hdr=$(printf '{"alg":"HS256","typ":"JWT"}' | b64)
pl=$(printf '{"company_id":"%s","sub":"%s","iat":%s,"exp":%s}' \
       "$MEMKEYS_CANARY_TENANT" "$MEMKEYS_AGENT_ID" "$now" "$exp" | b64)
sig=$(printf '%s.%s' "$hdr" "$pl" \
       | openssl dgst -sha256 -hmac "$MEMKEYS_RUN_TOKEN_SECRET" -binary | b64)
TOKEN="$hdr.$pl.$sig"
```

TTL = 3600s (1h) for the canary; shorten once the Paperclip-injected path
(§2 end-state) lands. The server re-checks `exp` on every call, so a long agent
session that outlives the token fails cleanly rather than silently — size the TTL
to the expected session length plus headroom, or have the wrapper re-mint.

---

## 5. Canary tenant / scoping

- **Tenant = the company UUID `615e9564-65df-4465-9b19-a5afb73446c6`** (the
  `company_id` claim = tenant floor). Clean-start dogfood (ISI-6307 decision C),
  so there is no legacy tenant to reconcile.
- `sub` = the Paperclip agent UUID → stamped as `entity.agent` on every write, so
  cross-agent recall within the tenant works (ProxOps verified session A → session
  B, same tenant, ISI-6328 transcripts).
- `memory_traverse` start ids are the **opaque composite** `tenant|entity.project|value`
  (dotted kind), not the raw value (ISI-6333) — unchanged by this transport.

---

## 6. Interim path to unblock the canary before this PR merges

The one-agent canary does **not** have to wait for the `MEMKEYS_ROLE=mcp` image.
ProxOps may start it today with option (c): the already-proven exec recipe that
spawns a full second server, with **randomized alt-ports + resource caps** so the
single canary session does not collide with the primary listeners:

```sh
exec env MEMKEYS_HTTP_PORT=$((20000 + RANDOM % 20000)) \
         MEMKEYS_MEMORY_HTTP_PORT=$((20000 + RANDOM % 20000)) \
         MEMKEYS_RUN_TOKEN="$TOKEN" memkeys-server
```

This is a **canary-only bridge** — it still double-runs the worker stack and
risks a rare port clash, which is exactly why the fleet flip waits for
`MEMKEYS_ROLE=mcp` (§1). Do not roll (c) out to multiple concurrent agents.

---

## 7. Strategic note — the long-term fleet transport

The ideal fleet-scale shape is **MCP-over-HTTP (streamable HTTP)** served at the
existing VIP (`10.0.0.220`), so agents use Claude Code's native remote MCP
(`type: "http"`, url + bearer header) with **no kubectl, no exec, and no
per-session process in the pod at all** — just the per-request bearer the
`/memory/*` path already proved. That is a new server transport (out of scope for
this cutover) and is filed as a follow-up. `MEMKEYS_ROLE=mcp` is the right answer
for shipping the cutover now; MCP-over-HTTP is the right answer for retiring
kubectl-exec later.
