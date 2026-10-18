//! memkeys-server binary — M3 HTTP + MCP transport.
//!
//! M1 shipped the FactStore + pipeline + keyed read as a library. M2 added the
//! graph projection with OTel wiring: connect, apply migrations (FactStore +
//! edges), then run the stage-5a projection worker — an initial SoT catch-up
//! scan (the projection is built FROM the FactStore) followed by a periodic
//! drain + hole scan. This binary now runs three surfaces concurrently:
//!   - Pod-readiness health (ISI-6132): `/healthz` liveness + `/readyz`
//!     readiness (a live DB ping) on `MEMKEYS_HTTP_PORT` (default 8080),
//!     matching the Helm chart's probes; a hand-rolled responder, see
//!     `health.rs`. Started before the (potentially long) catch-up scan so
//!     kubelet can reach the pod immediately after migrations.
//!   - M3 memory transport: an axum HTTP server on `MEMKEYS_MEMORY_HTTP_PORT`
//!     (default 3000) serving `/memory/*`, plus an MCP stdio server (JSON-RPC
//!     2.0 over stdin/stdout, WS-5 tools). A distinct port from the health
//!     surface so a Helm-set `MEMKEYS_HTTP_PORT` cannot collide with it.
//!   - Admission control (ISI-6148): per-request `JwtVerifier` on HTTP + a
//!     launch-bound `StaticVerifier` principal on stdio (§2.1), plus the
//!     `AdmissionControl` self-protection gates. The run-token issuer secret
//!     is provisioned via `MEMKEYS_RUN_TOKEN_SECRET`.

use std::sync::Arc;
use std::time::Duration;

use memkeys_server::admission::AdmissionControl;
use memkeys_server::auth::{JwtVerifier, Principal, StaticVerifier, TokenVerifier};
use memkeys_server::health;
use memkeys_server::http::AppState;
use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::pg_vector::PgVectorIndex;
use memkeys_server::service::MemKeysService;
use memkeys_server::telemetry_setup::Otel;
use memkeys_server::worker::VectorProjectionWorker;
use memkeys_server::VectorIndex;
use memkeys_server::{CategorizeWorker, DigestWorker, ExtractWorker};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // M4: providers + exporters + the tracing→OTel logs bridge. The guard is
    // held for the process lifetime; Drop flushes everything on exit.
    let otel = Otel::init()?;
    tracing::info!(target: "memkeys", "memkeys.server.started");

    let database_url =
        std::env::var("MEMKEYS_DATABASE_URL").map_err(|_| "MEMKEYS_DATABASE_URL is required")?;
    let store = PgStore::connect(&database_url).await?;
    let pool = store.pool().clone();
    let graph = Arc::new(PgGraph::from_pool(pool.clone()));
    let mut service = MemKeysService::with_graph_projection(
        Arc::new(store),
        Arc::new(memkeys_server::pipeline::NoopResolver),
        graph.clone(),
    );

    // M5: the optional semantic module (§5.4.2 "optional, activates on demand").
    // `MEMKEYS_SEMANTIC` truthy wires the fact-vector read path (embedder +
    // pgvector index) AND the stage-5b projection worker; otherwise op-4 keeps
    // returning `feature_disabled` (501) as in M3. The embedder is chosen by
    // `embed::from_env` (HTTP when MEMKEYS_EMBED_URL is set, else HashEmbedder).
    let semantic_on =
        std::env::var("MEMKEYS_SEMANTIC").is_ok_and(|v| matches!(v.as_str(), "1" | "on" | "true"));
    if semantic_on {
        let embedder = memkeys_server::embed::from_env();
        let vector: Arc<dyn VectorIndex> = Arc::new(PgVectorIndex::from_pool(pool.clone()));
        let vworker = Arc::new(VectorProjectionWorker::new(
            pool.clone(),
            embedder.clone(),
            vector.clone(),
        ));
        service = service
            .with_semantic(embedder, vector)
            .with_semantic_worker(vworker);
        tracing::info!(target: "memkeys", "memkeys.semantic.enabled: fact-vector tier + stage-5b worker on");
    }

    // M6 (ADR-llm-pipeline): the LLM pipeline workers 5c-5e behind the
    // pluggable generative endpoint. `generative::from_env` picks the adapter
    // (MEMKEYS_GEN_URL set ⇒ the HTTP endpoint; unset ⇒ the deterministic
    // TemplateGenerator so a minimal deployment — and CI — runs with no model
    // server). W-extract (5e) is always-on key enrichment; W-categorize (5d)
    // rides the `memory.semantic` gate; W-digest (5c) additionally requires
    // the `memory.semantic.digest` sub-flag (§5.4.2 ruling: a deployment can
    // buy fact-tier precision without paying for a summarizer).
    let generator = memkeys_server::generator_from_env();
    let extract_worker = Arc::new(ExtractWorker::new(pool.clone(), generator.clone()));
    service = service.with_extract_worker(extract_worker.clone());
    let mut categorize_worker: Option<Arc<CategorizeWorker>> = None;
    let mut digest_worker: Option<Arc<DigestWorker>> = None;
    if semantic_on {
        let cw = Arc::new(CategorizeWorker::new(pool.clone(), generator.clone()));
        service = service.with_categorize_worker(cw.clone());
        categorize_worker = Some(cw);
        let digest_on = std::env::var("MEMKEYS_SEMANTIC_DIGEST")
            .is_ok_and(|v| matches!(v.as_str(), "1" | "on" | "true"));
        if digest_on {
            // The digest worker embeds summaries with the SAME embedder as the
            // fact tier (ADR Decision 3: one embedder, both tiers, RRF-fusable).
            let embedder = memkeys_server::embedder_from_env();
            let dw = Arc::new(DigestWorker::new(pool.clone(), generator.clone(), embedder));
            service = service.with_digest_worker(dw.clone());
            digest_worker = Some(dw);
            tracing::info!(target: "memkeys", "memkeys.digest.enabled: stage-5c digest rollups on");
        }
    }

    let service = Arc::new(service);
    let worker = service
        .projection_worker()
        .expect("worker is wired with the graph projection");
    let vector_worker = service.vector_worker();

    // M7 (ISI-6081): the reconciler owns the per-tier `catch_up` SoT scans —
    // one cohesive component over every wired tier (graph always; vector / LLM
    // tiers ride their feature gates) instead of a hand-inlined list. It drives
    // both the boot convergence pass and the steady-state maintenance tick,
    // closing projection holes AND stale model-stamp rebuilds (incl. the M7
    // embedding-model re-embed) with collect-and-continue failure handling.
    let mut tiers: Vec<Arc<dyn memkeys_server::Reconcilable>> =
        vec![worker.clone() as Arc<dyn memkeys_server::Reconcilable>];
    if let Some(vw) = &vector_worker {
        tiers.push(vw.clone() as Arc<dyn memkeys_server::Reconcilable>);
    }
    tiers.push(extract_worker.clone() as Arc<dyn memkeys_server::Reconcilable>);
    if let Some(cw) = &categorize_worker {
        tiers.push(cw.clone() as Arc<dyn memkeys_server::Reconcilable>);
    }
    if let Some(dw) = &digest_worker {
        tiers.push(dw.clone() as Arc<dyn memkeys_server::Reconcilable>);
    }
    let reconciler = memkeys_server::Reconciler::new(tiers);

    // ISI-6132: the health listener starts before the (potentially long)
    // catch-up scan so kubelet probes can reach the pod immediately after
    // migrations; readiness is a live DB ping, not the scan state.
    let listener = health::bind(health::http_port()).await?;
    tracing::info!(
        target: "memkeys",
        port = listener.local_addr()?.port(),
        "memkeys.server.http.listen"
    );
    tokio::spawn(health::serve_pool(listener, graph.pool().clone()));

    println!("memkeys-server: migrations applied; FactStore + graph projection ready");

    // Initial convergence pass: the reconciler rebuilds every wired tier's
    // projection from the FactStore (holes + stale model-stamp rebuilds). It is
    // collect-and-continue — a tier's transient failure is logged, never
    // `?`-aborts the boot (ISI-6192 B2 class: one stuck tier must not wedge the
    // whole server start). The steady-state tick below retries.
    match reconciler.reconcile_all(graph.pool()).await {
        Ok(reports) => {
            for report in reports {
                let n = report.total_reconciled();
                if n > 0 {
                    println!(
                        "memkeys-server: reconciled {n} hole/stale facts for tenant {}",
                        report.tenant
                    );
                }
                // Per-tier backfill failures are logged inside reconcile_tenant
                // (ISI-6223 M2), so both this boot pass and the steady-state
                // tick surface them uniformly — no second loop needed here.
            }
        }
        Err(e) => {
            tracing::warn!(target: "memkeys", error = %e, "boot reconcile tenant scan failed")
        }
    }

    // ISI-6148: the run-token issuer secret (the deployment prerequisite). When
    // present the HTTP transport verifies every bearer per request and derives
    // the tenant floor server-side — closing the ISI-6137 S1 cross-tenant gap.
    // Absent = open loopback dev mode (tenant self-asserted); the bind gate
    // below refuses to expose that surface off loopback.
    let run_token_secret = std::env::var("MEMKEYS_RUN_TOKEN_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let admission = Arc::new(AdmissionControl::with_defaults());
    let http_verifier: Option<Arc<dyn TokenVerifier>> = run_token_secret
        .as_ref()
        .map(|s| Arc::new(JwtVerifier::new(s.clone())) as Arc<dyn TokenVerifier>);

    // M3 (ISI-6137 B3): bind loopback by DEFAULT so an unauthenticated server is
    // never network-reachable by accident. A non-loopback bind is permitted only
    // when the per-request verifier gates admission (MEMKEYS_RUN_TOKEN_SECRET) or
    // the operator explicitly accepts the risk (MEMKEYS_ALLOW_INSECURE=1);
    // otherwise we refuse to start rather than expose the open admission surface.
    //
    // The memory transport has its OWN port var, distinct from the health
    // surface's `MEMKEYS_HTTP_PORT` (ISI-6132, default 8080). Sharing it would
    // make a Helm-set service port collide with the /memory/* bind and crash
    // the process on the second listener; keep them independent.
    let http_port: u16 = std::env::var("MEMKEYS_MEMORY_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let http_host = std::env::var("MEMKEYS_HTTP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let has_verifier = http_verifier.is_some();
    let is_loopback = matches!(http_host.as_str(), "127.0.0.1" | "::1" | "localhost");
    let allow_insecure = std::env::var("MEMKEYS_ALLOW_INSECURE").is_ok_and(|v| v == "1");
    if !is_loopback && !has_verifier && !allow_insecure {
        return Err(format!(
            "refusing to bind {http_host}:{http_port}: a non-loopback bind with no \
             MEMKEYS_RUN_TOKEN_SECRET would expose an unauthenticated admission surface. Set \
             MEMKEYS_RUN_TOKEN_SECRET, keep the loopback default (127.0.0.1), or set \
             MEMKEYS_ALLOW_INSECURE=1 to override (NOT recommended)."
        )
        .into());
    }
    if !is_loopback && !has_verifier {
        tracing::warn!(
            target: "memkeys",
            "memkeys.admission.open: bound {http_host}:{http_port} with NO run-token verifier \
             (MEMKEYS_ALLOW_INSECURE); tenant is self-asserted — cross-tenant \
             impersonation is possible (ISI-6137 S1)."
        );
        eprintln!(
            "WARNING: memkeys-server bound {http_host}:{http_port} with no run-token verifier — \
             open admission surface, self-asserted tenant (ISI-6137 S1)."
        );
    }
    let app_state = AppState::new(service.clone(), http_verifier, admission.clone());
    let router = memkeys_server::http::router(app_state);
    let listener = tokio::net::TcpListener::bind((http_host.as_str(), http_port)).await?;
    println!("memkeys-server: HTTP listening on {http_host}:{http_port}");
    let http_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(target: "memkeys", "HTTP server error: {e}");
        }
    });

    // M3 (ISI-6148 §2.1): bind a SINGLE stdio principal at launch from the run
    // token — the stdio pipe is the trust boundary, so the principal is fixed
    // for the process and every tool call inherits it (the window is re-checked
    // per call for F12). With the issuer secret + a presented `MEMKEYS_RUN_TOKEN`
    // we verify it once; otherwise we bind a loopback dev principal (same open
    // posture the HTTP bind gate governs).
    let now = chrono::Utc::now().timestamp();
    let stdio_verifier: Arc<dyn TokenVerifier> = match (
        run_token_secret.as_ref(),
        std::env::var("MEMKEYS_RUN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    ) {
        (Some(secret), Some(token)) => {
            let principal = JwtVerifier::new(secret.clone())
                .verify(&token, now)
                .map_err(|e| format!("stdio launch run-token rejected: {}", e.detail))?;
            Arc::new(StaticVerifier::new(principal))
        }
        _ => {
            let tenant =
                std::env::var("MEMKEYS_STDIO_TENANT").unwrap_or_else(|_| "default".to_string());
            tracing::warn!(
                target: "memkeys",
                "memkeys.stdio.dev_principal: no MEMKEYS_RUN_TOKEN(+_SECRET); binding an \
                 UNVERIFIED stdio principal for tenant {tenant} (dev/loopback; ISI-6137 S1)."
            );
            Arc::new(StaticVerifier::new(Principal {
                tenant_id: tenant,
                agent_id: "stdio-dev".to_string(),
                run_id: None,
                expires_at: i64::MAX,
                trace_id: None,
            }))
        }
    };

    // M3: MCP stdio server (spawned as a concurrent task).
    let mcp_service = service.clone();
    let mcp_admission = admission.clone();
    let mcp_task = tokio::spawn(async move {
        memkeys_server::mcp::run_stdio(mcp_service, stdio_verifier, mcp_admission).await;
    });

    // Steady state: drain the post-commit queue(s) + rescan for holes.
    let loop_worker = worker.clone();
    let loop_vector_worker = vector_worker.clone();
    let loop_extract_worker = extract_worker.clone();
    let loop_categorize_worker = categorize_worker.clone();
    let loop_digest_worker = digest_worker.clone();
    let loop_reconciler = reconciler.clone();
    let maintenance = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            loop_worker.drain().await;
            if let Some(vw) = &loop_vector_worker {
                vw.drain().await;
            }
            // M6: the LLM pipeline workers share the maintenance cadence. The
            // 30s tick IS the W-digest debounce window — a burst of facts
            // coalesces into one rollup per scope per tick.
            loop_extract_worker.drain().await;
            if let Some(cw) = &loop_categorize_worker {
                cw.drain().await;
            }
            if let Some(dw) = &loop_digest_worker {
                dw.drain().await;
            }
            // Rescan every tenant for holes + stale model-stamp rebuilds across
            // all wired tiers, and emit `memkeys.projection.lag.current` (once
            // per kind, max across tenants). The reconciler collect-and-continues
            // internally, and reconcile_tenant logs each tier's backfill/lag
            // errors (ISI-6223 M2), so discarding the report here stays silent
            // only about success counts, never about failures.
            let _ = loop_reconciler
                .reconcile_all(loop_worker.graph_pool())
                .await;
        }
    });

    shutdown_signal().await?;
    maintenance.abort();
    http_server.abort();
    mcp_task.abort();
    println!("memkeys-server: shut down");
    // The OTel guard drops here at scope end — M4 Drop flushes providers/exporters.
    drop(otel);
    Ok(())
}

/// Wait for the orchestrator's termination signal. Kubernetes sends SIGTERM
/// (not SIGINT), and the graceful path matters: it runs the OTel flush above
/// instead of being SIGKILLed at the end of the termination grace period.
#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = term.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
