//! MCP server (rmcp).
//!
//! Exposes the memory API over MCP. This first cut ships the **read path**
//! (`recall`), which is the operation an agent runs before answering and the
//! one already verified end to end. The write path (`remember`/`forget`) is
//! deliberately absent rather than half-implemented: writing has to reproduce
//! cognee's graph/vector schema exactly, and a sloppy write would corrupt a
//! real 651 MB memory store. See `docs/design.md` §13.

use std::sync::Arc;

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
    /// Build from the environment, reusing the existing cognee data root.
    pub async fn from_env() -> anyhow::Result<Self> {
        let db = lancedb::connect(&config::lancedb_path().to_string_lossy())
            .execute()
            .await?;
        Ok(Self {
            embedder: EmbeddingClient::from_env()?,
            llm: LlmClient::from_env()?,
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
    pub async fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            svc: Arc::new(MemoryService::from_env().await?),
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
                "Long-term memory backed by a cognee-shaped store (SQLite property graph \
                 + LanceDB vectors). Use `recall` before answering a question that may \
                 depend on earlier context, and say so honestly when it finds nothing.",
            )
    }
}
