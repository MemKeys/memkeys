//! M4 local check — a five-minute collector/Jaeger walkthrough of the MemKeys
//! write path and keyed read. See `docs/observability/local-check.md`.
//!
//! Runs the REAL service (stages 1-4 + op 1) over the in-memory FactStore, so
//! no Postgres is needed: the traces you see in Jaeger are the same spans the
//! production wiring emits, per the WS-7 signal catalog.
//!
//! ```sh
//! # 1. start Jaeger (OTLP gRPC on :4317):
//! docker run --rm -p 16686:16686 -p 4317:4317 jaegertracing/all-in-one:latest
//! # 2. run this example pointed at it:
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
//!   cargo run -p memkeys-server --example otel_local_check
//! # 3. open http://localhost:16686 — service "memkeys"
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};

use memkeys_server::envelope::Envelope;
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{EntityKey, KeyedQuery};
use memkeys_server::pipeline::{CallerContext, NoopResolver};
use memkeys_server::service::MemKeysService;
use memkeys_server::telemetry_setup::Otel;

fn envelope(body: &str) -> Envelope {
    serde_json::from_value(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "project": "paperclip", "issue": "ISI-6078" },
        "fact": { "body": body }
    }))
    .expect("valid envelope")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // M4 wiring: OTLP when OTEL_EXPORTER_OTLP_ENDPOINT is set, else no-op.
    let otel = Otel::init()?;
    println!(
        "otel mode: {:?} (profile: {:?})",
        otel.mode(),
        otel.profile()
    );

    let service = MemKeysService::with_resolver(Arc::new(MemStore::new()), Arc::new(NoopResolver));
    let caller = CallerContext {
        tenant_id: "t-demo".into(),
        principal: "agent-demo".into(),
        trace_id: None,
    };
    let now = Utc.with_ymd_and_hms(2026, 10, 17, 10, 0, 0).unwrap();

    // 1) Write path — one trace: memkeys.fact.ingest (SERVER)
    //    → normalize / key / temporalize (INTERNAL) → memkeys.commit (CLIENT, db.*).
    let receipt = service
        .ingest(
            &envelope("M4 wiring decision: OTLP via collector, head sampling by profile"),
            &caller,
            now,
        )
        .await?;
    println!(
        "ingested fact {} (keyed: {})",
        receipt.fact_id, receipt.keyed
    );

    // 2) A rejected write — logs the memkeys.write.rejected key event,
    //    correlated to its (error) ingest trace.
    let bad = serde_json::from_value::<Envelope>(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:01Z" },
        "entity": { "project": "paperclip" },
        "fact": { "body": "" }
    }))?;
    match service.ingest(&bad, &caller, now).await {
        Err(memkeys_server::error::IngestError::Rejected { reason }) => {
            println!("rejected as expected: {reason}");
        }
        other => return Err(format!("expected rejection, got {other:?}").into()),
    }

    // 3) Keyed read — memkeys.retrieve (SERVER) → memkeys.store.query (CLIENT).
    let rows = service
        .fetch_by_keys(
            "t-demo",
            &KeyedQuery {
                keys: vec![(EntityKey::Project, "paperclip".into())],
                fact_type: None,
                as_of: None,
                valid: Default::default(),
                limit: KeyedQuery::DEFAULT_LIMIT,
            },
        )
        .await?;
    println!("keyed read returned {} fact(s)", rows.len());

    // Give the periodic metric reader a beat to ship, then flush everything.
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(otel);
    println!("flushed; open Jaeger and search for service \"memkeys\"");
    Ok(())
}
