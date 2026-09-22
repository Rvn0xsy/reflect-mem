//! Embedding client (Ollama HTTP).
//!
//! Must produce the *same* vectors as the original pipeline, or the 520 MB of
//! `reflect-mem.lancedb` we reuse in place becomes unusable: model
//! `qwen3-embedding:0.6b`, 1024 dimensions. The dimension is asserted on every
//! response — a silent mismatch would poison vector search.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The existing `.env` points Ollama at `host.docker.internal` because the
/// original pipeline ran in a container. A host-native binary cannot resolve
/// that, so rewrite it unless we ourselves are in Docker.
fn rewrite_endpoint_for_host(raw: &str, in_docker: bool) -> String {
    if in_docker {
        raw.to_string()
    } else {
        raw.replace("host.docker.internal", "localhost")
    }
}

fn running_in_docker() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/app").is_dir()
}

/// Ollama (`/api/embed`) and OpenAI (`/v1/embeddings`) both return an array of
/// vectors, just under a different key.
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embeddings: Option<Vec<Vec<f32>>>,
    #[serde(default)]
    data: Option<Vec<EmbedData>>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

impl EmbedResponse {
    /// Normalise to one vector per input, in input order.
    fn into_vectors(self) -> Vec<Vec<f32>> {
        if let Some(v) = self.embeddings {
            return v;
        }
        let mut data = self.data.unwrap_or_default();
        // OpenAI-ish payloads may arrive out of order; `index` restores it.
        if data.iter().all(|d| d.index.is_some()) {
            data.sort_by_key(|d| d.index.unwrap_or(0));
        }
        data.into_iter().map(|d| d.embedding).collect()
    }
}

/// Embedding client. Speaks Ollama's `/api/embed` and OpenAI's
/// `/v1/embeddings` wire format; the response shape is detected.
#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: usize,
    /// Sent as `Authorization: Bearer <key>`.
    api_key: Option<String>,
    /// Extra headers, for providers that do not use Bearer auth.
    headers: std::collections::BTreeMap<String, String>,
}

impl EmbeddingClient {
    /// Build from the resolved settings (config file overlaid by env).
    pub fn from_settings() -> Result<Self> {
        let s = crate::settings::get();
        let endpoint = rewrite_endpoint_for_host(&s.embedding.endpoint, running_in_docker());
        Self::with_auth(
            endpoint,
            s.embedding.model.clone(),
            s.embedding.dimensions,
            s.embedding.api_key.clone(),
            s.embedding.headers.clone(),
        )
    }

    pub fn new(endpoint: String, model: String, dimensions: usize) -> Result<Self> {
        Self::with_auth(endpoint, model, dimensions, None, Default::default())
    }

    pub fn with_auth(
        endpoint: String,
        model: String,
        dimensions: usize,
        api_key: Option<String>,
        headers: std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            endpoint,
            model,
            dimensions,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            headers,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Embed one text.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self
            .embed_batch(std::slice::from_ref(&text.to_string()))
            .await?;
        out.pop().context("embedding response contained no result")
    }

    /// Embed several texts in one request. Both supported APIs take an array
    /// `input`.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("POST {}", self.endpoint))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("embedding request failed ({status}): {body}");
        }

        let parsed: EmbedResponse = resp.json().await.context("parsing embedding response")?;
        let embeddings = parsed.into_vectors();
        if embeddings.len() != texts.len() {
            bail!(
                "embedding count mismatch: asked {} texts, got {} vectors",
                texts.len(),
                embeddings.len()
            );
        }
        for v in &embeddings {
            if v.len() != self.dimensions {
                bail!(
                    "embedding dimension mismatch: model {} returned {}, expected {} \
                     (reusing existing vectors requires the same model)",
                    self.model,
                    v.len(),
                    self.dimensions
                );
            }
        }
        Ok(embeddings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_docker_hostname_for_host_runs() {
        let raw = "http://host.docker.internal:11434/api/embed";
        assert_eq!(
            rewrite_endpoint_for_host(raw, false),
            "http://localhost:11434/api/embed"
        );
    }

    #[test]
    fn keeps_docker_hostname_inside_docker() {
        let raw = "http://host.docker.internal:11434/api/embed";
        assert_eq!(rewrite_endpoint_for_host(raw, true), raw);
    }

    #[test]
    fn leaves_other_hosts_untouched() {
        let raw = "http://127.0.0.1:11434/api/embed";
        assert_eq!(rewrite_endpoint_for_host(raw, false), raw);
    }

    #[test]
    fn parses_ollama_response() {
        let r: EmbedResponse =
            serde_json::from_str(r#"{"embeddings":[[1.0,2.0],[3.0,4.0]]}"#).unwrap();
        assert_eq!(r.into_vectors(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn parses_openai_response_and_restores_order() {
        let r: EmbedResponse = serde_json::from_str(
            r#"{"data":[{"index":1,"embedding":[3.0,4.0]},{"index":0,"embedding":[1.0,2.0]}]}"#,
        )
        .unwrap();
        assert_eq!(r.into_vectors(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn unknown_response_shape_yields_no_vectors() {
        let r: EmbedResponse = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(r.into_vectors().is_empty());
    }
}
