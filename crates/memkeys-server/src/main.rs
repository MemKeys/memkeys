//! memkeys-server binary — M2 bootstrap with M4 OTel wiring.
//!
//! M1 shipped the FactStore + pipeline + keyed read as a library with a
//! minimal entry point. M2 adds the graph projection: wire OTel (OTLP when
//! OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise no-op), connect, apply
//! migrations (FactStore + edges), then run the stage-5a projection worker —
//! an initial SoT catch-up scan (the projection is built FROM the FactStore)
//! followed by a periodic drain + hole scan.
//!
//! ISI-6132 additionally serves the pod-readiness health surface
//! (`/healthz` liveness, `/readyz` readiness with a DB ping) on
//! MEMKEYS_HTTP_PORT (default 8080), matching the Helm chart's probes. This
//! is not the M3 open-contract transport; see `health.rs`.

use std::sync::Arc;
use std::time::Duration;

use memkeys_server::health;
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
    let service = MemKeysService::with_graph_projection(
        Arc::new(store),
        Arc::new(memkeys_server::pipeline::NoopResolver),
        graph.clone(),
    );
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

    // Steady state: drain the post-commit queue + rescan for holes (the
    // reconciler/backfill of M7 subsumes the rescan; the queue drain is the
    // §6 stage-5a hot path).
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
