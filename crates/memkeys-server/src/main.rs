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
use memkeys_server::service::MemKeysService;
use memkeys_server::telemetry_setup::Otel;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // M4: providers + exporters + the tracing→OTel logs bridge. The guard is
    // held for the process lifetime; Drop flushes everything on exit.
    let otel = Otel::init()?;
    tracing::info!(target: "memkeys", "memkeys.server.started");

    let database_url =
        std::env::var("MEMKEYS_DATABASE_URL").map_err(|_| "MEMKEYS_DATABASE_URL is required")?;
    let store = PgStore::connect(&database_url).await?;
    let graph = Arc::new(PgGraph::from_pool(store.pool().clone()));
    let service = Arc::new(MemKeysService::with_graph_projection(
        Arc::new(store),
        Arc::new(memkeys_server::pipeline::NoopResolver),
        graph.clone(),
    ));
    let worker = service
        .projection_worker()
        .expect("worker is wired with the graph projection");

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

    // Initial convergence pass: build the projection from the FactStore.
    for tenant in distinct_tenants(graph.pool()).await? {
        let n = worker.catch_up(&tenant).await?;
        if n > 0 {
            println!("memkeys-server: projected {n} hole facts for tenant {tenant}");
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

    // Steady state: drain the post-commit queue + rescan for holes.
    let loop_worker = worker.clone();
    let maintenance = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            loop_worker.drain().await;
            if let Ok(tenants) = distinct_tenants(loop_worker.graph_pool()).await {
                for tenant in tenants {
                    let _ = loop_worker.catch_up(&tenant).await;
                }
            }
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

/// The tenants with committed facts (the catch-up scan grain).
async fn distinct_tenants(pool: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
    // DISTINCT keeps the scan bounded by tenant count, not fact count.
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT tenant_id FROM facts ORDER BY tenant_id")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}
