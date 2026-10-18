//! M7 — the reconciler / backfill (ISI-6081; docs/architecture.md §6 stage 5,
//! D3 bounded-lag invariant).
//!
//! Every derived view in MemKeys is a REBUILDABLE projection of the `facts`
//! source of truth: the graph edges (5a), the fact-vector index (5b), and the
//! LLM-derived digests / category facets / derived keys (5c-5e). Each tier's
//! worker already owns a `catch_up` SoT convergence scan that closes its holes
//! — a lost queue item, a module just-enabled, a stale derivation version. The
//! reconciler is the single component that OWNS running those scans on a
//! cadence across every wired tier and every tenant, so the boot pass and the
//! steady-state maintenance loop drive one cohesive thing instead of a hand-
//! inlined list (which is what M1-M6 left in `main`).
//!
//! It closes two kinds of drift:
//!
//! - **Holes** — a committed fact with no projected artifact (lost queue item,
//!   crash between commit and enqueue, a tier enabled after facts already
//!   landed). Each tier's `catch_up` rebuilds these from the SoT.
//! - **Stale model-stamp rebuilds** — when a model version changes, prior
//!   artifacts are stale and must be regenerated WITHOUT downtime (the derived
//!   row is rebuilt off the hot path; reads keep serving the old row until the
//!   upsert lands — D3: a stale projection costs freshness, never correctness).
//!   The LLM tiers key staleness on `(model_id, prompt_version)`; M7 adds the
//!   embedding-model stamp so the fact-vector tier re-embeds and the digest
//!   tier re-rolls on an embedder change too (ISI-6081, migration 0007).
//!
//! **Idempotency + resume.** Every tier scan is driven off durable state (a
//! projected row's presence, or its `(model_id, prompt_version)`/`model_id`
//! stamp), never off an in-process cursor. So a pass over a converged tenant is
//! a no-op, and a pass interrupted after N of M items resumes at the remaining
//! M-N on the next call — the already-applied items are skipped because their
//! durable stamp now matches. There is no checkpoint to lose.
//!
//! **OTel (WS-7 signal catalog).** Backfill work flows through each tier's
//! existing `memkeys.worker.process` CONSUMER spans (one per applied fact,
//! linked to the originating commit). The reconciler additionally realizes the
//! reserved `memkeys.projection.lag.current` gauge: per tier, the age of the
//! oldest still-outstanding committed fact after a pass (0 when converged) —
//! the async-observed companion to the per-apply `memkeys.projection.lag`
//! histogram, same maintenance-time pattern as `graph.edges.cardinality`.
//!
//! The gauge carries only `memkeys.projection.kind` (signal-catalog.yaml:
//! "Current max outstanding lag per projection"), so it is ONE stream per tier
//! across all tenants. A full `reconcile_all` pass therefore records it once per
//! kind as the MAX outstanding lag over every tenant scanned — NOT once per
//! `(tenant, kind)`, which would make each tenant's `record()` overwrite the
//! previous within a collection cycle and export only the last tenant by
//! `tenant_id` (ISI-6223 B1). A stuck tenant must surface as the tier's lag even
//! when a converged tenant sorts after it.
//!
//! **Failure policy.** Collect-and-continue: a tier that errors is recorded in
//! the report, never `?`-aborts the others — at boot that would wedge the whole
//! maintenance pass on one tier's transient failure. Durably-poisoned items are
//! stamped at the active version by their worker, so they are NOT re-driven
//! each pass and NOT counted as outstanding lag (poison stops reprocessing;
//! ADR-llm Decision 8 / ISI-6192 B2).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::telemetry::{self, Instruments};

/// One derived-view tier the reconciler drives. Each tier's worker implements
/// this by delegating to its existing `catch_up` SoT scan (the backfill) and,
/// where the tier is fact-grained, its `oldest_hole` measurement (the lag
/// gauge). The error is flattened to a `String` so the reconciler can treat the
/// five distinct per-tier error types uniformly in its report.
#[async_trait::async_trait]
pub trait Reconcilable: Send + Sync {
    /// The `memkeys.projection.kind` enum value this tier reports under.
    fn projection_kind(&self) -> &'static str;

    /// Whether this tier's outstanding lag is a meaningful per-fact age. True
    /// for the fact-grained tiers; the digest tier overrides to `false` (it is
    /// scope-grained and debounce-dominated — a single-fact age would
    /// misreport, and the catalog gauge is defined as "oldest un-applied
    /// committed FACT"). A `false` tier is still backfilled; only its
    /// lag-gauge emission is skipped.
    fn measures_lag(&self) -> bool {
        true
    }

    /// Backfill holes + stale-stamp rebuilds for one tenant (the per-tier SoT
    /// convergence scan). Returns the count applied this pass.
    async fn backfill(&self, tenant: &str) -> Result<usize, String>;

    /// Ingest time of the oldest still-outstanding (un-applied or stale)
    /// committed fact for this tenant — the `projection.lag.current` input.
    /// `None` when the tier is converged. Only called when [`measures_lag`] is
    /// true.
    ///
    /// [`measures_lag`]: Reconcilable::measures_lag
    async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
        let _ = tenant;
        Ok(None)
    }
}

/// The result of reconciling one tier for one tenant in a single pass.
#[derive(Debug, Clone)]
pub struct TierReport {
    pub projection_kind: &'static str,
    /// Count applied this pass, or the tier's error (collect-and-continue).
    pub reconciled: Result<usize, String>,
    /// Oldest-outstanding lag in seconds AFTER the pass. `Some(0.0)` = converged;
    /// `Some(>0)` = a transient-stuck or still-missing fact the next pass
    /// retries (the alertable signal). `None` = NOT measured this pass — either
    /// the tier does not measure lag, or its measurement errored. A measurement
    /// error stays `None` (never a fabricated `Some(0.0)`) so the gauge goes
    /// stale rather than falsely-healthy (ISI-6223 M3), and `None` is excluded
    /// from the cross-tenant max so it neither injects a 0 nor masks a real lag.
    pub lag_current_s: Option<f64>,
}

/// One full reconcile pass over a tenant across every wired tier.
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub tenant: String,
    pub tiers: Vec<TierReport>,
}

impl ReconcileReport {
    /// Total items applied across all tiers that succeeded this pass.
    pub fn total_reconciled(&self) -> usize {
        self.tiers
            .iter()
            .filter_map(|t| t.reconciled.as_ref().ok())
            .sum()
    }

    /// Whether any tier errored this pass (the others still ran).
    pub fn had_errors(&self) -> bool {
        self.tiers.iter().any(|t| t.reconciled.is_err())
    }
}

/// The reconciler: the cohesive owner of the per-tier `catch_up` scans. Built
/// from whatever tiers a deployment wired (graph is always present; vector /
/// LLM tiers ride their feature gates), it is cheap to clone (just `Arc`s).
#[derive(Clone)]
pub struct Reconciler {
    tiers: Vec<Arc<dyn Reconcilable>>,
    instruments: Arc<Instruments>,
}

impl std::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("tiers", &self.tiers.len())
            .finish()
    }
}

impl Reconciler {
    /// Build a reconciler over the given tiers (one `memkeys.projection.kind`
    /// each). Order is the pass order; `main` lists them 5a→5e.
    pub fn new(tiers: Vec<Arc<dyn Reconcilable>>) -> Self {
        Self {
            tiers,
            instruments: Arc::new(Instruments::new()),
        }
    }

    /// Reconcile ONE tenant across every tier. Collect-and-continue: a tier
    /// failure is recorded in the report AND logged (ISI-6223 M2 — so the
    /// steady-state maintenance tick, which discards the report, still surfaces
    /// a persistently failing tier; previously only the boot path logged).
    ///
    /// Computes each lag-measuring tier's outstanding lag into the report but
    /// does NOT emit `memkeys.projection.lag.current` here: that gauge is a
    /// single per-kind stream whose value is the MAX across tenants, so it is
    /// emitted once per full pass by [`reconcile_all`] (ISI-6223 B1). A tenant
    /// in isolation cannot know the cross-tenant max.
    pub async fn reconcile_tenant(&self, tenant: &str) -> ReconcileReport {
        let mut tiers = Vec::with_capacity(self.tiers.len());
        for tier in &self.tiers {
            let reconciled = tier.backfill(tenant).await;
            // M2: log the backfill failure here, in the one place both the boot
            // pass and the steady-state tick flow through, so a persistently
            // failing tier is never silent in steady state.
            if let Err(e) = &reconciled {
                tracing::warn!(
                    target: "memkeys", tenant = %tenant,
                    projection = tier.projection_kind(), error = %e,
                    "reconcile tier backfill failed; next maintenance pass retries"
                );
            }
            // Measure AFTER the backfill: a non-zero residual is a genuine
            // stuck/missing fact (transient-failing or not-yet-applied), which
            // is exactly what the lag.current gauge should surface. A durable
            // poison item is stamped at the active version by its worker, so it
            // does not count here (poison stops reprocessing).
            let lag_current_s = if tier.measures_lag() {
                match tier.oldest_hole(tenant).await {
                    Ok(Some(ts)) => Some(lag_seconds(ts)),
                    Ok(None) => Some(0.0), // converged
                    // M3: a measurement error must NOT become a fabricated
                    // Some(0.0) (healthiest) — leave it unmeasured so the gauge
                    // goes stale this pass, not falsely-healthy. Logged so M2's
                    // blind spot doesn't hide it either.
                    Err(e) => {
                        tracing::warn!(
                            target: "memkeys", tenant = %tenant,
                            projection = tier.projection_kind(), error = %e,
                            "reconcile tier lag measurement failed; lag.current left stale this pass"
                        );
                        None
                    }
                }
            } else {
                None
            };
            tiers.push(TierReport {
                projection_kind: tier.projection_kind(),
                reconciled,
                lag_current_s,
            });
        }
        ReconcileReport {
            tenant: tenant.to_string(),
            tiers,
        }
    }

    /// Reconcile every tenant that has committed facts (the boot pass and the
    /// steady-state maintenance tick share this). Returns one report per
    /// tenant. Tenant discovery is bounded by tenant count, not fact count.
    ///
    /// Emits `memkeys.projection.lag.current` ONCE per projection kind after the
    /// full pass, as the max outstanding lag across every tenant (B1).
    pub async fn reconcile_all(&self, pool: &sqlx::PgPool) -> Result<Vec<ReconcileReport>, String> {
        let tenants: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT tenant_id FROM facts ORDER BY tenant_id")
                .fetch_all(pool)
                .await
                .map_err(|e| e.to_string())?;
        let mut reports = Vec::with_capacity(tenants.len());
        for (tenant,) in tenants {
            reports.push(self.reconcile_tenant(&tenant).await);
        }
        for (kind, secs) in max_lag_by_kind(&reports) {
            self.instruments.projection_lag_current(kind, secs);
        }
        Ok(reports)
    }
}

/// The `memkeys.projection.lag.current` value to export per projection kind for
/// a full reconcile pass: the MAX outstanding lag over every tenant scanned
/// (B1 — the gauge is per-kind, not per-(tenant, kind)). Order-independent, so
/// a lagging tenant surfaces whether it sorts before or after a converged one.
///
/// A tier whose lag was NOT measured this pass (`None` — unmeasurable tier, or a
/// measurement error) contributes nothing: it is excluded from the max, so it
/// neither injects a false 0.0 nor masks a real lag from another tenant (M3). A
/// kind with no measured observation at all is absent from the result and so is
/// not recorded — the gauge goes stale rather than reporting a fabricated 0.0.
fn max_lag_by_kind(reports: &[ReconcileReport]) -> Vec<(&'static str, f64)> {
    let mut max: std::collections::BTreeMap<&'static str, f64> = std::collections::BTreeMap::new();
    for report in reports {
        for tier in &report.tiers {
            if let Some(secs) = tier.lag_current_s {
                max.entry(tier.projection_kind)
                    .and_modify(|m| *m = m.max(secs))
                    .or_insert(secs);
            }
        }
    }
    max.into_iter().collect()
}

/// Seconds between a committed fact's ingest and now (never negative — a clock
/// skew that puts ingest in the future reads as 0 lag, not a negative one).
fn lag_seconds(ingested_at: DateTime<Utc>) -> f64 {
    (Utc::now() - ingested_at).num_milliseconds().max(0) as f64 / 1000.0
}

// ===========================================================================
// Per-tier Reconcilable impls — each delegates to the worker's existing scan.
// ===========================================================================

#[async_trait::async_trait]
impl Reconcilable for crate::worker::GraphProjectionWorker {
    fn projection_kind(&self) -> &'static str {
        telemetry::PROJECTION_KIND_GRAPH
    }
    async fn backfill(&self, tenant: &str) -> Result<usize, String> {
        self.catch_up(tenant).await.map_err(|e| e.to_string())
    }
    async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
        crate::worker::GraphProjectionWorker::oldest_hole(self, tenant)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Reconcilable for crate::worker::VectorProjectionWorker {
    fn projection_kind(&self) -> &'static str {
        telemetry::PROJECTION_KIND_VECTOR
    }
    async fn backfill(&self, tenant: &str) -> Result<usize, String> {
        self.catch_up(tenant).await.map_err(|e| e.to_string())
    }
    async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
        crate::worker::VectorProjectionWorker::oldest_hole(self, tenant)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Reconcilable for crate::llm_worker::ExtractWorker {
    fn projection_kind(&self) -> &'static str {
        telemetry::PROJECTION_KIND_DERIVED_KEYS
    }
    async fn backfill(&self, tenant: &str) -> Result<usize, String> {
        self.catch_up(tenant).await.map_err(|e| e.to_string())
    }
    async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
        crate::llm_worker::ExtractWorker::oldest_hole(self, tenant)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Reconcilable for crate::llm_worker::CategorizeWorker {
    fn projection_kind(&self) -> &'static str {
        telemetry::PROJECTION_KIND_CATEGORY_FACETS
    }
    async fn backfill(&self, tenant: &str) -> Result<usize, String> {
        self.catch_up(tenant).await.map_err(|e| e.to_string())
    }
    async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
        crate::llm_worker::CategorizeWorker::oldest_hole(self, tenant)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Reconcilable for crate::llm_worker::DigestWorker {
    fn projection_kind(&self) -> &'static str {
        telemetry::PROJECTION_KIND_DIGEST
    }
    // The digest tier is scope-grained and debounce-dominated (the 30s tick IS
    // its coalescing window), so a single-fact lag.current would misreport; it
    // is still backfilled, only the fact-grained lag gauge is skipped.
    // ponytail: a per-scope staleness age could be surfaced later; today the
    // digest's freshness is already carried by projection.lag at rollup time.
    fn measures_lag(&self) -> bool {
        false
    }
    async fn backfill(&self, tenant: &str) -> Result<usize, String> {
        self.catch_up(tenant).await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A no-DB spy tier: scriptable count/error/oldest-hole, recording how many
    /// times it was backfilled (to prove collect-and-continue runs every tier).
    struct SpyTier {
        kind: &'static str,
        result: Result<usize, String>,
        oldest: Option<DateTime<Utc>>,
        oldest_err: bool,
        calls: AtomicUsize,
    }

    impl SpyTier {
        fn ok(kind: &'static str, n: usize) -> Self {
            Self {
                kind,
                result: Ok(n),
                oldest: None,
                oldest_err: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn err(kind: &'static str) -> Self {
            Self {
                kind,
                result: Err("boom".into()),
                oldest: None,
                oldest_err: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn with_oldest(mut self, ts: DateTime<Utc>) -> Self {
            self.oldest = Some(ts);
            self
        }
        fn with_oldest_err(mut self) -> Self {
            self.oldest_err = true;
            self
        }
    }

    #[async_trait::async_trait]
    impl Reconcilable for SpyTier {
        fn projection_kind(&self) -> &'static str {
            self.kind
        }
        async fn backfill(&self, _tenant: &str) -> Result<usize, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
        async fn oldest_hole(&self, _tenant: &str) -> Result<Option<DateTime<Utc>>, String> {
            if self.oldest_err {
                return Err("oldest_hole boom".into());
            }
            Ok(self.oldest)
        }
    }

    /// Build a one-tier report directly, for the pure `max_lag_by_kind` tests.
    fn report(tenant: &str, kind: &'static str, lag: Option<f64>) -> ReconcileReport {
        ReconcileReport {
            tenant: tenant.to_string(),
            tiers: vec![TierReport {
                projection_kind: kind,
                reconciled: Ok(0),
                lag_current_s: lag,
            }],
        }
    }

    #[tokio::test]
    async fn collect_and_continue_runs_every_tier_despite_a_failure() {
        let a = Arc::new(SpyTier::ok("graph", 3));
        let b = Arc::new(SpyTier::err("vector"));
        let c = Arc::new(SpyTier::ok("derived_keys", 2));
        let r = Reconciler::new(vec![a.clone(), b.clone(), c.clone()]);

        let report = r.reconcile_tenant("t1").await;

        // Every tier ran exactly once even though the middle one errored.
        assert_eq!(a.calls.load(Ordering::SeqCst), 1);
        assert_eq!(b.calls.load(Ordering::SeqCst), 1);
        assert_eq!(c.calls.load(Ordering::SeqCst), 1);
        // The report aggregates: 3 + 2 applied, one tier flagged as errored.
        assert_eq!(report.total_reconciled(), 5);
        assert!(report.had_errors());
        assert_eq!(report.tiers.len(), 3);
    }

    #[tokio::test]
    async fn converged_tier_reports_zero_lag_outstanding_tier_reports_positive() {
        let converged = Arc::new(SpyTier::ok("graph", 0));
        let stale = Utc::now() - chrono::Duration::seconds(42);
        let behind = Arc::new(SpyTier::ok("vector", 0).with_oldest(stale));
        let r = Reconciler::new(vec![converged, behind]);

        let report = r.reconcile_tenant("t1").await;

        assert_eq!(
            report.tiers[0].lag_current_s,
            Some(0.0),
            "converged → 0 lag"
        );
        assert!(
            report.tiers[1].lag_current_s.unwrap() >= 42.0,
            "outstanding hole surfaces as positive lag.current, got {:?}",
            report.tiers[1].lag_current_s
        );
    }

    #[tokio::test]
    async fn lag_measurement_error_is_unmeasured_not_fabricated_zero() {
        // M3: oldest_hole erroring must leave lag unmeasured (None), NOT a
        // falsely-healthy Some(0.0).
        let tier = Arc::new(SpyTier::ok("vector", 0).with_oldest_err());
        let r = Reconciler::new(vec![tier]);

        let report = r.reconcile_tenant("t1").await;

        assert_eq!(report.tiers[0].lag_current_s, None);
    }

    #[test]
    fn lag_gauge_is_max_across_tenants_not_last_scanned() {
        // B1 regression: the LAGGING tenant sorts FIRST (by tenant_id), the
        // CONVERGED tenant sorts LAST. Under the old last-tenant-wins record()
        // the converged tenant's 0.0 would overwrite the real 42s and export 0,
        // silencing the D3 alert. max-across-tenants surfaces the 42s.
        let reports = vec![
            report("a-lagging", "vector", Some(42.0)),
            report("z-converged", "vector", Some(0.0)),
        ];
        assert_eq!(max_lag_by_kind(&reports), vec![("vector", 42.0)]);
    }

    #[test]
    fn lag_error_neither_fabricates_zero_nor_masks_a_real_lag() {
        // M3 + B1: tenant t1's measurement errored (None), tenant t2 is 10s
        // behind on the same kind. The None contributes nothing, so the real
        // 10s still surfaces as the exported max.
        let reports = vec![
            report("t1", "vector", None),
            report("t2", "vector", Some(10.0)),
        ];
        assert_eq!(max_lag_by_kind(&reports), vec![("vector", 10.0)]);
    }

    #[test]
    fn kind_with_only_unmeasured_observations_is_absent_so_gauge_goes_stale() {
        // M3: every observation for the kind is None (unmeasured / errored) ⇒
        // the kind is absent from the result ⇒ no record() ⇒ the gauge holds
        // its prior value (stale) instead of reporting a fabricated 0.0.
        let reports = vec![report("t1", "vector", None), report("t2", "vector", None)];
        assert!(max_lag_by_kind(&reports).is_empty());
    }
}
