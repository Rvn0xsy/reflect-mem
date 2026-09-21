//! Import a dumped graph (`nodes.jsonl` / `edges.jsonl`) into
//! `reflect-mem.graph.sqlite`.
//!
//! The dump is produced by an external exporter (the graph store is a private
//! `LBUG+` format that this binary cannot read); see `reflect-mem migrate
//! --help` for the expected input layout.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::storage::graph::{GraphEdge, GraphNode, GraphStore};

/// `summary.json` written by the dumper, used to reconcile the import.
#[derive(Debug, Deserialize)]
pub struct DumpSummary {
    pub nodes: i64,
    pub edges: i64,
    #[serde(default)]
    pub metadata: i64,
    #[serde(default)]
    pub storage_version: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MetadataRow {
    key: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug)]
pub struct ImportReport {
    pub nodes: usize,
    pub edges: usize,
    pub metadata: usize,
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let item = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: invalid JSONL row", path.display(), idx + 1))?;
        out.push(item);
    }
    Ok(out)
}

fn read_summary(input_dir: &Path) -> Result<Option<DumpSummary>> {
    let path = input_dir.join("summary.json");
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
    ))
}

/// Import `input_dir` into the graph at `graph_path`, replacing any existing
/// graph (the migration is a full, idempotent load).
pub fn import(input_dir: &Path, graph_path: &Path) -> Result<ImportReport> {
    let nodes_path = input_dir.join("nodes.jsonl");
    let edges_path = input_dir.join("edges.jsonl");
    let metadata_path = input_dir.join("metadata.jsonl");

    if !nodes_path.exists() {
        bail!(
            "{} not found — expected a JSONL graph dump in this directory",
            nodes_path.display()
        );
    }

    let summary = read_summary(input_dir)?;
    if let Some(s) = &summary {
        println!(
            "importing dump: {} nodes / {} edges / {} metadata (storage_version={:?})",
            s.nodes, s.edges, s.metadata, s.storage_version
        );
    }

    let nodes: Vec<GraphNode> = read_jsonl(&nodes_path)?;
    let edges: Vec<GraphEdge> = if edges_path.exists() {
        read_jsonl(&edges_path)?
    } else {
        Vec::new()
    };
    let metadata_rows: Vec<MetadataRow> = if metadata_path.exists() {
        read_jsonl(&metadata_path)?
    } else {
        Vec::new()
    };
    let metadata: Vec<(String, String)> = metadata_rows
        .into_iter()
        .map(|m| (m.key, m.value.unwrap_or_default()))
        .collect();

    // Fresh graph every run: a partial re-import would otherwise mix old and new.
    if graph_path.exists() {
        std::fs::remove_file(graph_path)
            .with_context(|| format!("removing existing {}", graph_path.display()))?;
    }
    let store = GraphStore::open(graph_path)?;
    let n_nodes = store.insert_nodes(&nodes)?;
    let n_edges = store.insert_edges(&edges)?;
    let n_meta = store.insert_metadata(&metadata)?;

    // Reconcile against the dumper's own counts.
    let mut problems = Vec::new();
    if let Some(s) = &summary {
        if s.nodes as usize != n_nodes {
            problems.push(format!(
                "nodes: summary says {}, imported {}",
                s.nodes, n_nodes
            ));
        }
        if s.edges as usize != n_edges {
            problems.push(format!(
                "edges: summary says {}, imported {}",
                s.edges, n_edges
            ));
        }
        if s.metadata as usize != n_meta {
            problems.push(format!(
                "metadata: summary says {}, imported {}",
                s.metadata, n_meta
            ));
        }
    }
    let stored_nodes = store.node_count()?;
    let stored_edges = store.edge_count()?;
    if stored_nodes as usize != n_nodes {
        problems.push(format!(
            "nodes: inserted {n_nodes}, db holds {stored_nodes}"
        ));
    }
    if stored_edges as usize != n_edges {
        problems.push(format!(
            "edges: inserted {n_edges}, db holds {stored_edges}"
        ));
    }

    if !problems.is_empty() {
        bail!(
            "import reconciliation failed:\n  - {}",
            problems.join("\n  - ")
        );
    }

    println!(
        "imported ok -> {} ({} nodes, {} edges, {} metadata)",
        graph_path.display(),
        stored_nodes,
        stored_edges,
        n_meta
    );
    Ok(ImportReport {
        nodes: n_nodes,
        edges: n_edges,
        metadata: n_meta,
    })
}
