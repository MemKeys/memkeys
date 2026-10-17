//! memkeys-server binary — M1 bootstrap with M4 OTel wiring.
//!
//! M1 ships the FactStore + pipeline + keyed read as a library with this
//! minimal entry point: wire OTel (OTLP when OTEL_EXPORTER_OTLP_ENDPOINT is
//! set; otherwise no-op), connect to Postgres, apply migrations, and exit.
//! The transport (HTTP/MCP) is a later milestone; the open contract is
//! not part of M1.

use memkeys_server::pg::PgStore;
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
    println!(
        "memkeys-server: migrations applied; FactStore ready (M1: library surface; no transport); otel={:?}",
        otel.mode()
    );
    drop(store);
    drop(otel);
    Ok(())
}
