//! Admission control — the self-protection layer the M3 transport puts in
//! front of the service. Three independent gates, all mapping to the WS-5 §7
//! taxonomy:
//!
//! 1. **Concurrency shedding** — a bounded in-flight permit pool. When the
//!    server is saturated it sheds new work with `rate_limited` (429) rather
//!    than queueing unboundedly and falling over. The in-flight depth is an
//!    observable gauge (WS-7 queue.depth family).
//! 2. **Write-side size caps (F14)** — `max_body_bytes` / `max_attachments`;
//!    an over-cap write is rejected `invalid_envelope` (422) *before* it can
//!    plant an unbounded body that later blows the embedder/summarizer/reads.
//! 3. **Boundary-probe throttle (F15)** — a principal generating repeated
//!    boundary-crossing `not_found`s (the fail-closed id-probing oracle, §3.3)
//!    is throttled to `rate_limited`, denying it an unlimited invisible probe
//!    budget.
//!
//! The admission layer owns none of the retrieval logic; it only decides
//! *whether* a call runs and *whether a write is small enough to accept*.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::wire::{ErrorCode, WireError};

/// Deployment-tunable admission limits. Defaults align with the WS-5 §6.2.4
/// 64 KB read ceiling (a single fact must be readable in one `view:full` call).
#[derive(Debug, Clone, Copy)]
pub struct AdmissionConfig {
    /// Max concurrent in-flight protocol calls across all tenants.
    pub max_in_flight: usize,
    /// Max `fact.body` bytes on a write (F14).
    pub max_body_bytes: usize,
    /// Max `attachments[]` refs on a write (F14).
    pub max_attachments: usize,
    /// Boundary-crossing `not_found`s a principal may emit per window before
    /// it is throttled (F15).
    pub probe_threshold: u32,
    /// The probe window, seconds (F15).
    pub probe_window_secs: i64,
    /// Retry-after hint seconds on a shed/throttle.
    pub retry_after_secs: u64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 256,
            max_body_bytes: 64 * 1024,
            max_attachments: 32,
            probe_threshold: 20,
            probe_window_secs: 60,
            retry_after_secs: 1,
        }
    }
}

/// The admission controller. Cheap to clone-share behind an `Arc`.
pub struct AdmissionControl {
    cfg: AdmissionConfig,
    permits: Arc<Semaphore>,
    /// Per-tenant boundary-`not_found` fixed-window counters (F15).
    probes: Mutex<HashMap<String, ProbeWindow>>,
}

#[derive(Debug, Clone, Copy)]
struct ProbeWindow {
    window_start: i64,
    count: u32,
}

/// Held for the duration of an admitted call; releases the permit on drop so
/// the in-flight depth always reflects live work (RAII, panic-safe).
pub struct AdmitGuard {
    _permit: OwnedSemaphorePermit,
    permits: Arc<Semaphore>,
    max_in_flight: usize,
}

impl AdmitGuard {
    /// The live in-flight depth at the moment this guard was taken — the
    /// WS-7 queue.depth gauge value for the admission pool.
    pub fn in_flight(&self) -> usize {
        self.max_in_flight - self.permits.available_permits()
    }
}

impl AdmissionControl {
    pub fn new(cfg: AdmissionConfig) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(cfg.max_in_flight)),
            probes: Mutex::new(HashMap::new()),
            cfg,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(AdmissionConfig::default())
    }

    pub fn config(&self) -> &AdmissionConfig {
        &self.cfg
    }

    /// Gate 1: try to admit a call. Returns `rate_limited` (429, retriable)
    /// when the in-flight pool is saturated — load shedding, not unbounded
    /// queueing. The returned guard releases the permit on drop.
    pub fn try_admit(&self) -> Result<AdmitGuard, WireError> {
        match self.permits.clone().try_acquire_owned() {
            Ok(permit) => Ok(AdmitGuard {
                _permit: permit,
                permits: self.permits.clone(),
                max_in_flight: self.cfg.max_in_flight,
            }),
            Err(_) => Err(WireError::new(
                ErrorCode::RateLimited,
                "server at capacity; retry with backoff",
            )
            .with_retry_after(self.cfg.retry_after_secs)
            .with_hint("the in-flight request pool is saturated")),
        }
    }

    /// Gate 2 (F14): reject an over-cap write as `invalid_envelope` *before*
    /// it reaches the pipeline. `body_bytes` is the serialized body length;
    /// `attachments` is the ref count.
    pub fn check_write_size(
        &self,
        body_bytes: usize,
        attachments: usize,
    ) -> Result<(), WireError> {
        if body_bytes > self.cfg.max_body_bytes {
            return Err(WireError::new(
                ErrorCode::InvalidEnvelope,
                format!(
                    "fact.body is {body_bytes} bytes, over the {}-byte cap",
                    self.cfg.max_body_bytes
                ),
            )
            .with_hint("split the fact or raise max_body_bytes"));
        }
        if attachments > self.cfg.max_attachments {
            return Err(WireError::new(
                ErrorCode::InvalidEnvelope,
                format!(
                    "{attachments} attachments, over the {}-ref cap",
                    self.cfg.max_attachments
                ),
            ));
        }
        Ok(())
    }

    /// Gate 3 (F15): record a boundary-crossing `not_found` for `tenant` at
    /// `now` (unix seconds). Returns `Err(rate_limited)` once the principal is
    /// over its probe budget for the current window — the caller converts a
    /// would-be `not_found` into a throttle so the fail-closed boundary is not
    /// an unlimited existence oracle.
    pub fn record_boundary_miss(&self, tenant: &str, now: i64) -> Result<(), WireError> {
        let mut probes = self.probes.lock().expect("admission probe lock");
        let w = probes.entry(tenant.to_string()).or_insert(ProbeWindow {
            window_start: now,
            count: 0,
        });
        if now - w.window_start > self.cfg.probe_window_secs {
            w.window_start = now;
            w.count = 0;
        }
        w.count = w.count.saturating_add(1);
        if w.count > self.cfg.probe_threshold {
            return Err(WireError::new(
                ErrorCode::RateLimited,
                "too many boundary-crossing lookups; throttled",
            )
            .with_retry_after(self.cfg.retry_after_secs));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheds_when_pool_saturated() {
        let ac = AdmissionControl::new(AdmissionConfig {
            max_in_flight: 2,
            ..Default::default()
        });
        let g1 = ac.try_admit().unwrap();
        let _g2 = ac.try_admit().unwrap();
        assert_eq!(g1.in_flight(), 2);
        let shed = ac.try_admit().unwrap_err();
        assert_eq!(shed.error_code(), ErrorCode::RateLimited);
        assert!(shed.retriable);
        drop(g1);
        // A permit freed → admits again.
        assert!(ac.try_admit().is_ok());
    }

    #[test]
    fn rejects_oversized_body_and_attachments() {
        let ac = AdmissionControl::new(AdmissionConfig {
            max_body_bytes: 10,
            max_attachments: 1,
            ..Default::default()
        });
        assert_eq!(
            ac.check_write_size(11, 0).unwrap_err().error_code(),
            ErrorCode::InvalidEnvelope
        );
        assert_eq!(
            ac.check_write_size(5, 2).unwrap_err().error_code(),
            ErrorCode::InvalidEnvelope
        );
        assert!(ac.check_write_size(10, 1).is_ok());
    }

    #[test]
    fn throttles_boundary_probing_then_resets_next_window() {
        let ac = AdmissionControl::new(AdmissionConfig {
            probe_threshold: 3,
            probe_window_secs: 60,
            ..Default::default()
        });
        for _ in 0..3 {
            assert!(ac.record_boundary_miss("t", 100).is_ok());
        }
        // 4th in the window is over budget.
        assert_eq!(
            ac.record_boundary_miss("t", 110).unwrap_err().error_code(),
            ErrorCode::RateLimited
        );
        // A different tenant is unaffected (per-principal budget).
        assert!(ac.record_boundary_miss("other", 110).is_ok());
        // Next window resets the first tenant.
        assert!(ac.record_boundary_miss("t", 1000).is_ok());
    }
}
