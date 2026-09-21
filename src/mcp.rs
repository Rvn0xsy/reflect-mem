//! MCP server (rmcp).
//!
//! Exposes the memory API over MCP: `remember` / `recall` / `forget`. Writing
//! must reproduce the graph/vector schema exactly or it would corrupt the
//! store, so the write path stays schema-faithful to the migrated data. See
//! `docs/design.md`.

use std::sync::Arc;

use anyhow::Context as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use uuid::Uuid;

use crate::config;
use crate::embed::EmbeddingClient;
use crate::forget;
use crate::llm::LlmClient;
use crate::recall::{self, RecallOptions, SearchType};
use crate::remember;
use crate::storage::graph::GraphStore;
use crate::storage::relational::RelationalStore;
use crate::storage::vector::VectorStore;
use crate::storage::vector_writer::VectorWriter;

/// State shared by every tool call. Built once, then shared behind an `Arc`.
pub struct MemoryService {
    embedder: EmbeddingClient,
    llm: LlmClient,
    vectors: VectorStore,
    graph: GraphStore,
    relational: RelationalStore,
    vector_writer: VectorWriter,
}

/// forgeUuid parse helper for tool params.
fn parse_uuid(raw: &str, what: &str) -> std::result::Result<Uuid, ErrorData> {
    Uuid::parse_str(raw)
        .map_err(|_| ErrorData::invalid_params(format!("{what} must be a UUID, got {raw:?}"), None))
}

impl MemoryService {
    /// Build from the resolved settings, reusing the existing data root.
    pub async fn from_settings() -> anyhow::Result<Self> {
        let db = lancedb::connect(&config::lancedb_path().to_string_lossy())
            .execute()
            .await?;
        Ok(Self {
            embedder: EmbeddingClient::from_settings()?,
            llm: LlmClient::from_settings()?,
            graph: GraphStore::open(&config::graph_db_path())?,
            relational: RelationalStore::open(&config::relational_db_path())?,
            vectors: VectorStore::new(db.clone()),
            vector_writer: VectorWriter::new(db),
        })
    }
}

/// Convert any error into an MCP internal error.
fn internal<E: std::fmt::Display>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Parameters for `recall`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallParams {
    /// Natural-language question to search memory for.
    pub query: String,
    /// `SUMMARIES` (fast, pre-computed summaries) or `GRAPH_COMPLETION`
    /// (multi-hop reasoning over the knowledge graph). Defaults to `SUMMARIES`.
    #[serde(default)]
    pub search_type: Option<String>,
    /// How many memory items to retrieve. Keep small to bound context. Default 5.
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Graph expansion depth for `GRAPH_COMPLETION`. Default 2.
    #[serde(default)]
    pub hops: Option<u32>,
}

/// Parameters for `forget`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetParams {
    /// Remove one data item by UUID (requires dataset_name).
    #[serde(default)]
    pub data_id: Option<String>,
    /// Forget everything in this dataset.
    #[serde(default)]
    pub dataset: Option<String>,
    /// Dataset UUID alternative to dataset.
    #[serde(default)]
    pub dataset_id: Option<String>,
    /// Wipe all memory. Irreversible.
    #[serde(default)]
    pub everything: bool,
}

/// Parameters for `remember`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberParams {
    /// The text to store as permanent memory.
    pub data: String,
    /// Target dataset. Defaults to `main_dataset`.
    #[serde(default)]
    pub dataset_name: Option<String>,
    /// Optional hint steering entity extraction.
    #[serde(default)]
    pub custom_prompt: Option<String>,
}

/// The MCP server.
#[derive(Clone)]
pub struct MemoryServer {
    tool_router: ToolRouter<Self>,
    svc: Arc<MemoryService>,
}

impl MemoryServer {
    pub async fn from_settings() -> anyhow::Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            svc: Arc::new(MemoryService::from_settings().await?),
        })
    }

    /// Serve over stdio (the transport IDE/CLI agents use) until the client
    /// disconnects.
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let running = self
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed: {e}"))?;
        running
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("MCP server task failed: {e}"))?;
        Ok(())
    }

    /// Serve over streamable HTTP at `/mcp` until the process is stopped.
    ///
    /// When `token` is set, every request must carry
    /// `Authorization: Bearer <token>`; without one the endpoint is open, which
    /// is only safe when it is bound to loopback.
    pub async fn serve_streamable_http(
        self,
        bind: &str,
        token: Option<String>,
    ) -> anyhow::Result<()> {
        use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
        use rmcp::transport::streamable_http_server::tower::{
            StreamableHttpServerConfig, StreamableHttpService,
        };

        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("binding {bind}"))?;
        let addr = listener.local_addr()?;

        // The service factory is synchronous, so build the server once and
        // hand out clones (each MCP session gets its own handler).
        let service: StreamableHttpService<Self, LocalSessionManager> = StreamableHttpService::new(
            {
                let server = self.clone();
                move || Ok(server.clone())
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );

        let mut router = axum::Router::new().nest_service("/mcp", service);
        match &token {
            Some(t) => {
                router = router.layer(axum::middleware::from_fn_with_state(
                    BearerToken(t.clone()),
                    require_bearer,
                ));
                eprintln!("reflect-mem MCP on http://{addr}/mcp (bearer token required)");
            }
            None => {
                eprintln!(
                    "reflect-mem MCP on http://{addr}/mcp (no auth — set mcp.token to require one)"
                );
            }
        }

        axum::serve(listener, router)
            .await
            .context("serving streamable HTTP")?;
        Ok(())
    }
}

/// Shared state for the bearer-token middleware.
#[derive(Clone)]
struct BearerToken(String);

/// Reject any request that does not present the expected bearer token.
async fn require_bearer(
    axum::extract::State(expected): axum::extract::State<BearerToken>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if token_matches(presented.trim(), &expected.0) {
        return next.run(req).await;
    }

    let mut res = axum::response::Response::new(axum::body::Body::from("unauthorized\n"));
    *res.status_mut() = axum::http::StatusCode::UNAUTHORIZED;
    res.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer"),
    );
    res
}

/// Constant-time comparison, so the token cannot be recovered by timing.
fn token_matches(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[tool_router(router = tool_router)]
impl MemoryServer {
    /// Search long-term memory and synthesise an answer.
    #[tool(
        name = "recall",
        description = "Search long-term memory and answer a question from it. \
            Use SUMMARIES for fast lookups over pre-computed summaries, and \
            GRAPH_COMPLETION when the answer requires connecting entities across \
            several memories (multi-hop reasoning). Returns a short synthesised \
            answer, or states plainly that nothing relevant was found."
    )]
    pub async fn recall(
        &self,
        Parameters(p): Parameters<RecallParams>,
    ) -> Result<String, ErrorData> {
        let search_type = match p.search_type.as_deref() {
            None => SearchType::Summaries,
            Some(raw) => SearchType::parse(raw).map_err(internal)?,
        };
        let opts = RecallOptions {
            search_type,
            top_k: p.top_k.unwrap_or(5).clamp(1, 50),
            max_hops: p.hops.unwrap_or(2).min(6),
            system_prompt: None,
        };

        let outcome = recall::recall(
            &p.query,
            &opts,
            &self.svc.embedder,
            &self.svc.vectors,
            &self.svc.graph,
            &self.svc.llm,
        )
        .await
        .map_err(internal)?;

        Ok(outcome.answer)
    }

    /// Remove memory items (graph nodes/edges, vectors, relational rows).
    #[tool(
        name = "forget",
        description = "Forget stored memory. Pass data_id + dataset to forget one item,             dataset to forget everything in a dataset, or everything=true to wipe all memory.             Shared entities referenced by surviving memories are kept; only the reference             is detached. Irreversible."
    )]
    pub async fn forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
    ) -> Result<String, ErrorData> {
        let data_id = match &p.data_id {
            Some(raw) => Some(parse_uuid(raw, "data_id")?),
            None => None,
        };
        let dataset_id = match &p.dataset_id {
            Some(raw) => Some(parse_uuid(raw, "dataset_id")?),
            None => None,
        };
        let target = forget::resolve_target(data_id, p.dataset.clone(), dataset_id, p.everything)
            .map_err(internal)?;
        let report = forget::forget(
            &target,
            &self.svc.graph,
            &self.svc.relational,
            &self.svc.vectors,
            &self.svc.vector_writer,
        )
        .await
        .map_err(internal)?;
        Ok(format!(
            "Forgot {} data item(s): removed {} nodes ({} shared kept), {} edges, {} vectors, {} datasets.",
            report.data_items,
            report.nodes_deleted,
            report.nodes_detached,
            report.edges_deleted,
            report.vectors_deleted,
            report.datasets_deleted
        ))
    }

    /// Store text as permanent memory (ingests + builds knowledge graph).
    #[tool(
        name = "remember",
        description = "Store text as permanent memory: runs entity extraction and             integrates the facts into the knowledge graph. Ingesting the same content             twice is a no-op. Returns a short confirmation with counts."
    )]
    pub async fn remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> Result<String, ErrorData> {
        let dataset = p
            .dataset_name
            .as_deref()
            .unwrap_or("main_dataset")
            .to_string();
        let ctx = remember::WriteContext {
            embedder: &self.svc.embedder,
            llm: &self.svc.llm,
            graph: &self.svc.graph,
            relational: &self.svc.relational,
            vectors: &self.svc.vector_writer,
        };
        let report = remember::remember(
            &p.data,
            &dataset,
            p.custom_prompt.as_deref(),
            &ctx,
            &remember::StderrProgress::new(),
        )
        .await
        .map_err(internal)?;

        if report.skipped_existing {
            return Ok(format!(
                "Already stored (dataset={}, data_id={}); nothing to do.",
                report.dataset, report.data_id
            ));
        }
        Ok(format!(
            "Stored in dataset '{}': {} chunk(s), +{} graph nodes, +{} edges, {} vectors. \
             data_id={}",
            report.dataset,
            report.chunks,
            report.graph_nodes,
            report.graph_edges,
            report.vectors,
            report.data_id
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "reflect-mem",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Long-term memory backed by a SQLite property graph + LanceDB vectors. \
                 Use `recall` before answering a question that may depend on earlier \
                 context, and say so honestly when it finds nothing.",
            )
    }
}
