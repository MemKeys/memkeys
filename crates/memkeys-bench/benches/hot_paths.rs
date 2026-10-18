//! Criterion microbenchmarks for the CPU-bound hot paths of memkeys-server.
//!
//! These are the regression-tracked paths run by `cargo bench` and the
//! `bench.yml` workflow on `main`. They are deliberately **database-free**:
//! the DB-bound single-writer ceiling is owned by the `write-bench` binary
//! (sustained ops/sec against a real Postgres, run by `scripts/run-write-bench.sh`),
//! which is too heavy and too environment-sensitive for per-commit criterion
//! sampling. Here we isolate the pure, deterministic CPU work so a regression
//! shows up as a clean statistical signal, not postgres jitter.
//!
//! Two hot paths (the ISI-6100 / ISI-6094-P2 targets):
//!   1. **Write pipeline** — `pipeline::normalize` (stage 1-2 key extraction +
//!      principal-salted fact.id) and `pipeline::temporalize` (stage 3
//!      bi-temporal placement).
//!   2. **Attribute key lookup / index** — `MemStore::fetch_by_keys` (op 1,
//!      §7.2) over a populated in-memory FactStore, plus the stage-4
//!      `commit_fact` insert and idempotent-duplicate paths.
//!
//! All inputs derive from the pinned deterministic workload
//! (`memkeys_bench::workload`) so benchmarks are reproducible across machines.

use chrono::{DateTime, Utc};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};

use memkeys_bench::workload::{generate, WorkloadConfig};
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{EntityKey, EntityKeys, FactType, KeyedQuery, ProcessedFact, Validity};
use memkeys_server::pipeline::{normalize, temporalize, CallerContext};
use memkeys_server::store::FactStore;
use memkeys_server::Envelope;

/// Fixed wall-clock the normalize/temporalize stages are stamped against — the
/// benchmark must not read the real clock (non-determinism) and must not drift
/// run-to-run.
fn bench_now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
        .expect("fixed now")
        .with_timezone(&Utc)
}

fn caller() -> CallerContext {
    CallerContext {
        tenant_id: "bench-tenant".to_string(),
        principal: "memkeys-bench".to_string(),
        trace_id: None,
    }
}

/// One realistic gateway_flush envelope from the pinned workload.
fn sample_envelopes(n: u64) -> Vec<Envelope> {
    let cfg = WorkloadConfig {
        ops: n,
        ..WorkloadConfig::default()
    };
    generate(&cfg).into_iter().map(|op| op.envelope).collect()
}

/// Write pipeline stage 1-2: normalize (required-field checks, canonicalization,
/// principal-salted idempotency id). Pure, no DB.
fn bench_normalize(c: &mut Criterion) {
    let envelopes = sample_envelopes(256);
    let caller = caller();
    let now = bench_now();

    let mut group = c.benchmark_group("write_pipeline");
    group.throughput(Throughput::Elements(1));
    group.bench_function("normalize", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let env = &envelopes[i % envelopes.len()];
            i += 1;
            normalize(std::hint::black_box(env), &caller, now).expect("normalize ok")
        });
    });
    group.finish();
}

/// Write pipeline stage 3: temporalize (bi-temporal placement). Operates on a
/// pre-normalized fact; the clone is done in untimed setup via iter_batched.
fn bench_temporalize(c: &mut Criterion) {
    let caller = caller();
    let now = bench_now();
    let template = normalize(&sample_envelopes(1)[0], &caller, now).expect("normalize ok");

    let mut group = c.benchmark_group("write_pipeline");
    group.throughput(Throughput::Elements(1));
    group.bench_function("temporalize", |b| {
        b.iter_batched(
            || template.clone(),
            |mut fact| {
                temporalize(&mut fact).expect("temporalize ok");
                fact
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

/// Build a keyed `ProcessedFact` directly (stage 2 `key_stage` is crate-private,
/// so we construct the committed shape the store sees). Facts spread over
/// `cardinality` distinct issue keys, matching the workload's read fan-in.
fn keyed_fact(i: u64, cardinality: u64) -> ProcessedFact {
    let issue = format!("bench-issue-{}", i % cardinality);
    // UUIDv5 keeps ids stable across runs (no real randomness in a benchmark).
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("fact-{i}").as_bytes());
    let event_time = bench_now() + chrono::Duration::milliseconds(i as i64);
    ProcessedFact {
        id,
        tenant_id: "bench-tenant".into(),
        fact_type: FactType::Context,
        subject: Some(format!("bench-statement-{i}")),
        body: format!("bench body {i}"),
        body_hash: String::new(),
        confidence: Some(0.9),
        supersedes: None,
        event_time,
        ingested_at: event_time,
        entity: EntityKeys {
            project: Some("bench-project".into()),
            issue: Some(issue),
            run: Some("bench-run".into()),
            ..EntityKeys::default()
        },
        keyed: true,
        provenance_principal: "memkeys-bench".into(),
        provenance_source_kind: "gateway_flush".into(),
        provenance_trace_id: None,
        rejected_hints: Vec::new(),
        attachments: serde_json::Value::Null,
    }
}

/// A single-threaded tokio runtime to drive the async FactStore port. The store
/// itself is in-memory (RwLock over a HashMap); the runtime overhead is
/// constant across samples, so the delta we measure is the lookup/index work.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
}

fn populated_store(rt: &tokio::runtime::Runtime, n: u64, cardinality: u64) -> MemStore {
    let store = MemStore::new();
    rt.block_on(async {
        for i in 0..n {
            store
                .commit_fact(&keyed_fact(i, cardinality))
                .await
                .expect("seed commit");
        }
    });
    store
}

/// Attribute key lookup / index — op 1 `fetch_by_keys` over a store whose size
/// scales so a regression in the linear scan + collapse is visible.
fn bench_fetch_by_keys(c: &mut Criterion) {
    let rt = rt();
    let cardinality = 64u64;

    let mut group = c.benchmark_group("attr_key_lookup");
    for &n in &[1_000u64, 10_000u64] {
        let store = populated_store(&rt, n, cardinality);
        let query = KeyedQuery {
            keys: vec![(EntityKey::Issue, "bench-issue-7".into())],
            fact_type: None,
            as_of: None,
            valid: Validity::Current,
            limit: 50,
        };
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new("fetch_by_keys", n), &n, |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    store
                        .fetch_by_keys(std::hint::black_box("bench-tenant"), &query)
                        .await
                        .expect("fetch ok")
                })
            });
        });
    }
    group.finish();
}

/// Stage 4 commit — the fresh-insert path (HashMap insert + record build) and
/// the idempotent-duplicate path (lookup + body compare, no write).
fn bench_commit(c: &mut Criterion) {
    let rt = rt();
    let cardinality = 64u64;

    let mut group = c.benchmark_group("write_pipeline");
    group.throughput(Throughput::Elements(1));

    group.bench_function("commit_fact_insert", |b| {
        b.iter_batched(
            MemStore::new,
            |store| {
                rt.block_on(async {
                    store
                        .commit_fact(&keyed_fact(0, cardinality))
                        .await
                        .expect("insert ok")
                })
            },
            BatchSize::SmallInput,
        );
    });

    // Duplicate path: id already present with the same body -> idempotent ack.
    let dup_store = MemStore::new();
    let dup = keyed_fact(0, cardinality);
    rt.block_on(async { dup_store.commit_fact(&dup).await.expect("seed") });
    group.bench_function("commit_fact_duplicate", |b| {
        b.iter(|| rt.block_on(async { dup_store.commit_fact(&dup).await.expect("dup ok") }));
    });

    group.finish();
}

criterion_group!(
    hot_paths,
    bench_normalize,
    bench_temporalize,
    bench_fetch_by_keys,
    bench_commit
);
criterion_main!(hot_paths);
