//! MemKeys M1 — FactStore source of truth, write pipeline stages 1-4, keyed read.
//!
//! Grounded in `docs/architecture.md`:
//! - §3/§3.1 — the fact envelope and the per-source required/allowed/server-owned matrix
//! - §4      — key extraction (verify → enrich → accept-low-trust)
//! - §5.1    — append-only, bi-temporal record shape; supersession is read-time collapse
//! - §6      — write pipeline stages 1-4 (synchronous, authoritative; ack gated on stage 4)
//! - §7.2    — Operation 2, fetch-by-keys (the M1 "op 1 keyed read" surface)
//!
//! OTel instrumentation follows the WS-7 signal catalog and semconv pins
//! (docs/observability/signal-catalog.yaml, semconv-pins.yaml; pinned release 1.30.0):
//! the write path is one trace `memkeys.fact.ingest` → normalize/key/temporalize →
//! `memkeys.commit` (db.* attributes, db.query.text denied), retrieval is
//! `memkeys.retrieve` → `memkeys.store.query`.

pub mod envelope;
pub mod error;
pub mod graph;
pub mod http;
pub mod mcp;
pub mod memstore;
pub mod model;
pub mod pg;
pub mod pg_graph;
pub mod pipeline;
pub mod service;
pub mod store;
pub mod telemetry;
pub mod telemetry_setup;
pub mod worker;

pub use envelope::{EntityHints, Envelope, FactBlock, SourceBlock, SourceKind};
pub use error::{IngestError, QueryError};
pub use graph::{
    BranchPoint, Cursor, Direction, EdgeType, FactRef, GraphError, GraphView, LinksReq,
    LinksResult, Node, NodeKind, NodeRef, ProjectOutcome, RebuildStats, Scope, SupersedesReq,
    SupersedesResult, TraverseReq, TraverseResult, TruncReason,
};
pub use model::{EntityKey, EntityKeys, FactRecord, KeyedQuery, ProcessedFact, Validity};
pub use pg::PgStore;
pub use pg_graph::PgGraph;
pub use pipeline::{CallerContext, EntityResolver, NoopResolver, RunBinding, ThreadBinding};
pub use service::{IngestReceipt, MemKeysService};
pub use store::{CommitOutcome, FactStore};
pub use worker::GraphProjectionWorker;
