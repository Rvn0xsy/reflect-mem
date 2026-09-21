//! Embedding client (Ollama HTTP).
//!
//! Must produce the *same* vectors as the Python pipeline, or the 520 MB of
//! `cognee.lancedb` we reuse in place becomes unusable: model
//! `qwen3-embedding:0.6b`, 1024 dimensions. The dimension is asserted on every
//! response — a silent mismatch would poison vector search.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_ENDPOINT: &str = "http://localhost:11434/api/embed";
const DEFAULT_MODEL: &str = "qwen3-embedding:0.6b";
const DEFAULT_DIMENSIONS: usize = 1024;

/// The existing `.env` points Ollama at `host.docker.internal` because the
/// legacy pipeline ran in a container. A host-native binary cannot resolve
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

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

/// Ollama embedding client.
#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: usize,
}

impl EmbeddingClient {
    /// Build from `EMBEDDING_ENDPOINT` / `EMBEDDING_MODEL` / `EMBEDDING_DIMENSIONS`,
    /// matching the Python config names so the same `.env` keeps working.
    pub fn from_env() -> Result<Self> {
        let endpoint = rewrite_endpoint_for_host(
            &env_or("EMBEDDING_ENDPOINT", DEFAULT_ENDPOINT),
            running_in_docker(),
        );
        let model = env_or("EMBEDDING_MODEL", DEFAULT_MODEL);
        let dimensions = env_or("EMBEDDING_DIMENSIONS", &DEFAULT_DIMENSIONS.to_string())
            .parse::<usize>()
            .context("EMBEDDING_DIMENSIONS must be a number")?;
        Self::new(endpoint, model, dimensions)
    }

    pub fn new(endpoint: String, model: String, dimensions: usize) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            endpoint,
            model,
            dimensions,
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

    /// Embed several texts in one request (Ollama accepts an array `input`).
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let resp = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {}", self.endpoint))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("embedding request failed ({status}): {body}");
        }

        let parsed: EmbedResponse = resp.json().await.context("parsing embedding response")?;
        if parsed.embeddings.len() != texts.len() {
            bail!(
                "embedding count mismatch: asked {} texts, got {} vectors",
                texts.len(),
                parsed.embeddings.len()
            );
        }
        for v in &parsed.embeddings {
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
        Ok(parsed.embeddings)
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
}
