//! Criterion micro-benchmarks for the CPU-bound hot paths of memkeys-server
//! (ISI-6100 / ISI-6094 P2). Runnable with `cargo bench -p memkeys-bench`.
//!
//! These complement the `write-bench` binary: `write-bench` measures the
//! end-to-end single-writer ceiling against a real Postgres (I/O bound), while
//! these criterion benches isolate the *pure* compute of the write pipeline and
//! the in-memory keyed lookup so a regression in the code — not the database —
//! is what gets tracked. No Postgres is required, so they run fast enough to be
//! scheduled in CI on `main` (see `.github/workflows/bench.yml`).
//!
//! Two hot paths, per the P2 brief:
//!   * **FactStore write pipeline** — stages 1-3 (`normalize` → `key_stage` →
//!     `temporalize`): envelope validation, SHA-256 body hash, UUIDv5
//!     idempotent id derivation, entity-key resolution.
//!   * **Attribute key lookup / index** — `MemStore::fetch_by_keys`: the keyed
//!     read with AND-filtering, newest-first sort, limit, and read-time
//!     supersession collapse (§5.2), over a representative corpus.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime;

use memkeys_bench::workload::{generate, WorkloadConfig};
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{EntityKey, KeyedQuery, Validity};
use memkeys_server::pipeline::{key_stage, normalize, temporalize, CallerContext, NoopResolver};
use memkeys_server::store::FactStore;

/// Single current-thread runtime shared by every async bench. The hot-path
/// futures resolve without yielding (NoopResolver does no I/O, MemStore is a
/// lock-guarded map), so `block_on` adds negligible, constant overhead.
fn rt() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current-thread runtime")
}

fn caller() -> CallerContext {
    CallerContext {
        tenant_id: "bench-tenant".into(),
        principal: "memkeys-bench".into(),
        trace_id: Some("bench-trace".into()),
    }
}

/// The canonical write-pipeline hot path: parse-validate, hash, derive id,
/// resolve keys, enforce the supersession grain.
fn bench_write_pipeline(c: &mut Criterion) {
    let rt = rt();
    let caller = caller();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    // One representative gateway_flush envelope (256-byte body, run anchor +
    // issue/project hints) — the dominant v1 write shape.
    let op = generate(&WorkloadConfig {
        ops: 1,
        ..WorkloadConfig::default()
    })
    .remove(0);
    let env = op.envelope;
    let resolver = NoopResolver;

    let mut g = c.benchmark_group("write_pipeline");
    g.throughput(Throughput::Elements(1));

    // Stage 1 — normalize (pure: SHA-256 body hash + UUIDv5 content-digest id).
    g.bench_function("normalize", |b| {
        b.iter(|| normalize(black_box(&env), black_box(&caller), black_box(now)).unwrap())
    });

    // Stage 2 — key (entity-key resolution + anchor enforcement, NoopResolver).
    g.bench_function("key_stage", |b| {
        b.iter_batched(
            || normalize(&env, &caller, now).unwrap(),
            |mut fact| {
                rt.block_on(key_stage(&env, &caller, &mut fact, &resolver))
                    .unwrap();
                black_box(fact)
            },
            BatchSize::SmallInput,
        )
    });

    // Stage 3 — temporalize (supersession-grain guard).
    g.bench_function("temporalize", |b| {
        b.iter_batched(
            || normalize(&env, &caller, now).unwrap(),
            |mut fact| {
                temporalize(&mut fact).unwrap();
                black_box(fact)
            },
            BatchSize::SmallInput,
        )
    });

    // Stages 1-3 end to end — the full per-op synchronous compute before the
    // stage-4 durable write.
    g.bench_function("stages_1_3", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut fact = normalize(&env, &caller, now).unwrap();
                key_stage(&env, &caller, &mut fact, &resolver)
                    .await
                    .unwrap();
                temporalize(&mut fact).unwrap();
                black_box(fact)
            })
        })
    });

    g.finish();
}

/// Attribute-key lookup / index: `fetch_by_keys` over a representative corpus,
/// across two corpus sizes so a complexity regression (e.g. losing the limit
/// truncation or the collapse bound) shows up as a super-linear slope.
fn bench_keyed_lookup(c: &mut Criterion) {
    let rt = rt();
    let caller = caller();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-17T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let resolver = NoopResolver;

    let mut g = c.benchmark_group("keyed_lookup");

    for corpus in [1_000u64, 8_000u64] {
        // Build the store once per size, outside the timed loop, through the
        // real pipeline so the committed records match production shape.
        let store = Arc::new(MemStore::new());
        let ops = generate(&WorkloadConfig {
            ops: corpus,
            key_cardinality: 64,
            ..WorkloadConfig::default()
        });
        rt.block_on(async {
            for op in &ops {
                let mut fact = normalize(&op.envelope, &caller, now).unwrap();
                key_stage(&op.envelope, &caller, &mut fact, &resolver)
                    .await
                    .unwrap();
                temporalize(&mut fact).unwrap();
                store.commit_fact(&fact).await.unwrap();
            }
        });

        // Query one issue key: scans the full corpus, matches ~corpus/64 rows,
        // sorts newest-first, truncates to limit, collapses supersessions.
        let query = KeyedQuery {
            keys: vec![(EntityKey::Issue, "bench-issue-0".into())],
            fact_type: None,
            as_of: None,
            valid: Validity::Current,
            limit: KeyedQuery::DEFAULT_LIMIT,
        };

        g.throughput(Throughput::Elements(corpus));
        g.bench_with_input(BenchmarkId::from_parameter(corpus), &corpus, |b, _| {
            b.iter(|| {
                let rows = rt
                    .block_on(store.fetch_by_keys("bench-tenant", black_box(&query)))
                    .unwrap();
                black_box(rows)
            })
        });
    }

    g.finish();
}

criterion_group! {
    name = benches;
    // Keep CI runtime bounded while retaining statistical signal.
    config = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = bench_write_pipeline, bench_keyed_lookup
}
criterion_main!(benches);
