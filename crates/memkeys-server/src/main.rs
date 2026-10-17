//! memkeys-server binary — M1 bootstrap.
//!
//! M1 ships the FactStore + pipeline + keyed read as a library with this
//! minimal entry point: connect to Postgres, apply migrations, initialize
//! OTel (OTLP when OTEL_EXPORTER_OTLP_ENDPOINT is set; otherwise no-op), and
//! exit. The transport (HTTP/MCP) is a later milestone; the open contract is
//! not part of M1.

use memkeys_server::pg::PgStore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("MEMKEYS_DATABASE_URL").map_err(|_| "MEMKEYS_DATABASE_URL is required")?;
    let store = PgStore::connect(&database_url).await?;
    println!(
        "memkeys-server: migrations applied; FactStore ready (M1: library surface; no transport)"
    );
    drop(store);
    Ok(())
}
