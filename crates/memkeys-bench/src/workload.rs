//! Deterministic write-path workload for the single-writer ceiling benchmark.
//!
//! The mix mirrors the dominant v1 write path (§8 roundtrip): gateway_flush
//! envelopes carrying a run anchor plus issue/project hints, where a
//! configurable fraction of ops are supersessions of the previous statement on
//! the same (entity, subject, type) grain (§5.1). All content is derived from
//! (seed, op index) — identical seed + op count ⇒ byte-identical workload.

use memkeys_server::envelope::{EntityHints, Envelope, FactBlock, SourceBlock, SourceKind};

use crate::Rng;

/// Workload shape. Defaults reproduce the recorded baseline run.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    pub seed: u64,
    pub ops: u64,
    /// Ops superseding the previous statement (§5.1 grain), 0.0..=1.0.
    pub supersede_ratio: f64,
    /// Distinct entity.issue keys the writes spread over (read fan-in).
    pub key_cardinality: u32,
    /// Approximate fact.body size in bytes (realistic handoff-note size).
    pub body_bytes: usize,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        WorkloadConfig {
            // Pinned seed of the recorded baseline (docs/promotion-thresholds.md):
            // M8 + issue ISI-6082, 2026-10-17.
            seed: 0x6082_2026_1017,
            ops: 1000,
            supersede_ratio: 0.05,
            key_cardinality: 64,
            body_bytes: 256,
        }
    }
}

/// One generated write op. `supersedes` chains onto the previous op's fact.
#[derive(Debug, Clone)]
pub struct WriteOp {
    pub envelope: Envelope,
    /// True when this op supersedes the previous one (needs its fact id).
    pub is_supersede: bool,
    /// The entity.issue value (for the supplementary read phase).
    pub issue_key: String,
}

/// Generate the full op list. `now` (emitted_at axis) advances deterministically
/// from the fixed epoch so the event-time ordering is reproducible too.
pub fn generate(cfg: &WorkloadConfig) -> Vec<WriteOp> {
    let mut rng = Rng::new(cfg.seed);
    let epoch = chrono::DateTime::parse_from_rfc3339("2026-10-17T10:00:00Z")
        .expect("fixed epoch")
        .with_timezone(&chrono::Utc);

    let mut ops = Vec::with_capacity(cfg.ops as usize);
    let mut prev: Option<(String, String)> = None; // (issue, subject) of last op
    for i in 0..cfg.ops {
        let is_supersede =
            prev.is_some() && rng.next_u64() % 1000 < (cfg.supersede_ratio * 1000.0) as u64;

        let (issue, subject): (String, String) = match (&prev, is_supersede) {
            // Supersession grain (§5.1/§6 stage 3): same entity, subject, type.
            (Some((pi, ps)), true) => (pi.clone(), ps.clone()),
            _ => (
                format!("bench-issue-{}", i % cfg.key_cardinality as u64),
                format!("bench-statement-{i}"),
            ),
        };

        let emitted_at = epoch + chrono::Duration::milliseconds(1000 * i as i64 + 7);
        let body = synth_body(&mut rng, i, cfg.body_bytes);

        let envelope = Envelope {
            source: SourceBlock {
                kind: SourceKind::GatewayFlush,
                producer: Some("memkeys-bench".into()),
                emitted_at: emitted_at.to_rfc3339(),
                idempotency_key: None,
            },
            entity: EntityHints {
                project: Some("bench-project".into()),
                issue: Some(issue.clone()),
                run: Some("bench-run".into()),
                ..EntityHints::default()
            },
            fact: FactBlock {
                fact_type: None, // gateway_flush default: context (§3.1)
                body: serde_json::Value::String(body),
                subject: Some(subject.clone()),
                event_time: None,
                confidence: Some(0.9),
                // Filled in by the driver once the previous fact id is known.
                supersedes: None,
            },
            attachments: vec![],
            provenance: None,
        };

        prev = Some((issue.clone(), subject));
        ops.push(WriteOp {
            envelope,
            is_supersede,
            issue_key: issue,
        });
    }
    ops
}

/// Deterministic synthetic body: op index + word soup padded to `target` bytes.
fn synth_body(rng: &mut Rng, op: u64, target: usize) -> String {
    const WORDS: [&str; 16] = [
        "flush",
        "handoff",
        "decision",
        "context",
        "pipeline",
        "fact",
        "anchor",
        "run",
        "issue",
        "key",
        "tenant",
        "supersede",
        "grain",
        "stage",
        "commit",
        "memory",
    ];
    let mut body = format!("bench op {op}: ");
    while body.len() < target {
        let w = WORDS[(rng.next_u64() % WORDS.len() as u64) as usize];
        body.push_str(w);
        body.push(' ');
    }
    body.truncate(target);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_workload() {
        let cfg = WorkloadConfig::default();
        let a = generate(&cfg);
        let b = generate(&cfg);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.issue_key, y.issue_key);
            assert_eq!(x.envelope.fact.subject, y.envelope.fact.subject);
            match (&x.envelope.fact.body, &y.envelope.fact.body) {
                (serde_json::Value::String(s1), serde_json::Value::String(s2)) => {
                    assert_eq!(s1, s2);
                }
                other => panic!("body not a string: {other:?}"),
            }
        }
    }

    #[test]
    fn bodies_are_unique_and_sized() {
        let cfg = WorkloadConfig {
            ops: 200,
            body_bytes: 256,
            ..WorkloadConfig::default()
        };
        let ops = generate(&cfg);
        let bodies: std::collections::HashSet<&str> = ops
            .iter()
            .map(|o| o.envelope.fact.body.as_str().expect("string body"))
            .collect();
        assert_eq!(
            bodies.len(),
            ops.len(),
            "bodies must be unique (id collision)"
        );
        for o in &ops {
            let b = o.envelope.fact.body.as_str().expect("string body");
            assert_eq!(b.len(), 256);
        }
    }

    #[test]
    fn supersede_ops_carry_the_previous_grain() {
        let cfg = WorkloadConfig {
            seed: 1,
            ops: 500,
            ..WorkloadConfig::default()
        };
        let ops = generate(&cfg);
        assert!(
            ops.iter().any(|o| o.is_supersede),
            "seed-1 mix should contain supersessions"
        );
        let mut prev: Option<(&str, &str)> = None;
        for o in &ops {
            if o.is_supersede {
                let (pi, ps) = prev.expect("supersede never first");
                assert_eq!(o.issue_key, pi, "supersede keeps the entity grain");
                assert_eq!(
                    o.envelope.fact.subject.as_deref(),
                    Some(ps),
                    "supersede keeps the subject grain"
                );
            }
            prev = Some((
                o.issue_key.as_str(),
                o.envelope.fact.subject.as_deref().expect("subject set"),
            ));
        }
    }
}
