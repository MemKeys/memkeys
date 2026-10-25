//! Embedder adapters + the deployment factory (§5.4.2, §2 "the embedding model
//! varies most across deployments, so it is pushed to the edge").
//!
//! Two concrete embedders sit behind the backend-portable [`Embedder`] port:
//! - [`HashEmbedder`](crate::semantic::HashEmbedder) — the deterministic,
//!   dependency-free default (semantic.rs); lets a minimal deployment and CI
//!   run semantic search with no model server.
//! - [`HttpEmbedder`] — calls a real OpenAI-compatible embedding endpoint.
//!
//! [`from_env`] is the single wiring point: `MEMKEYS_EMBED_URL` set ⇒ the HTTP
//! embedder; unset ⇒ the hash embedder fallback, so a minimal deployment still
//! runs. The embedder's output width is taken from `MEMKEYS_EMBED_DIM` (default
//! [`HASH_EMBED_DIM`] = 256); the fact-vector column is reconciled to that width
//! at startup (see [`crate::pg_vector::ensure_vector_column_dim`]), so wiring a
//! wider external embedder (e.g. nomic-embed-text, 768) is a two-env-var change
//! with no bespoke migration (ISI-6671 — benchmark apples-to-apples).

use std::sync::Arc;

use crate::semantic::{EmbedError, Embedder, HashEmbedder, HASH_EMBED_DIM};

/// Environment variable naming the embedding endpoint. Unset ⇒ [`HashEmbedder`].
pub const EMBED_URL_ENV: &str = "MEMKEYS_EMBED_URL";

/// Environment variable naming the HTTP embedder's model id (M7: the embedding
/// half of the derivation version, stamped on `fact_vectors`/`digests`). An
/// operator MUST bump this when they change the embedding model behind the same
/// `MEMKEYS_EMBED_URL` so the reconciler re-embeds prior vectors. Unset ⇒ a
/// stable id derived from the endpoint url (so a bare swap of the URL already
/// counts as a new model and backfills). When set, it is ALSO sent as the
/// OpenAI `/embeddings` request `model` field (Ollama and other multi-model
/// servers require it; single-model servers ignore it).
pub const EMBED_MODEL_ENV: &str = "MEMKEYS_EMBED_MODEL";

/// Environment variable naming the embedder's output dimension (default
/// [`HASH_EMBED_DIM`] = 256). Must match the model the endpoint serves
/// (nomic-embed-text = 768); the fact-vector column is reconciled to this width
/// at startup. Out of range or non-numeric ⇒ a warning and the 256 default.
pub const EMBED_DIM_ENV: &str = "MEMKEYS_EMBED_DIM";

/// pgvector builds an HNSW index on the embedding column, which caps a vector at
/// 2000 dimensions — the ceiling we accept a configured [`EMBED_DIM_ENV`] within.
pub const MAX_EMBED_DIM: usize = 2000;

/// An OpenAI-compatible HTTP embedding-model adapter.
///
/// Wire contract — the de-facto standard spoken by Ollama, vLLM, and
/// text-embeddings-inference: `POST <url>` with `{"model": "<model>", "input":
/// "<text>"}` → `{"data": [{"embedding": [<f32>, …]}]}`. The legacy minimal
/// `{"embedding": […]}` top-level shape is still accepted on the response for
/// back-compat. The returned vector must have exactly `dim` components.
pub struct HttpEmbedder {
    url: String,
    dim: usize,
    model_id: String,
    /// The `model` sent in the OpenAI request; `None` ⇒ the field is omitted
    /// (single-model servers that reject an unknown/empty model still work).
    request_model: Option<String>,
    client: reqwest::Client,
}

impl HttpEmbedder {
    /// Build an HTTP embedder for `url` producing `dim`-wide vectors with the
    /// embedder build id `model_id` (M7: the embedding half of a fact vector's
    /// derivation version — a change MUST be reflected here so prior vectors
    /// backfill). `request_model` is the OpenAI `/embeddings` `model` field
    /// (`None` ⇒ omitted). `dim` must equal the reconciled index width.
    pub fn new(
        url: impl Into<String>,
        dim: usize,
        model_id: impl Into<String>,
        request_model: Option<String>,
    ) -> Self {
        Self {
            url: url.into(),
            dim,
            model_id: model_id.into(),
            request_model,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(serde::Serialize)]
struct EmbedRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    input: &'a str,
}

/// Accepts both the OpenAI `/embeddings` response (`{"data":[{"embedding":…}]}`)
/// and the legacy minimal top-level `{"embedding":…}` shape.
#[derive(serde::Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    data: Vec<EmbedDatum>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
}

#[derive(serde::Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
}

impl EmbedResponse {
    /// The first embedding (OpenAI batches, but we send one input), falling back
    /// to the legacy top-level field.
    fn into_embedding(self) -> Option<Vec<f32>> {
        self.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .or(self.embedding)
    }
}

#[async_trait::async_trait]
impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&EmbedRequest {
                model: self.request_model.as_deref(),
                input: text,
            })
            .send()
            .await
            .map_err(|e| EmbedError::Unavailable(format!("embed request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(EmbedError::Unavailable(format!(
                "embed endpoint returned {}",
                resp.status()
            )));
        }
        let body: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| EmbedError::Unavailable(format!("embed response decode failed: {e}")))?;
        let embedding = body.into_embedding().ok_or_else(|| {
            EmbedError::Unavailable(
                "embed response carried no embedding (expected data[0].embedding or embedding)"
                    .to_string(),
            )
        })?;
        if embedding.len() != self.dim {
            // A width that doesn't match the index is a misconfiguration, not a
            // silent wrong-width upsert (the ANN would then never match). Point
            // MEMKEYS_EMBED_DIM at the model's true width.
            return Err(EmbedError::Unavailable(format!(
                "embed endpoint returned dim {}, expected {} (set MEMKEYS_EMBED_DIM to match the model)",
                embedding.len(),
                self.dim
            )));
        }
        Ok(embedding)
    }
}

/// The configured embedder output width: [`EMBED_DIM_ENV`] parsed, else the
/// [`HASH_EMBED_DIM`] default. An out-of-range or non-numeric value is a boot
/// misconfig — we log it and fall back to the default rather than wire a width
/// the HNSW index cannot hold.
pub fn embed_dim_from_env() -> usize {
    match std::env::var(EMBED_DIM_ENV).ok().filter(|s| !s.is_empty()) {
        None => HASH_EMBED_DIM,
        Some(s) => match s.parse::<usize>() {
            Ok(d) if (1..=MAX_EMBED_DIM).contains(&d) => d,
            _ => {
                tracing::error!(
                    target: "memkeys",
                    "memkeys.embedder.dim_invalid: MEMKEYS_EMBED_DIM={s:?} is not an integer in \
                     1..={MAX_EMBED_DIM}; falling back to {HASH_EMBED_DIM}"
                );
                HASH_EMBED_DIM
            }
        },
    }
}

/// The deployment embedder factory (the single wiring point). `MEMKEYS_EMBED_URL`
/// set ⇒ [`HttpEmbedder`] at [`embed_dim_from_env`] width; unset ⇒ the
/// [`HashEmbedder`] fallback (fixed 256) so a minimal deployment still runs
/// semantic search with no model server.
pub fn from_env() -> Arc<dyn Embedder> {
    match std::env::var(EMBED_URL_ENV).ok().filter(|s| !s.is_empty()) {
        Some(url) => {
            // The configured model name (if any) is both the derivation stamp
            // (M7 — a swap backfills prior vectors) AND the OpenAI request
            // `model`. Unset ⇒ stamp from the url so a bare URL swap still
            // counts as a new model, and omit the request field.
            let request_model = std::env::var(EMBED_MODEL_ENV)
                .ok()
                .filter(|s| !s.is_empty());
            let model_id = request_model
                .clone()
                .unwrap_or_else(|| format!("http:{url}"));
            let dim = embed_dim_from_env();
            tracing::info!(
                target: "memkeys",
                "memkeys.embedder.http: using OpenAI-compatible HTTP embedder at {url} (dim {dim}, model {model_id})"
            );
            Arc::new(HttpEmbedder::new(url, dim, model_id, request_model))
        }
        None => {
            tracing::info!(
                target: "memkeys",
                "memkeys.embedder.hash: MEMKEYS_EMBED_URL unset — using the deterministic \
                 HashEmbedder fallback (dim {HASH_EMBED_DIM})"
            );
            Arc::new(HashEmbedder::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_embedder_reports_configured_dim_and_model() {
        let e = HttpEmbedder::new("http://localhost:9/embed", 768, "nomic-embed-text", None);
        assert_eq!(e.dim(), 768);
        assert_eq!(e.model_id(), "nomic-embed-text");
    }

    #[test]
    fn response_parses_openai_and_legacy_shapes() {
        // OpenAI /embeddings: data[].embedding
        let openai: EmbedResponse =
            serde_json::from_str(r#"{"data":[{"embedding":[1.0,2.0,3.0]}],"model":"x"}"#).unwrap();
        assert_eq!(openai.into_embedding(), Some(vec![1.0, 2.0, 3.0]));
        // Legacy minimal top-level embedding
        let legacy: EmbedResponse = serde_json::from_str(r#"{"embedding":[4.0,5.0]}"#).unwrap();
        assert_eq!(legacy.into_embedding(), Some(vec![4.0, 5.0]));
        // Neither ⇒ None (surfaced as a clear EmbedError at call time)
        let empty: EmbedResponse = serde_json::from_str(r#"{"object":"list","data":[]}"#).unwrap();
        assert_eq!(empty.into_embedding(), None);
    }

    #[test]
    fn request_omits_model_when_unset() {
        let with = serde_json::to_string(&EmbedRequest {
            model: Some("nomic-embed-text"),
            input: "hi",
        })
        .unwrap();
        assert!(with.contains("\"model\":\"nomic-embed-text\""));
        let without = serde_json::to_string(&EmbedRequest {
            model: None,
            input: "hi",
        })
        .unwrap();
        assert!(!without.contains("model"));
    }
}
