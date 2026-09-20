//! `doctor` — verify and repair store consistency.
//!
//! The graph (derived data) can be rebuilt from a migration dump; the vector
//! tables are then healed against the live graph in both directions:
//!
//! - rows missing for live nodes are re-embedded (embeddings are deterministic
//!   for a fixed model, so healed vectors equal the originals)
//! - rows for ids no longer in the graph are removed
//!
//! Also removes ref-less edges whose endpoints no longer exist — leftovers of
//! writes made before provenance was stamped on edges.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::embed::EmbeddingClient;
use crate::storage::graph::{GraphNode, GraphStore};
use crate::storage::vector::VectorStore;
use crate::storage::vector_writer::{self, VectorRow};

#[derive(Debug, Default)]
pub struct DoctorReport {
    pub nodes_restored: usize,
    pub edges_deleted: usize,
    pub vectors_added: usize,
    pub vectors_purged: usize,
}

/// Restore graph rows found in a migration dump but missing from the store,
/// then drop edges whose endpoints are gone.
pub fn repair_graph_from_dump(graph: &GraphStore, dump_nodes: &[Value]) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();
    let mut restored: Vec<GraphNode> = Vec::new();

    for row in dump_nodes {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if graph.node_by_id(id)?.is_some() {
            continue;
        }
        restored.push(GraphNode {
            id: id.to_string(),
            name: row.get("name").and_then(Value::as_str).map(str::to_string),
            type_: row.get("type").and_then(Value::as_str).map(str::to_string),
            created_at: row
                .get("created_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            updated_at: row
                .get("updated_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            properties: row
                .get("properties")
                .and_then(Value::as_str)
                .map(str::to_string),
            source_ref_keys: row
                .get("source_ref_keys")
                .and_then(Value::as_str)
                .map(str::to_string),
            source_dataset_ids: row
                .get("source_dataset_ids")
                .and_then(Value::as_str)
                .map(str::to_string),
            source_run_ids: row
                .get("source_run_ids")
                .and_then(Value::as_str)
                .map(str::to_string),
            source_run_refs: row
                .get("source_run_refs")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if !restored.is_empty() {
        graph.insert_nodes(&restored)?;
        report.nodes_restored = restored.len();
    }

    // Collapse duplicate edge rows (re-ingestion used to append copies).
    report.edges_deleted += graph.dedup_edges()?;

    // Ref-less edges with dangling endpoints cannot be attributed to any
    // surviving data item, so they are removed.
    let dangling = graph.edges_with_missing_endpoint()?;
    if !dangling.is_empty() {
        let keys: Vec<(String, String)> = dangling
            .iter()
            .map(|(f, t, _)| (f.clone(), t.clone()))
            .collect();
        report.edges_deleted = graph.delete_edges_by_endpoints(&keys)?;
    }

    Ok(report)
}

/// Heal every vector table against the live graph.
pub async fn heal_vectors(
    graph: &GraphStore,
    vectors: &VectorStore,
    embedder: &EmbeddingClient,
) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();
    let all_nodes = graph.all_nodes()?;
    // table name -> live nodes belonging to it
    let mut tables: std::collections::BTreeMap<String, Vec<&GraphNode>> =
        std::collections::BTreeMap::new();
    for n in &all_nodes {
        let type_ = n.type_.clone().unwrap_or_default();
        let props: Value = n
            .properties
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok())
            .unwrap_or(Value::Null);
        let fields = props
            .get("metadata")
            .and_then(|m| m.get("index_fields"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for f in fields.iter().filter_map(Value::as_str) {
            tables.entry(format!("{type_}_{f}")).or_default().push(n);
        }
    }

    for (table, nodes) in tables {
        let present: HashSet<String> = vectors.table_ids(&table).await?.into_iter().collect();
        let live_here: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();

        let missing: Vec<&GraphNode> = nodes
            .iter()
            .filter(|n| !present.contains(&n.id))
            .copied()
            .collect();
        let stale: Vec<String> = present
            .iter()
            .filter(|id| !live_here.contains(*id))
            .cloned()
            .collect();

        if !missing.is_empty() {
            let texts: Vec<String> = missing
                .iter()
                .map(|n| {
                    let type_ = n.type_.clone().unwrap_or_default();
                    let field = table.strip_prefix(&format!("{type_}_")).unwrap_or("name");
                    embeddable_text(n, field)
                })
                .collect();
            let new_vectors = embedder.embed_batch(&texts).await?;
            let rows: Vec<VectorRow> = missing
                .iter()
                .zip(new_vectors)
                .map(|(n, v)| VectorRow {
                    id: n.id.clone(),
                    vector: v,
                    payload: n
                        .properties
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p).ok())
                        .unwrap_or(Value::Null),
                })
                .collect();
            report.vectors_added += vector_writer::append_rows(
                vectors.connection(),
                &table,
                embedder.dimensions(),
                &rows,
            )
            .await?;
        }

        if !stale.is_empty() {
            report.vectors_purged += vectors.delete_by_ids(&table, &stale).await?;
        }

        // Deduplicate rows sharing an id (append-based ingestion leaves a copy
        // per write; same id + same embeddable text => identical vector, so
        // delete-all-then-reinsert-one is lossless).
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let all_ids = vectors.table_ids(&table).await?;
        for id in &all_ids {
            *counts.entry(id.as_str()).or_default() += 1;
        }
        let dup_ids: Vec<String> = counts
            .into_iter()
            .filter(|(_, c)| *c > 1)
            .map(|(id, _)| id.to_string())
            .collect();
        if !dup_ids.is_empty() {
            vectors.delete_by_ids(&table, &dup_ids).await?;
            let reinsert: Vec<&GraphNode> = nodes
                .iter()
                .filter(|n| dup_ids.contains(&n.id))
                .copied()
                .collect();
            let texts: Vec<String> = reinsert
                .iter()
                .map(|n| {
                    let type_ = n.type_.clone().unwrap_or_default();
                    let field = table.strip_prefix(&format!("{type_}_")).unwrap_or("name");
                    embeddable_text(n, field)
                })
                .collect();
            let new_vectors = embedder.embed_batch(&texts).await?;
            let rows: Vec<VectorRow> = reinsert
                .iter()
                .zip(new_vectors)
                .map(|(n, v)| VectorRow {
                    id: n.id.clone(),
                    vector: v,
                    payload: n
                        .properties
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p).ok())
                        .unwrap_or(Value::Null),
                })
                .collect();
            vector_writer::append_rows(vectors.connection(), &table, embedder.dimensions(), &rows)
                .await?;
            report.vectors_purged += dup_ids.len();
        }
    }

    Ok(report)
}

/// The text a datapoint contributes to its index field.
fn embeddable_text(node: &GraphNode, field: &str) -> String {
    match field {
        "name" => node.name.clone().unwrap_or_default(),
        other => {
            let props: Value = node
                .properties
                .as_deref()
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(Value::Null);
            props
                .get(other)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
    }
}
