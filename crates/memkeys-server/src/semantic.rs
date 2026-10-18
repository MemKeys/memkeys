//! M5 — the semantic surface: the fact-vector tier (§5.4.2), op-3 semantic
//! search (§7.4), and Reciprocal Rank Fusion of the keyed / traverse / semantic
//! retrieval paths.
//!
//! This module is the **backend-portable** core (§7 "Retrieval contract,
//! backend-portable"): everything here is pure Rust with no Postgres or
//! embedding-endpoint dependency, so it unit-tests green in CI with no
//! services. The two ports — [`Embedder`] and [`VectorIndex`] — are the seams
//! the deployment-specific adapters implement:
//!
//! - [`Embedder`] abstracts the embedding-model endpoint, which §2 pushes to
//!   the edge (it "varies most across deployments"). [`HashEmbedder`] is the
//!   deterministic, dependency-free default that makes a minimal AgentMesh
//!   deployment (and CI) run semantic search with no model server. A real
//!   HTTP/ONNX embedder is a drop-in follow-up adapter.
//! - [`VectorIndex`] abstracts the vector store. [`MemVectorIndex`] is the
//!   in-process impl; the pgvector adapter (`PgVectorIndex`) implements the
//!   same trait against `agentmesh_fact_vectors`.
//!
//! **Multi-tenancy boundary (§5.4.2 / §7.4 R6).** The ANN pass is *always*
//! bounded: `tenant` is mandatory on every [`VectorIndex::search`] call and a
//! tenant's vectors are physically partitioned, so team A's facts can never
//! surface in team B's results. A finer [`CandidateSet`] narrows *within* that
//! tenant floor (the scope pre-filter); [`CandidateSet::All`] means "everything
//! this tenant owns" — including `unkeyed` facts — which closes the R6 recall
//! hole without widening isolation.
//!
//! **R1 resolve-to-ground-truth.** Vectors carry **keys and the embedding
//! only, never body text** (§5.4.2). A semantic hit is a `fact.id` + score; the
//! authoritative body is always hydrated from the FactStore afterward (§7.5).

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::model::EntityKeys;

/// Dimensionality of [`HashEmbedder`] vectors. A fixed, modest width — the
/// deterministic embedder is a lexical signal, not a learned model; 256 is
/// enough to separate token bags without being wasteful.
pub const HASH_EMBED_DIM: usize = 256;

/// The [`HashEmbedder`]'s `model_id` — the embedding half of a fact vector's
/// derivation version (M7). Stamped onto every `fact_vectors` row it produces
/// so the reconciler can detect a stale embedding and re-embed when the active
/// embedder changes (ISI-6081). The algorithm + width are baked into the id: a
/// change to either MUST bump this string so prior vectors backfill.
pub const HASH_EMBED_MODEL_ID: &str = "hash-fnv1a-256/v1";

/// The RRF constant `k` (§7.4 step 3): `score = Σ 1/(k_rrf + rank)`. 60 is the
/// standard value from the original RRF paper and the ratified default.
pub const K_RRF: f64 = 60.0;

/// Failure of the embedding port.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedder unavailable: {0}")]
    Unavailable(String),
}

/// Failure of the vector-index port.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("vector index backend error: {0}")]
    Backend(String),
    #[error("embedding dimension mismatch: index has {expected}, got {got}")]
    DimMismatch { expected: usize, got: usize },
}

/// The embedding-model port (§5.4.2 fact tier). Maps text → a dense vector.
/// Implementations must be deterministic for a given input within a
/// `derivation_version` so that re-embedding on replay is idempotent.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// The fixed output dimensionality (must match the configured index).
    fn dim(&self) -> usize;
    /// The embedder build id — the embedding half of a fact vector's derivation
    /// version (§5.5; M7). Stamped onto every embedded row (`fact_vectors`,
    /// `digests`) so the reconciler re-embeds when the active embedder changes
    /// (ISI-6081): a stored `model_id` that differs from the active one marks a
    /// stale embedding for backfill, exactly as `(model_id, prompt_version)`
    /// does for the LLM-derived tiers. A deterministic embedder's id captures
    /// its algorithm + width; a model-backed one, the model name/revision.
    fn model_id(&self) -> &str;
    /// Embed one text. Called on stage 5b (fact body) and per query (§7.4).
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

/// Deterministic, dependency-free embedder for minimal deployments and CI.
///
/// Signed feature hashing: each lexical token is hashed to a dimension index
/// and a sign, accumulated, then L2-normalized so cosine similarity is a plain
/// dot product. This captures lexical overlap — enough for the fact tier to be
/// *runnable* everywhere — without pulling in a model runtime. It is explicitly
/// **not** a semantic model: a production deployment swaps in a real embedder
/// behind the same [`Embedder`] trait. Deterministic ⇒ stage-5b re-embeds and
/// query embeds are reproducible across restarts.
#[derive(Debug, Clone, Default)]
pub struct HashEmbedder;

impl HashEmbedder {
    pub fn new() -> Self {
        Self
    }
}

/// FNV-1a 64-bit — a small, stable, allocation-free hash (we need
/// reproducibility across runs, which `DefaultHasher` does not guarantee).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Split on any non-alphanumeric boundary, lowercase, drop empties.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

#[async_trait::async_trait]
impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        HASH_EMBED_DIM
    }

    fn model_id(&self) -> &str {
        HASH_EMBED_MODEL_ID
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = vec![0f32; HASH_EMBED_DIM];
        for tok in tokenize(text) {
            let h = fnv1a(tok.as_bytes());
            let idx = (h % HASH_EMBED_DIM as u64) as usize;
            // Independent sign bit so distinct tokens hashing to the same bucket
            // don't always reinforce (reduces collision bias).
            let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        l2_normalize(&mut v);
        Ok(v)
    }
}

/// L2-normalize in place; a zero vector (no tokens) is left as zeros — its
/// cosine with anything is 0, which is the correct "no signal" outcome.
fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two vectors. For L2-normalized inputs this is the dot
/// product; we divide by norms anyway so it is correct for un-normalized input.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// One row of the fact-vector tier (§5.4.2): **keys and vector only, never body
/// text**. `entity` is carried so a future in-index scope pre-filter can run
/// without a FactStore round-trip, but the hard boundary is still `tenant_id`.
#[derive(Debug, Clone)]
pub struct FactVector {
    pub fact_id: Uuid,
    pub tenant_id: String,
    pub embedding: Vec<f32>,
    pub fact_type: String,
    pub entity: EntityKeys,
    /// Transaction axis (= `facts.ingested_at`): the fact's "known by" time.
    /// The unscoped (`CandidateSet::All`) ANN pass filters `ingested_at <=
    /// as_of` so a whole-tenant `as_of` search is temporally bounded — not a
    /// silent read over newer-than-as_of facts (§7.4; ISI-6172).
    pub ingested_at: DateTime<Utc>,
    /// The embedder build that produced `embedding` ([`Embedder::model_id`];
    /// M7). Persisted on the row so the reconciler re-embeds this fact when the
    /// active embedder's id differs — the stage-5b stale-embedding backfill
    /// trigger (ISI-6081), mirroring the LLM tiers' `(model_id, prompt_version)`.
    pub model_id: String,
}

/// A similarity hit from the index: an id and its score (cosine, in `[-1, 1]`).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredId {
    pub fact_id: Uuid,
    pub score: f32,
}

/// The scope pre-filter applied *before* the ANN pass (§7.4 step 1). This is
/// the translation of the caller's `entity.*`/`category` attributes to a
/// candidate id set; the server computes it (keyed DQL → ids, or a graph hop)
/// and hands it here. [`CandidateSet::All`] = no finer key ⇒ the whole tenant
/// floor (R6: `unkeyed` facts become reachable without widening isolation).
#[derive(Debug, Clone)]
pub enum CandidateSet {
    /// Everything the tenant owns (tenant floor is still enforced by `search`).
    All,
    /// Only these ids (the scope narrowed within the tenant floor).
    Only(Vec<Uuid>),
}

/// The vector-store port. The pgvector adapter and [`MemVectorIndex`] implement
/// this identically; the service and stage-5b worker depend only on the trait.
#[async_trait::async_trait]
pub trait VectorIndex: Send + Sync {
    /// Stage 5b — upsert one fact vector. **Idempotent on `fact.id`**: a replay
    /// overwrites with the same embedding and is not a duplicate (§5.4.2).
    async fn upsert(&self, v: &FactVector) -> Result<(), VectorError>;

    /// ANN search for the top-`k` most similar facts to `query`, bounded to
    /// `tenant` (the hard multi-tenancy boundary) and restricted to
    /// `candidates` (the scope pre-filter). Results are score-descending.
    ///
    /// `as_of` is the §7.4 point-in-time bound ("facts known by then"): when
    /// `Some`, only facts with `ingested_at <= as_of` are eligible. It MUST be
    /// honored on the `CandidateSet::All` (whole-tenant) path too — a keyed
    /// scope already bounds its candidate ids by `as_of`, but the unscoped path
    /// has no such pre-filter, so without this an unscoped `as_of` search would
    /// silently rank over facts newer than `as_of` (ISI-6172 / CR ISI-6168 B1).
    async fn search(
        &self,
        tenant: &str,
        candidates: &CandidateSet,
        query: &[f32],
        k: usize,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<ScoredId>, VectorError>;

    /// Drop every vector for a tenant/fact (retention sweep / disable, §5.4.2).
    async fn delete(&self, tenant: &str, fact_id: Uuid) -> Result<(), VectorError>;
}

/// In-process [`VectorIndex`] — the minimal-deployment and test backend.
/// Vectors are physically partitioned by tenant (`tenant → id → vector`), so
/// isolation is structural, not a query predicate that could be forgotten.
#[derive(Debug, Default)]
pub struct MemVectorIndex {
    inner: Mutex<HashMap<String, HashMap<Uuid, FactVector>>>,
}

impl MemVectorIndex {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Count of vectors held for a tenant (tests / `graph.edges.cardinality`
    /// analogue for the vector tier).
    pub fn len(&self, tenant: &str) -> usize {
        self.inner
            .lock()
            .expect("vector index lock")
            .get(tenant)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self, tenant: &str) -> bool {
        self.len(tenant) == 0
    }
}

#[async_trait::async_trait]
impl VectorIndex for MemVectorIndex {
    async fn upsert(&self, v: &FactVector) -> Result<(), VectorError> {
        let mut g = self.inner.lock().expect("vector index lock");
        g.entry(v.tenant_id.clone())
            .or_default()
            .insert(v.fact_id, v.clone()); // idempotent on fact_id
        Ok(())
    }

    async fn search(
        &self,
        tenant: &str,
        candidates: &CandidateSet,
        query: &[f32],
        k: usize,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<ScoredId>, VectorError> {
        let g = self.inner.lock().expect("vector index lock");
        let Some(by_id) = g.get(tenant) else {
            return Ok(Vec::new()); // tenant has no vectors
        };
        let mut scored: Vec<ScoredId> = by_id
            .values()
            .filter(|fv| match candidates {
                CandidateSet::All => true,
                CandidateSet::Only(ids) => ids.contains(&fv.fact_id),
            })
            // §7.4 point-in-time bound: drop facts not yet known at `as_of`.
            // Applies on every candidate path, All included (ISI-6172).
            .filter(|fv| as_of.is_none_or(|t| fv.ingested_at <= t))
            .map(|fv| ScoredId {
                fact_id: fv.fact_id,
                score: cosine(query, &fv.embedding),
            })
            .collect();
        // Score desc; tiebreak on id so ordering is deterministic.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.fact_id.cmp(&b.fact_id))
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn delete(&self, tenant: &str, fact_id: Uuid) -> Result<(), VectorError> {
        if let Some(by_id) = self
            .inner
            .lock()
            .expect("vector index lock")
            .get_mut(tenant)
        {
            by_id.remove(&fact_id);
        }
        Ok(())
    }
}

/// Which retrieval path produced a ranked list feeding RRF. Doubles as the
/// `via` provenance of a fused hit (§7.4 response: "via/digest_scope show how
/// the hit surfaced").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FusionSource {
    /// Op 1 — attribute-keyed fetch (§7.2).
    Keyed,
    /// Op 2/3 — graph traversal (§7.3).
    Graph,
    /// Op 4 — semantic, fact tier (§5.4.2 precision).
    SemanticFact,
    /// Op 4 — semantic, digest tier (§5.4.2 context; resolves to member facts).
    SemanticDigest,
}

impl FusionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FusionSource::Keyed => "keyed",
            FusionSource::Graph => "graph",
            FusionSource::SemanticFact => "fact",
            FusionSource::SemanticDigest => "digest",
        }
    }
}

/// One ranked input list to [`rrf_fuse`]: fact ids in rank order (best first),
/// labelled by the path that produced them and weighted (deployment knob;
/// default 1.0 — the tiers/paths are complementary, so RRF, not a hand-tuned
/// prior, decides, §7.4 open-item-4 ruling).
#[derive(Debug, Clone)]
pub struct RankedList {
    pub source: FusionSource,
    pub weight: f64,
    pub ids: Vec<Uuid>,
}

impl RankedList {
    pub fn new(source: FusionSource, ids: Vec<Uuid>) -> Self {
        Self {
            source,
            weight: 1.0,
            ids,
        }
    }

    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }
}

/// A fused result: the fact id, its summed RRF score, and the set of sources
/// that surfaced it (provenance for the `via` field, dedup across paths).
#[derive(Debug, Clone, PartialEq)]
pub struct FusedHit {
    pub fact_id: Uuid,
    pub score: f64,
    pub sources: Vec<FusionSource>,
}

/// Reciprocal Rank Fusion (§7.4 step 3). For each list, a fact at 0-based
/// position `i` contributes `weight / (K_RRF + (i + 1))` to its total; scores
/// sum across lists, so a fact that several paths agree on ranks highest —
/// which is exactly the "keyed + traverse + semantic agree" signal. Ids are
/// de-duplicated across lists (§7.4 step 4). Output is score-descending, with a
/// deterministic id tiebreak.
pub fn rrf_fuse(lists: &[RankedList]) -> Vec<FusedHit> {
    let mut scores: HashMap<Uuid, f64> = HashMap::new();
    // Preserve a stable source order per id (first-seen wins for display).
    let mut sources: HashMap<Uuid, Vec<FusionSource>> = HashMap::new();

    for list in lists {
        for (i, id) in list.ids.iter().enumerate() {
            let rank = (i + 1) as f64;
            let contribution = list.weight / (K_RRF + rank);
            *scores.entry(*id).or_insert(0.0) += contribution;
            let srcs = sources.entry(*id).or_default();
            if !srcs.contains(&list.source) {
                srcs.push(list.source);
            }
        }
    }

    let mut fused: Vec<FusedHit> = scores
        .into_iter()
        .map(|(fact_id, score)| FusedHit {
            fact_id,
            score,
            sources: sources.remove(&fact_id).unwrap_or_default(),
        })
        .collect();
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.fact_id.cmp(&b.fact_id))
    });
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[tokio::test]
    async fn hash_embedder_is_deterministic() {
        let e = HashEmbedder::new();
        let a = e.embed("chose postgres for the fact store").await.unwrap();
        let b = e.embed("chose postgres for the fact store").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), HASH_EMBED_DIM);
    }

    #[tokio::test]
    async fn hash_embedder_ranks_related_text_higher() {
        let e = HashEmbedder::new();
        let q = e.embed("postgres database choice").await.unwrap();
        let near = e
            .embed("we chose the postgres database for durability")
            .await
            .unwrap();
        let far = e
            .embed("the frontend uses a react component library")
            .await
            .unwrap();
        assert!(
            cosine(&q, &near) > cosine(&q, &far),
            "related text must be more similar: near={} far={}",
            cosine(&q, &near),
            cosine(&q, &far)
        );
    }

    #[tokio::test]
    async fn empty_text_embeds_to_zero_and_scores_zero() {
        let e = HashEmbedder::new();
        let z = e.embed("   ").await.unwrap();
        let q = e.embed("anything").await.unwrap();
        assert_eq!(cosine(&q, &z), 0.0);
    }

    /// A fixed "long ago" ingest time so the as_of-agnostic tests (which pass
    /// `None`) are unaffected; the temporal test uses [`vec_of_at`].
    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(1_600_000_000, 0).unwrap()
    }

    async fn vec_of(e: &HashEmbedder, tenant: &str, n: u128, text: &str) -> FactVector {
        vec_of_at(e, tenant, n, text, epoch()).await
    }

    async fn vec_of_at(
        e: &HashEmbedder,
        tenant: &str,
        n: u128,
        text: &str,
        at: DateTime<Utc>,
    ) -> FactVector {
        FactVector {
            fact_id: id(n),
            tenant_id: tenant.into(),
            embedding: e.embed(text).await.unwrap(),
            fact_type: "decision".into(),
            entity: EntityKeys::default(),
            ingested_at: at,
            model_id: e.model_id().to_string(),
        }
    }

    #[tokio::test]
    async fn index_enforces_tenant_isolation() {
        let e = HashEmbedder::new();
        let idx = MemVectorIndex::new();
        idx.upsert(&vec_of(&e, "team-a", 1, "postgres decision").await)
            .await
            .unwrap();
        idx.upsert(&vec_of(&e, "team-b", 2, "postgres decision").await)
            .await
            .unwrap();

        let q = e.embed("postgres decision").await.unwrap();
        // team-a's search must NEVER see team-b's fact (§5.4.2 hard boundary).
        let hits = idx
            .search("team-a", &CandidateSet::All, &q, 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fact_id, id(1));
    }

    #[tokio::test]
    async fn candidate_set_narrows_within_tenant() {
        let e = HashEmbedder::new();
        let idx = MemVectorIndex::new();
        idx.upsert(&vec_of(&e, "t", 1, "postgres decision").await)
            .await
            .unwrap();
        idx.upsert(&vec_of(&e, "t", 2, "postgres decision again").await)
            .await
            .unwrap();

        let q = e.embed("postgres decision").await.unwrap();
        let hits = idx
            .search("t", &CandidateSet::Only(vec![id(2)]), &q, 10, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fact_id, id(2));
    }

    #[tokio::test]
    async fn as_of_bounds_unscoped_candidate_set() {
        // Regression for ISI-6172 (CR ISI-6168 B1): on the UNSCOPED
        // (CandidateSet::All, whole-tenant) path, `as_of` must still drop facts
        // ingested after it — not silently rank over newer-than-as_of vectors.
        let e = HashEmbedder::new();
        let idx = MemVectorIndex::new();
        let old = DateTime::from_timestamp(1_000, 0).unwrap();
        let new = DateTime::from_timestamp(2_000, 0).unwrap();
        idx.upsert(&vec_of_at(&e, "t", 1, "postgres decision", old).await)
            .await
            .unwrap();
        idx.upsert(&vec_of_at(&e, "t", 2, "postgres decision", new).await)
            .await
            .unwrap();

        let q = e.embed("postgres decision").await.unwrap();
        // as_of between the two ingests: only the older fact is "known by then".
        let cutoff = DateTime::from_timestamp(1_500, 0).unwrap();
        let hits = idx
            .search("t", &CandidateSet::All, &q, 10, Some(cutoff))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "as_of must bound the whole-tenant path");
        assert_eq!(hits[0].fact_id, id(1));

        // No as_of ⇒ both facts eligible (the live-head read).
        let all = idx
            .search("t", &CandidateSet::All, &q, 10, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_fact_id() {
        let e = HashEmbedder::new();
        let idx = MemVectorIndex::new();
        idx.upsert(&vec_of(&e, "t", 1, "v1").await).await.unwrap();
        idx.upsert(&vec_of(&e, "t", 1, "v1").await).await.unwrap();
        assert_eq!(idx.len("t"), 1);
    }

    #[test]
    fn rrf_rewards_cross_path_agreement() {
        // id(1) is mid-rank in two paths; id(2) is top of only one. The fact
        // both keyed and semantic agree on should win.
        let keyed = RankedList::new(FusionSource::Keyed, vec![id(2), id(1), id(3)]);
        let semantic = RankedList::new(FusionSource::SemanticFact, vec![id(1), id(4)]);
        let fused = rrf_fuse(&[keyed, semantic]);
        assert_eq!(fused[0].fact_id, id(1), "cross-path agreement ranks first");
        // id(1) surfaced via both paths — provenance is preserved & de-duped.
        assert_eq!(fused[0].sources.len(), 2);
    }

    #[test]
    fn rrf_dedups_ids_across_lists() {
        let a = RankedList::new(FusionSource::Keyed, vec![id(1), id(2)]);
        let b = RankedList::new(FusionSource::Graph, vec![id(2), id(1)]);
        let fused = rrf_fuse(&[a, b]);
        assert_eq!(fused.len(), 2, "each id appears once after fusion");
    }

    #[test]
    fn rrf_weight_is_a_knob() {
        // Equal default: single-list top beats single-list second elsewhere.
        let base_a = RankedList::new(FusionSource::SemanticFact, vec![id(1)]);
        let base_b = RankedList::new(FusionSource::SemanticDigest, vec![id(2)]);
        let fused = rrf_fuse(&[base_a, base_b]);
        assert_eq!(fused[0].fact_id, id(1), "id tiebreak when scores equal");

        // Up-weight digest: id(2) must overtake id(1).
        let a = RankedList::new(FusionSource::SemanticFact, vec![id(1)]);
        let b = RankedList::new(FusionSource::SemanticDigest, vec![id(2)]).with_weight(5.0);
        let fused = rrf_fuse(&[a, b]);
        assert_eq!(fused[0].fact_id, id(2), "weight raises the digest tier");
    }

    #[test]
    fn rrf_fuses_all_three_paths() {
        let keyed = RankedList::new(FusionSource::Keyed, vec![id(1)]);
        let graph = RankedList::new(FusionSource::Graph, vec![id(1), id(2)]);
        let semantic = RankedList::new(FusionSource::SemanticFact, vec![id(1), id(3)]);
        let fused = rrf_fuse(&[keyed, graph, semantic]);
        assert_eq!(fused[0].fact_id, id(1));
        assert_eq!(fused[0].sources.len(), 3, "keyed + graph + semantic");
        assert_eq!(fused.len(), 3);
    }
}
