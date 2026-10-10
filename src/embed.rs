//! Embedding client (Ollama HTTP).
//!
//! Must produce the *same* vectors as the original pipeline, or the 520 MB of
//! `reflect-mem.lancedb` we reuse in place becomes unusable: model
//! `qwen3-embedding:0.6b`, 1024 dimensions. The dimension is asserted on every
//! response — a silent mismatch would poison vector search.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Inputs per embedding HTTP request. Ollama resets the connection on very large
/// payloads; a 548-datapoint ingest died that way mid-write.
const EMBED_BATCH_SIZE: usize = 64;
/// How many times a failed batch is sent again before it is split in half.
const EMBED_ATTEMPTS: usize = 2;
/// How many times a batch may be halved before the error is returned.
const EMBED_MAX_SPLIT_DEPTH: usize = 3;

/// What the retry/split recursion for one batch returns.
type BatchFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>>> + Send + 'a>>;

/// Index ranges for the HTTP requests one [`EmbeddingClient::embed_batch`] makes.
fn split_batches(texts: &[String]) -> Vec<(usize, usize)> {
    (0..texts.len())
        .step_by(EMBED_BATCH_SIZE)
        .map(|start| (start, (start + EMBED_BATCH_SIZE).min(texts.len())))
        .collect()
}

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

    /// Embed several texts. Both supported APIs take an array `input`.
    ///
    /// The work is split into [`EMBED_BATCH_SIZE`] inputs per request: Ollama
    /// resets the connection on very large payloads (a 548-datapoint ingest did,
    /// in the middle of writing vectors), and smaller requests also make a retry
    /// cheap. A failing batch is retried, then halved — down to single texts —
    /// so one oversized or pathological input cannot cost the whole item.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for (index, batch) in split_batches(texts).into_iter().enumerate() {
            let slice = &texts[batch.0..batch.1];
            out.extend(self.embed_batch_resilient(slice, index, 0).await?);
        }
        Ok(out)
    }

    fn embed_batch_resilient<'a>(
        &'a self,
        texts: &'a [String],
        batch_index: usize,
        depth: usize,
    ) -> BatchFuture<'a> {
        Box::pin(async move {
            let mut last_err = None;
            for _ in 0..EMBED_ATTEMPTS {
                match self.embed_request(texts).await {
                    Ok(vectors) => return Ok(vectors),
                    Err(err) => last_err = Some(err),
                }
            }
            let err = last_err.unwrap_or_else(|| anyhow::anyhow!("embedding failed"));
            if depth >= EMBED_MAX_SPLIT_DEPTH || texts.len() == 1 {
                return Err(err).with_context(|| {
                    format!(
                        "embedding batch {batch_index} failed after {EMBED_ATTEMPTS} tries \
                         ({} text(s), first: {:.60})",
                        texts.len(),
                        texts.first().map(String::as_str).unwrap_or("")
                    )
                });
            }
            let mid = texts.len() / 2;
            eprintln!(
                "[embed] batch {batch_index}: {} texts failed ({err}); splitting in half",
                texts.len()
            );
            let (left, right) = texts.split_at(mid);
            let mut vectors = self
                .embed_batch_resilient(left, batch_index, depth + 1)
                .await?;
            vectors.extend(
                self.embed_batch_resilient(right, batch_index, depth + 1)
                    .await?,
            );
            Ok(vectors)
        })
    }

    /// One HTTP request: the whole batch in a single `input` array.
    async fn embed_request(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn split_batches_covers_everything_in_order() {
        let texts: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let batches = split_batches(&texts);
        assert_eq!(batches.len(), 4); // 64 + 64 + 64 + 8
        assert_eq!(batches[0], (0, 64));
        assert_eq!(batches[3], (192, 200));
        let total: usize = batches.iter().map(|(a, b)| b - a).sum();
        assert_eq!(total, texts.len());
    }

    #[test]
    fn split_batches_of_empty_input_is_empty() {
        assert!(split_batches(&[]).is_empty());
    }
}
