//! Embedder adapters + the deployment factory (§5.4.2, §2 "the embedding model
//! varies most across deployments, so it is pushed to the edge").
//!
//! Two concrete embedders sit behind the backend-portable [`Embedder`] port:
//! - [`HashEmbedder`](crate::semantic::HashEmbedder) — the deterministic,
//!   dependency-free default (semantic.rs); lets a minimal deployment and CI
//!   run semantic search with no model server.
//! - [`HttpEmbedder`] — calls a real embedding-model endpoint over HTTP.
//!
//! [`from_env`] is the single wiring point: `MEMKEYS_EMBED_URL` set ⇒ the HTTP
//! embedder; unset ⇒ the hash embedder fallback, so a minimal deployment still
//! runs. The output dimension MUST match migration 0003 (`vector(256)`); the
//! factory pins it to [`HASH_EMBED_DIM`] and an HTTP embedder that returns a
//! different width fails the embed (a dimension mismatch is a misconfiguration,
//! not a silent wrong-width upsert).

use std::sync::Arc;

use crate::semantic::{EmbedError, Embedder, HashEmbedder, HASH_EMBED_DIM};

/// Environment variable naming the embedding endpoint. Unset ⇒ [`HashEmbedder`].
pub const EMBED_URL_ENV: &str = "MEMKEYS_EMBED_URL";

/// An HTTP embedding-model adapter.
///
/// Wire contract (deliberately minimal — a thin shim a deployment adapts to its
/// model server): `POST <url>` with `{"input": "<text>"}` → `{"embedding":
/// [<f32>, …]}`. The returned vector must have exactly `dim` components.
pub struct HttpEmbedder {
    url: String,
    dim: usize,
    client: reqwest::Client,
}

impl HttpEmbedder {
    /// Build an HTTP embedder for `url` producing `dim`-wide vectors. `dim` must
    /// equal the configured index width (migration 0003 = 256).
    pub fn new(url: impl Into<String>, dim: usize) -> Self {
        Self {
            url: url.into(),
            dim,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(serde::Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
}

#[derive(serde::Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&EmbedRequest { input: text })
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
        if body.embedding.len() != self.dim {
            // A width that doesn't match the index is a misconfiguration, not a
            // silent wrong-width upsert (the ANN would then never match).
            return Err(EmbedError::Unavailable(format!(
                "embed endpoint returned dim {}, expected {}",
                body.embedding.len(),
                self.dim
            )));
        }
        Ok(body.embedding)
    }
}

/// The deployment embedder factory (the single wiring point). `MEMKEYS_EMBED_URL`
/// set ⇒ [`HttpEmbedder`] at the index width ([`HASH_EMBED_DIM`] = 256, matching
/// migration 0003); unset ⇒ the [`HashEmbedder`] fallback so a minimal
/// deployment still runs semantic search with no model server.
pub fn from_env() -> Arc<dyn Embedder> {
    match std::env::var(EMBED_URL_ENV).ok().filter(|s| !s.is_empty()) {
        Some(url) => {
            tracing::info!(
                target: "memkeys",
                "memkeys.embedder.http: using HTTP embedder at {url} (dim {HASH_EMBED_DIM})"
            );
            Arc::new(HttpEmbedder::new(url, HASH_EMBED_DIM))
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
    fn http_embedder_reports_configured_dim() {
        let e = HttpEmbedder::new("http://localhost:9/embed", HASH_EMBED_DIM);
        assert_eq!(e.dim(), HASH_EMBED_DIM);
    }
}
