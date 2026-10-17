//! `write-bench` — the single-writer write-path ceiling benchmark (M8).
//!
//! Measures the synchronous stages 1-4 path (§6) through the real
//! `MemKeysService` against a real Postgres: one writer, sequential ops, ack
//! gated on stage 4 — exactly the deployment profile whose ceiling v1 promotion
//! decisions need. Reports sustained ops/sec and p50/p95/p99 ingest latency,
//! plus a supplementary op-1 keyed-read phase over the written keys.
//!
//! Reproducible: fixed-seed deterministic workload (see `workload`), warmup
//! excluded, config echoed into the JSON report. The scripted run is
//! `scripts/run-write-bench.sh`.

use std::sync::Arc;
use std::time::Instant;

use memkeys_bench::workload::{generate, WorkloadConfig};
use memkeys_bench::PhaseStats;
use memkeys_server::model::{EntityKey, KeyedQuery, Validity};
use memkeys_server::pg::PgStore;
use memkeys_server::pipeline::{CallerContext, NoopResolver};
use memkeys_server::service::MemKeysService;

#[tokio::main]
async fn main() {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!(
                "usage: write-bench [--url URL] [--ops N] [--warmup N] [--read-ops N] \
                 [--seed N] [--supersede-ratio F] [--key-cardinality N] [--body-bytes N] \
                 [--json PATH|-]"
            );
            std::process::exit(2);
        }
    };

    let url = cfg
        .url
        .or_else(|| std::env::var("MEMKEYS_DATABASE_URL").ok())
        .or_else(|| std::env::var("TEST_DATABASE_URL").ok())
        .unwrap_or_default();
    if url.is_empty() {
        eprintln!("no database url: pass --url or set MEMKEYS_DATABASE_URL/TEST_DATABASE_URL");
        std::process::exit(2);
    }

    let store = match PgStore::connect(&url).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("connect + migrate failed: {e}");
            std::process::exit(2);
        }
    };
    let pg_version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(store.pool())
        .await
        .unwrap_or_else(|_| "unknown".into());
    let service = MemKeysService::with_resolver(store, Arc::new(NoopResolver));

    // Unique tenant per run: reruns never collide with previous rows.
    let tenant = format!("bench-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S-%f"));
    let caller = CallerContext {
        tenant_id: tenant.clone(),
        principal: "memkeys-bench".into(),
        trace_id: None,
    };

    // --- warmup (excluded from every reported number) ---
    let mut wcfg = WorkloadConfig {
        seed: cfg.seed,
        ops: cfg.warmup,
        supersede_ratio: cfg.supersede_ratio,
        key_cardinality: cfg.key_cardinality,
        body_bytes: cfg.body_bytes,
    };
    wcfg.seed ^= 0x5A5A_5A5A_5A5A_5A5A; // different stream than the measured run
    for op in generate(&wcfg) {
        if let Err(e) = service
            .ingest(&op.envelope, &caller, chrono::Utc::now())
            .await
        {
            eprintln!("warmup op failed: {e}");
            std::process::exit(2);
        }
    }

    // --- measured write loop: single writer, sequential ---
    let ops = generate(&WorkloadConfig {
        seed: cfg.seed,
        ops: cfg.ops,
        supersede_ratio: cfg.supersede_ratio,
        key_cardinality: cfg.key_cardinality,
        body_bytes: cfg.body_bytes,
    });
    let mut samples: Vec<f64> = Vec::with_capacity(cfg.ops as usize);
    let mut last_fact_id: Option<uuid::Uuid> = None;
    let mut failed = 0u64;
    let t0 = Instant::now();
    for op in &ops {
        let mut envelope = op.envelope.clone();
        if op.is_supersede {
            envelope.fact.supersedes = last_fact_id;
        }
        let t = Instant::now();
        match service.ingest(&envelope, &caller, chrono::Utc::now()).await {
            Ok(receipt) => {
                samples.push(t.elapsed().as_secs_f64() * 1000.0);
                last_fact_id = Some(receipt.fact_id);
            }
            Err(e) => {
                failed += 1;
                eprintln!("op failed: {e}");
            }
        }
    }
    let write_wall = t0.elapsed().as_secs_f64();
    if failed > 0 || samples.len() != cfg.ops as usize {
        eprintln!(
            "write phase incomplete: {failed} failed, {} of {} sampled",
            samples.len(),
            cfg.ops
        );
        std::process::exit(1);
    }
    let write_stats = PhaseStats::from_samples(samples, write_wall);

    // --- supplementary read phase: op 1 keyed fetch over the written keys ---
    let mut read_stats = None;
    if cfg.read_ops > 0 {
        let mut samples = Vec::with_capacity(cfg.read_ops as usize);
        let t0 = Instant::now();
        for i in 0..cfg.read_ops {
            let issue = format!("bench-issue-{}", (i as u32) % cfg.key_cardinality);
            let query = KeyedQuery {
                keys: vec![(EntityKey::Issue, issue)],
                fact_type: None,
                as_of: None,
                valid: Validity::Current,
                limit: 100,
            };
            let t = Instant::now();
            match service.fetch_by_keys(&tenant, &query).await {
                Ok(_) => samples.push(t.elapsed().as_secs_f64() * 1000.0),
                Err(e) => {
                    eprintln!("read op failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        let wall = t0.elapsed().as_secs_f64();
        read_stats = Some(PhaseStats::from_samples(samples, wall));
    }

    // --- report ---
    let report = serde_json::json!({
        "tool": "memkeys-write-bench",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": chrono::Utc::now().to_rfc3339(),
        "mode": "single-writer sequential (stages 1-4, ack on stage 4)",
        "tenant": tenant,
        "config": {
            "ops": cfg.ops,
            "warmup_ops": cfg.warmup,
            "read_ops": cfg.read_ops,
            "seed": cfg.seed,
            "supersede_ratio": cfg.supersede_ratio,
            "key_cardinality": cfg.key_cardinality,
            "body_bytes": cfg.body_bytes,
        },
        "environment": {
            "postgres": pg_version,
            "target": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        },
        "write_path": write_stats,
        "keyed_read": read_stats,
    });

    println!("=== MemKeys write-path benchmark (single-writer ceiling) ===");
    println!(
        "ops={} warmup={} seed={} supersede_ratio={} key_cardinality={} body_bytes={}",
        cfg.ops, cfg.warmup, cfg.seed, cfg.supersede_ratio, cfg.key_cardinality, cfg.body_bytes
    );
    println!(
        "write path  : {ops} ops in {wall:.3}s -> {ceiling:.0} ops/sec",
        ops = write_stats.ops,
        wall = write_stats.wall_secs,
        ceiling = write_stats.ops_per_sec
    );
    println!(
        "  ingest latency (ms): p50={:.3} p95={:.3} p99={:.3} max={:.3} mean={:.3}",
        write_stats.p50_ms,
        write_stats.p95_ms,
        write_stats.p99_ms,
        write_stats.max_ms,
        write_stats.mean_ms
    );
    if let Some(r) = &read_stats {
        println!(
            "keyed read  : {ops} ops -> {:.0} ops/sec | latency (ms): p50={:.3} p95={:.3} p99={:.3}",
            r.ops_per_sec, r.p50_ms, r.p95_ms, r.p99_ms,
            ops = r.ops
        );
    }
    println!(
        "single-writer ceiling: {:.0} ops/sec (sustained, stages 1-4)",
        write_stats.ops_per_sec
    );

    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    match cfg.json.as_deref() {
        Some("-") => println!("{json}"),
        Some(path) => {
            std::fs::write(path, &json).unwrap_or_else(|e| {
                eprintln!("failed to write {path}: {e}");
                std::process::exit(1);
            });
            println!("json report: {path}");
        }
        None => {}
    }
}

struct CliConfig {
    url: Option<String>,
    ops: u64,
    warmup: u64,
    read_ops: u64,
    seed: u64,
    supersede_ratio: f64,
    key_cardinality: u32,
    body_bytes: usize,
    json: Option<String>,
}

impl Default for CliConfig {
    fn default() -> Self {
        // Mirrors WorkloadConfig::default plus read phase.
        CliConfig {
            url: None,
            ops: 1000,
            warmup: 100,
            read_ops: 1000,
            seed: 0x6082_2026_1017,
            supersede_ratio: 0.05,
            key_cardinality: 64,
            body_bytes: 256,
            json: None,
        }
    }
}

fn parse_args() -> Result<CliConfig, String> {
    let mut cfg = CliConfig::default();
    let mut args = std::env::args();
    args.next(); // program name
    let need = |name: &str, it: &mut std::env::Args| -> Result<String, String> {
        it.next().ok_or_else(|| format!("missing value for {name}"))
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" | "--database-url" => cfg.url = Some(need("--url", &mut args)?),
            "--ops" => {
                cfg.ops = need("--ops", &mut args)?
                    .parse()
                    .map_err(|e| format!("--ops: {e}"))?
            }
            "--warmup" => {
                cfg.warmup = need("--warmup", &mut args)?
                    .parse()
                    .map_err(|e| format!("--warmup: {e}"))?
            }
            "--read-ops" => {
                cfg.read_ops = need("--read-ops", &mut args)?
                    .parse()
                    .map_err(|e| format!("--read-ops: {e}"))?
            }
            "--seed" => {
                cfg.seed = need("--seed", &mut args)?
                    .parse()
                    .map_err(|e| format!("--seed: {e}"))?
            }
            "--supersede-ratio" => {
                let v: f64 = need("--supersede-ratio", &mut args)?
                    .parse()
                    .map_err(|e| format!("--supersede-ratio: {e}"))?;
                if !(0.0..=1.0).contains(&v) {
                    return Err("--supersede-ratio must be within [0.0, 1.0]".into());
                }
                cfg.supersede_ratio = v;
            }
            "--key-cardinality" => {
                cfg.key_cardinality = need("--key-cardinality", &mut args)?
                    .parse()
                    .map_err(|e| format!("--key-cardinality: {e}"))?
            }
            "--body-bytes" => {
                cfg.body_bytes = need("--body-bytes", &mut args)?
                    .parse()
                    .map_err(|e| format!("--body-bytes: {e}"))?
            }
            "--json" => cfg.json = Some(need("--json", &mut args)?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if cfg.ops == 0 || cfg.warmup == 0 {
        return Err("--ops and --warmup must be >= 1".into());
    }
    Ok(cfg)
}
