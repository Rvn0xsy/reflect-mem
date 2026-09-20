//! `recall` — the read path.
//!
//! Two modes, matching the Python memory API:
//!
//! - [`SearchType::Summaries`] — vector search over the pre-computed
//!   `TextSummary_text` table (no graph).
//! - [`SearchType::GraphCompletion`] — vector search picks seed entities, the
//!   SQLite property graph expands their K-hop neighbourhood, and the LLM
//!   synthesises an answer from that subgraph. This is the multi-hop mode.
//!
//! Raw embedding vectors are never put in the prompt — only text fields.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::embed::EmbeddingClient;
use crate::llm::LlmClient;
use crate::storage::graph::{GraphEdge, GraphNode, GraphStore};
use crate::storage::vector::{TABLE_ENTITY, TABLE_TEXT_SUMMARY, VectorHit, VectorStore};

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a memory assistant. Answer the user's question using ONLY the memory \
context provided. Be concise and concrete. If the context does not contain the \
answer, say so plainly instead of guessing. Answer in the language of the question.";

/// Cap how much context we feed the model, so recall stays cheap and bounded.
const MAX_CONTEXT_CHARS: usize = 12_000;
const MAX_NODE_DESC_CHARS: usize = 300;
const MAX_EDGE_TEXT_CHARS: usize = 200;

/// Which recall mode to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchType {
    Summaries,
    GraphCompletion,
}

impl SearchType {
    /// Parse a user-supplied search type (case-insensitive).
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "SUMMARIES" | "SUMMARY" => Ok(Self::Summaries),
            "GRAPH_COMPLETION" | "GRAPH" => Ok(Self::GraphCompletion),
            other => {
                bail!("unsupported search_type {other:?}; expected SUMMARIES or GRAPH_COMPLETION")
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summaries => "SUMMARIES",
            Self::GraphCompletion => "GRAPH_COMPLETION",
        }
    }
}

/// Recall knobs.
#[derive(Debug, Clone)]
pub struct RecallOptions {
    pub search_type: SearchType,
    /// Vector hits to seed from.
    pub top_k: usize,
    /// Graph expansion depth (GRAPH_COMPLETION only).
    pub max_hops: u32,
    pub system_prompt: Option<String>,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            search_type: SearchType::Summaries,
            top_k: 5,
            max_hops: 2,
            system_prompt: None,
        }
    }
}

/// What recall produced, plus enough provenance to debug a bad answer.
#[derive(Debug)]
pub struct RecallOutcome {
    pub answer: String,
    pub search_type: SearchType,
    pub seeds: Vec<VectorHit>,
    pub reached_nodes: usize,
    pub reached_edges: usize,
    pub context_chars: usize,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Read a string field out of a node/edge `properties` JSON blob.
fn props_field(props: Option<&str>, key: &str) -> Option<String> {
    let raw = props?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    match value.get(key)? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn node_label(node: &GraphNode) -> String {
    let type_ = node.type_.as_deref().unwrap_or("Node");
    match node.name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(name) => format!("[{type_}] {name}"),
        None => format!("[{type_}]"),
    }
}

/// Render the per-node detail lines used as graph context.
fn node_lines(nodes: &[GraphNode]) -> Vec<String> {
    nodes
        .iter()
        .map(|n| {
            let mut line = node_label(n);
            let detail = props_field(n.properties.as_deref(), "description")
                .or_else(|| props_field(n.properties.as_deref(), "text"))
                .or_else(|| props_field(n.properties.as_deref(), "name"));
            if let Some(d) = detail {
                line.push_str(" — ");
                line.push_str(&truncate(&d, MAX_NODE_DESC_CHARS));
            }
            line
        })
        .collect()
}

/// Render edges as `A -[rel]-> B (edge_text)`.
fn edge_lines(edges: &[GraphEdge], names: &HashMap<String, String>) -> Vec<String> {
    edges
        .iter()
        .map(|e| {
            let from = names
                .get(&e.from_id)
                .map(String::as_str)
                .unwrap_or(&e.from_id);
            let to = names.get(&e.to_id).map(String::as_str).unwrap_or(&e.to_id);
            let rel = e.relationship_name.as_deref().unwrap_or("related_to");
            let mut line = format!("{from} -[{rel}]-> {to}");
            if let Some(text) = props_field(e.properties.as_deref(), "edge_text") {
                line.push_str(" — ");
                line.push_str(&truncate(&text, MAX_EDGE_TEXT_CHARS));
            }
            line
        })
        .collect()
}

fn join_bounded(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        if out.chars().count() + line.chars().count() + 1 > MAX_CONTEXT_CHARS {
            out.push_str("\n… (context truncated)");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Run recall.
pub async fn recall(
    query: &str,
    opts: &RecallOptions,
    embedder: &EmbeddingClient,
    vectors: &VectorStore,
    graph: &GraphStore,
    llm: &LlmClient,
) -> Result<RecallOutcome> {
    let query_vec = embedder.embed(query).await?;

    let (context, seeds, reached_nodes, reached_edges) = match opts.search_type {
        SearchType::Summaries => {
            let hits = vectors
                .search(TABLE_TEXT_SUMMARY, query_vec, opts.top_k)
                .await?;
            if hits.is_empty() {
                return Ok(empty_outcome(opts.search_type));
            }
            let lines: Vec<String> = hits
                .iter()
                .filter_map(|h| h.text.as_deref())
                .map(|t| truncate(t, MAX_NODE_DESC_CHARS * 2))
                .collect();
            (join_bounded(&lines), hits, 0, 0)
        }
        SearchType::GraphCompletion => {
            let seeds = vectors.search(TABLE_ENTITY, query_vec, opts.top_k).await?;
            if seeds.is_empty() {
                return Ok(empty_outcome(opts.search_type));
            }
            let seed_ids: Vec<String> = seeds.iter().map(|h| h.id.clone()).collect();
            let reached = graph.traverse(&seed_ids, opts.max_hops, None)?;
            let nodes: Vec<GraphNode> = reached.iter().map(|r| r.node.clone()).collect();
            let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
            let edges = graph.edges_within(&ids)?;
            let names: HashMap<String, String> = nodes
                .iter()
                .map(|n| {
                    let label = match n.name.as_deref().filter(|s| !s.trim().is_empty()) {
                        Some(name) => name.to_string(),
                        None => node_label(n),
                    };
                    (n.id.clone(), label)
                })
                .collect();

            let mut lines = vec!["Entities:".to_string()];
            lines.extend(node_lines(&nodes));
            if !edges.is_empty() {
                lines.push(String::new());
                lines.push("Relationships:".to_string());
                lines.extend(edge_lines(&edges, &names));
            }
            (join_bounded(&lines), seeds, nodes.len(), edges.len())
        }
    };

    if context.trim().is_empty() {
        return Ok(empty_outcome(opts.search_type));
    }

    let system = opts
        .system_prompt
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let user = format!("Memory context:\n{context}\n\nQuestion: {query}");
    let answer = llm.complete(&system, &user).await?;

    Ok(RecallOutcome {
        answer,
        search_type: opts.search_type,
        seeds,
        reached_nodes,
        reached_edges,
        context_chars: context.chars().count(),
    })
}

fn empty_outcome(search_type: SearchType) -> RecallOutcome {
    RecallOutcome {
        answer: "未找到相关记忆。".to_string(),
        search_type,
        seeds: Vec::new(),
        reached_nodes: 0,
        reached_edges: 0,
        context_chars: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_types_case_insensitively() {
        assert_eq!(
            SearchType::parse("summaries").unwrap(),
            SearchType::Summaries
        );
        assert_eq!(
            SearchType::parse(" graph_completion ").unwrap(),
            SearchType::GraphCompletion
        );
        assert!(SearchType::parse("chunks").is_err());
    }

    #[test]
    fn reads_fields_out_of_properties_json() {
        let props = r#"{"description":"hello","edge_text":"linking","n":7}"#;
        assert_eq!(
            props_field(Some(props), "description").as_deref(),
            Some("hello")
        );
        assert_eq!(
            props_field(Some(props), "edge_text").as_deref(),
            Some("linking")
        );
        assert_eq!(props_field(Some(props), "n").as_deref(), Some("7"));
        assert_eq!(props_field(Some(props), "missing"), None);
        assert_eq!(props_field(None, "description"), None);
        assert_eq!(props_field(Some("not json"), "description"), None);
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        // multi-byte must not panic or split a codepoint
        assert_eq!(truncate("开发习惯", 2), "开发…");
    }

    #[test]
    fn node_label_falls_back_to_type() {
        let mut n = GraphNode {
            id: "x".into(),
            name: Some(String::new()),
            type_: Some("DocumentChunk".into()),
            created_at: None,
            updated_at: None,
            properties: None,
            source_ref_keys: None,
            source_dataset_ids: None,
            source_run_ids: None,
            source_run_refs: None,
        };
        assert_eq!(node_label(&n), "[DocumentChunk]");
        n.name = Some("可读的名字".into());
        assert_eq!(node_label(&n), "[DocumentChunk] 可读的名字");
    }

    #[test]
    fn context_is_bounded() {
        let lines: Vec<String> = (0..1000)
            .map(|i| format!("line {i} {}", "x".repeat(100)))
            .collect();
        let joined = join_bounded(&lines);
        assert!(joined.chars().count() <= MAX_CONTEXT_CHARS + 40);
        assert!(joined.ends_with("(context truncated)"));
    }
}
