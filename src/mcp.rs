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

use crate::config;
use crate::embed::EmbeddingClient;
use crate::llm::LlmClient;
use crate::recall::{self, RecallOptions, SearchType};
use crate::storage::graph::GraphStore;
use crate::storage::vector::VectorStore;

/// State shared by every tool call. Built once, then shared behind an `Arc`.
pub struct MemoryService {
    embedder: EmbeddingClient,
    llm: LlmClient,
    vectors: VectorStore,
    graph: GraphStore,
}

impl MemoryService {
    /// Build from the environment, reusing the existing cognee data root.
    pub async fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            embedder: EmbeddingClient::from_env()?,
            llm: LlmClient::from_env()?,
            vectors: VectorStore::open(&config::lancedb_path()).await?,
            graph: GraphStore::open(&config::graph_db_path())?,
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
