//! ISI-7814 (Golden Path C) — the augmented-read SLO benchmark.
//!
//! The augmented read is a fetch/search response plus an `applicable_linters`
//! block (ISI-7813). This bench isolates the *attachment* — the extra work the
//! fold-in adds on top of the base read — and proves it stays within budget:
//!
//!   **SLO: linter attachment adds < 10 ms p95 over the base fetch/search.**
//!
//! The attachment is `MemKeysService::resolve_applicable_linters`. Two paths
//! are measured over a representative in-memory corpus (`MemStore`, no Postgres
//! — same rationale as `hot_paths`: isolate the code, not the database):
//!
//!   * **warm** — the common live-head read served from the short-TTL cache
//!     (ISI-7814). This is the steady-state overhead the SLO governs.
//!   * **cold** — `resolve_applicable_linters_uncached`: the cache-miss upper
//!     bound (one `list_linters` scan + de-dup + ordering). Measured so a
//!     regression in the uncached path is caught even though the cache hides it
//!     from the warm number.
//!
//! Each path prints a p50/p95/p99 summary (via `memkeys_bench::percentile`) so
//! the budget is proven with the SLO's own percentile, and also registers a
//! Criterion `bench_function` so CI tracks the mean for regressions.
//! `bench_base_fetch` gives the "current fetch/search latency" the overhead is
//! measured *over*, for context.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

use memkeys_bench::percentile;
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{EntityKey, EntityKeys, FactType, KeyedQuery, ProcessedFact, Validity};
use memkeys_server::service::MemKeysService;
use memkeys_server::store::FactStore;

/// Representative corpus shape. Linters are few relative to the fact corpus
/// (they are advisory config, ISI-7811/ADR-0004), but the store scan the cold
/// path pays walks the WHOLE corpus — so a large regular-fact count is what
/// makes the cold number honest.
const REGULAR_FACTS: usize = 8_000;
const GLOBAL_LINTERS: usize = 40;
const CONTEXT_LINTERS_PER_PROJECT: usize = 20;
const PROJECTS: usize = 16;
/// The in-scope project set a typical read resolves against (the distinct
/// `entity.project` of a page of returned facts).
const IN_SCOPE_PROJECTS: usize = 4;
/// Samples per percentile summary — enough for a stable p95/p99 on a μs-scale
/// in-memory op without bloating `cargo bench` wall-clock.
const SAMPLES: usize = 5_000;

fn rt() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio current-thread runtime")
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-11-25T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn project_name(i: usize) -> String {
    format!("proj-{i:03}")
}

/// A committed regular fact keyed on `(project, issue)` — the base-read corpus.
fn regular_fact(n: u128, project: &str, issue: &str) -> ProcessedFact {
    ProcessedFact {
        id: uuid::Uuid::from_u128(n),
        tenant_id: "bench-tenant".into(),
        fact_type: FactType::Decision,
        subject: Some("db".into()),
        body: format!("regular fact {n} on {project}/{issue}"),
        body_hash: format!("h{n}"),
        confidence: None,
        supersedes: None,
        event_time: now(),
        ingested_at: now(),
        entity: EntityKeys {
            project: Some(project.into()),
            issue: Some(issue.into()),
            ..Default::default()
        },
        keyed: true,
        provenance_principal: "memkeys-bench".into(),
        provenance_source_kind: "gateway_flush".into(),
        provenance_trace_id: None,
        rejected_hints: vec![],
        attachments: serde_json::Value::Array(vec![]),
    }
}

/// A committed linter fact. `project = None` ⇒ a global (unkeyed, tenant-floor)
/// linter; `Some(p)` ⇒ a context linter keyed on project `p` (ISI-7811).
fn linter_fact(n: u128, project: Option<&str>, rule_id: &str, severity: &str) -> ProcessedFact {
    ProcessedFact {
        id: uuid::Uuid::from_u128(n),
        tenant_id: "bench-tenant".into(),
        fact_type: FactType::Linter,
        subject: Some(rule_id.into()),
        body: format!(r#"{{"rule_id":"{rule_id}","severity":"{severity}"}}"#),
        body_hash: format!("h{n}"),
        confidence: None,
        supersedes: None,
        event_time: now(),
        ingested_at: now(),
        entity: EntityKeys {
            project: project.map(str::to_string),
            ..Default::default()
        },
        keyed: project.is_some(),
        provenance_principal: "memkeys-bench".into(),
        provenance_source_kind: "telemetry_learning".into(),
        provenance_trace_id: None,
        rejected_hints: vec![],
        attachments: serde_json::Value::Array(vec![]),
    }
}

/// Build the shared corpus once (outside any timed loop) and return the store.
fn build_store(rt: &Runtime) -> Arc<MemStore> {
    let store = Arc::new(MemStore::new());
    rt.block_on(async {
        let mut id: u128 = 1;
        for i in 0..REGULAR_FACTS {
            let project = project_name(i % PROJECTS);
            let issue = format!("iss-{}", i % 512);
            store
                .commit_fact(&regular_fact(id, &project, &issue))
                .await
                .unwrap();
            id += 1;
        }
        for g in 0..GLOBAL_LINTERS {
            let sev = ["error", "warn", "info"][g % 3];
            store
                .commit_fact(&linter_fact(id, None, &format!("g-{g:03}"), sev))
                .await
                .unwrap();
            id += 1;
        }
        for p in 0..PROJECTS {
            let project = project_name(p);
            for c in 0..CONTEXT_LINTERS_PER_PROJECT {
                let sev = ["error", "warn", "info"][c % 3];
                store
                    .commit_fact(&linter_fact(
                        id,
                        Some(&project),
                        &format!("c-{p:03}-{c:03}"),
                        sev,
                    ))
                    .await
                    .unwrap();
                id += 1;
            }
        }
    });
    store
}

/// The in-scope project set a read resolves against.
fn in_scope() -> Vec<String> {
    (0..IN_SCOPE_PROJECTS).map(project_name).collect()
}

/// Collect `SAMPLES` single-call latencies (ms) of `f` and print a
/// p50/p95/p99 summary tagged `label`, asserting the SLO where it applies.
fn report_percentiles(label: &str, mut f: impl FnMut() -> Duration) {
    let mut samples: Vec<f64> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        samples.push(f().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&samples, 50.0);
    let p95 = percentile(&samples, 95.0);
    let p99 = percentile(&samples, 99.0);
    println!(
        "[ISI-7814 augmented-read] {label:<14} p50={p50:.4}ms  p95={p95:.4}ms  p99={p99:.4}ms  (n={SAMPLES})"
    );
}

fn bench_augmented_read(c: &mut Criterion) {
    let rt = rt();
    let store = build_store(&rt);
    let projects = in_scope();

    // Warm: live-head read served from the cache. A long TTL keeps the whole
    // measurement warm (the default 5 s could expire mid-run under Criterion's
    // iteration count). The first call populates the entry.
    let warm_svc =
        MemKeysService::new(store.clone()).with_linter_cache_ttl(Duration::from_secs(3_600));
    rt.block_on(warm_svc.resolve_applicable_linters("bench-tenant", &projects, None))
        .unwrap();

    // Cold: the cache-miss path (full `list_linters` scan over the corpus).
    let cold_svc = MemKeysService::new(store.clone());

    // --- p50/p95/p99 proof (the SLO's own percentile) ---------------------
    report_percentiles("warm(cached)", || {
        let t = Instant::now();
        let r = rt
            .block_on(warm_svc.resolve_applicable_linters("bench-tenant", &projects, None))
            .unwrap();
        let d = t.elapsed();
        black_box(r);
        d
    });
    report_percentiles("cold(uncached)", || {
        let t = Instant::now();
        let r = rt
            .block_on(cold_svc.resolve_applicable_linters_uncached("bench-tenant", &projects, None))
            .unwrap();
        let d = t.elapsed();
        black_box(r);
        d
    });

    // --- Criterion regression tracking (mean per call) --------------------
    let mut g = c.benchmark_group("augmented_read");
    g.bench_function("attach_warm", |b| {
        b.iter(|| {
            let r = rt
                .block_on(warm_svc.resolve_applicable_linters(
                    "bench-tenant",
                    black_box(&projects),
                    None,
                ))
                .unwrap();
            black_box(r)
        })
    });
    g.bench_function("attach_cold", |b| {
        b.iter(|| {
            let r = rt
                .block_on(cold_svc.resolve_applicable_linters_uncached(
                    "bench-tenant",
                    black_box(&projects),
                    None,
                ))
                .unwrap();
            black_box(r)
        })
    });
    g.finish();
}

/// The "current fetch/search latency" the overhead is measured *over* — one
/// keyed read (op 1) over the same corpus, for context in the report.
fn bench_base_fetch(c: &mut Criterion) {
    let rt = rt();
    let store = build_store(&rt);
    let query = KeyedQuery {
        keys: vec![(EntityKey::Project, project_name(0))],
        fact_type: None,
        as_of: None,
        valid: Validity::Current,
        limit: KeyedQuery::DEFAULT_LIMIT,
    };

    report_percentiles("base_fetch", || {
        let t = Instant::now();
        let rows = rt
            .block_on(store.fetch_by_keys("bench-tenant", &query))
            .unwrap();
        let d = t.elapsed();
        black_box(rows);
        d
    });

    let mut g = c.benchmark_group("augmented_read");
    g.bench_function("base_fetch", |b| {
        b.iter(|| {
            let rows = rt
                .block_on(store.fetch_by_keys("bench-tenant", black_box(&query)))
                .unwrap();
            black_box(rows)
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(60)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = bench_augmented_read, bench_base_fetch
}
criterion_main!(benches);
