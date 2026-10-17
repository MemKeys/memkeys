//! memkeys-server binary — M3 HTTP + MCP transport.
//!
//! M1 shipped the FactStore + pipeline + keyed read as a library. M2 added
//! the graph projection with OTel wiring. M3 adds:
//!   - axum HTTP server on `MEMKEYS_HTTP_PORT` (default 3000) serving
//!     `/memory/*` routes.
//!   - MCP stdio server: JSON-RPC 2.0 over stdin/stdout, WS-5 tools.
//!   - Admission control: `X-MemKeys-Tenant` (required) + `X-MemKeys-Token`
//!     bearer check when `MEMKEYS_AUTH_TOKEN` is set.

use std::sync::Arc;
use std::time::Duration;

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

    println!("memkeys-server: migrations applied; FactStore + graph projection ready");

    // Initial convergence pass: build the projection from the FactStore.
    for tenant in distinct_tenants(graph.pool()).await? {
        let n = worker.catch_up(&tenant).await?;
        if n > 0 {
            println!("memkeys-server: projected {n} hole facts for tenant {tenant}");
        }
    }

    // M3: HTTP server.
    let http_port: u16 = std::env::var("MEMKEYS_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let app_state = AppState::new(service.clone());
    let router = memkeys_server::http::router(app_state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", http_port)).await?;
    println!("memkeys-server: HTTP listening on :{http_port}");
    let http_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(target: "memkeys", "HTTP server error: {e}");
        }
    });

    // M3: MCP stdio server (spawned as a concurrent task).
    let mcp_service = service.clone();
    let mcp_task = tokio::spawn(async move {
        memkeys_server::mcp::run_stdio(mcp_service).await;
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

    tokio::signal::ctrl_c().await?;
    maintenance.abort();
    http_server.abort();
    mcp_task.abort();
    println!("memkeys-server: shut down");
    // The OTel guard drops here at scope end — M4 Drop flushes providers/exporters.
    drop(otel);
    Ok(())
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
