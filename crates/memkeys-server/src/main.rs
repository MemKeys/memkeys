//! memkeys-server binary — M2 bootstrap.
//!
//! M1 shipped the FactStore + pipeline + keyed read as a library with a
//! minimal entry point. M2 adds the graph projection: connect, apply
//! migrations (FactStore + edges), then run the stage-5a projection worker —
//! an initial SoT catch-up scan (the projection is built FROM the FactStore)
//! followed by a periodic drain + hole scan. The transport (HTTP/MCP) is M3;
//! the open contract is not part of M2.

use std::sync::Arc;
use std::time::Duration;

use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::service::MemKeysService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    tokio::signal::ctrl_c().await?;
    maintenance.abort();
    println!("memkeys-server: shut down");
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
