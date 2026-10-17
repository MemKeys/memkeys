//! MemKeys M8 — write-path benchmark + conformance harness.
//!
//! Two binaries over the real M1 surfaces (`MemKeysService` + `PgStore`):
//! - `write-bench`  — the single-writer write-path ceiling: sustained ops/sec
//!   and per-op latency percentiles (p50/p95/p99) for stages 1-4 ingest, plus a
//!   supplementary keyed-read phase (op 1). Deterministic workload, scripted
//!   reproducible run (`scripts/run-write-bench.sh`).
//! - `conformance`  — the conformance suite: numbered checks of the §3/§4/§5/§6
//!   invariants and the FactStore contract, executed against a real Postgres.
//!   Any FAIL fails the run (exit 1) — the promotion gate of
//!   docs/promotion-thresholds.md.
//!
//! Both binaries connect with `--url` or `MEMKEYS_DATABASE_URL` (falling back
//! to `TEST_DATABASE_URL`), so CI's postgres service and a local Postgres work
//! identically.

pub mod workload;

/// Deterministic xorshift64* RNG — the workload is fully reproducible from the
/// seed; no external rand dependency needed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the zero fixed point.
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// Nearest-rank percentile over sorted-in-place samples (p in 0..=100).
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let rank = rank.clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Latency/throughput summary of one benchmark phase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhaseStats {
    pub ops: u64,
    /// Wall-clock seconds of the timed loop (excludes warmup).
    pub wall_secs: f64,
    /// Sustained throughput of the phase.
    pub ops_per_sec: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

impl PhaseStats {
    /// Compute from raw per-op latencies (milliseconds) and the wall-clock
    /// duration of the whole timed loop.
    pub fn from_samples(samples_ms: Vec<f64>, wall_secs: f64) -> Self {
        let mut sorted = samples_ms;
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN latencies"));
        let ops = sorted.len() as u64;
        let mean = sorted.iter().sum::<f64>() / ops as f64;
        PhaseStats {
            ops,
            wall_secs,
            ops_per_sec: ops as f64 / wall_secs,
            p50_ms: percentile(&sorted, 50.0),
            p95_ms: percentile(&sorted, 95.0),
            p99_ms: percentile(&sorted, 99.0),
            max_ms: sorted[sorted.len() - 1],
            mean_ms: mean,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_advances() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        assert_eq!(a.next_u64(), b.next_u64());
        assert_eq!(a.next_u64(), b.next_u64());
        let first = a.next_u64();
        assert_ne!(first, a.next_u64(), "sanity: sequence advances");
    }

    #[test]
    fn percentile_nearest_rank() {
        // 1..=100 sorted: nearest-rank p50 = 50, p95 = 95, p99 = 99, max = 100.
        let sorted: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile(&sorted, 50.0), 50.0);
        assert_eq!(percentile(&sorted, 95.0), 95.0);
        assert_eq!(percentile(&sorted, 99.0), 99.0);
        assert_eq!(percentile(&sorted, 100.0), 100.0);
        // Degenerate cases.
        assert_eq!(percentile(&[7.0], 99.0), 7.0);
        assert!(percentile(&[], 50.0).is_nan());
    }

    #[test]
    fn phase_stats_math() {
        let s = PhaseStats::from_samples(vec![1.0, 2.0, 3.0, 10.0], 0.01);
        assert_eq!(s.ops, 4);
        assert_eq!(s.ops_per_sec, 400.0);
        assert_eq!(s.p50_ms, 2.0); // nearest-rank p50 of 4 samples = 2nd
        assert_eq!(s.p95_ms, 10.0);
        assert_eq!(s.p99_ms, 10.0);
        assert_eq!(s.mean_ms, 4.0);
        assert_eq!(s.max_ms, 10.0);
    }
}
