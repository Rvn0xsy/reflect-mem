//! `forget` — provenance-scoped deletion across all three stores.
//!
//! Mirrors cognee's graph-provenance delete (the mode the migrated store is
//! in, `GraphMetadata.provenance_version = 1`):
//!
//! 1. find nodes/edges carrying the ref `source_ref:v1:<dataset>:<data>`
//! 2. partition: refs left empty → hard delete; nodes still owned by other
//!    data → detach the ref only (shared entities survive)
//! 3. delete vectors of hard-deleted nodes from `{type}_{index_field}` tables
//! 4. hard-delete unowned edges (then nodes); clean orphaned EdgeType vectors
//! 5. delete the relational `data` row (and dataset when emptying it)

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::config;
use crate::ingest::ids;
use crate::storage::graph::GraphStore;
use crate::storage::relational::RelationalStore;
use crate::storage::vector::VectorStore;
use crate::storage::vector_writer::VectorWriter;

/// The three vector tables that exist per datapoint type + which property is
/// indexed; used to locate vector rows for a deleted node.
fn vector_tables_for(type_: &str, properties: &Value) -> Vec<String> {
    let fields = properties
        .get("metadata")
        .and_then(|m| m.get("index_fields"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if fields.is_empty() {
        return vec![];
    }
    fields
        .iter()
        .filter_map(Value::as_str)
        .map(|f| format!("{type_}_{f}"))
        .collect()
}

/// Deletion target, resolved before touching any store.
#[derive(Debug)]
pub enum ForgetTarget {
    /// One data item within a dataset.
    DataItem { dataset: String, data_id: Uuid },
    /// Every data item in a dataset (dataset row included).
    Dataset { dataset: String },
    /// All datasets of the user.
    Everything,
}

/// Counts for the confirmation message.
#[derive(Debug, Default)]
pub struct ForgetReport {
    pub data_items: usize,
    pub nodes_deleted: usize,
    pub nodes_detached: usize,
    pub edges_deleted: usize,
    pub vectors_deleted: usize,
    pub datasets_deleted: usize,
}

/// Resolve the CLI/MCP parameters into a target.
pub fn resolve_target(
    data_id: Option<Uuid>,
    dataset: Option<String>,
    dataset_id: Option<Uuid>,
    everything: bool,
) -> Result<ForgetTarget> {
    if everything {
        return Ok(ForgetTarget::Everything);
    }
    if let Some(did) = data_id {
        let ds = dataset
            .or_else(|| dataset_id.map(|id| id.to_string()))
            .context("data_id requires --dataset or --dataset-id")?;
        return Ok(ForgetTarget::DataItem {
            dataset: ds,
            data_id: did,
        });
    }
    if let Some(ds) = dataset.or_else(|| dataset_id.map(|id| id.to_string())) {
        return Ok(ForgetTarget::Dataset { dataset: ds });
    }
    bail!("nothing to forget: pass data_id+dataset, dataset, or everything=true")
}

/// Execute a forget.
pub async fn forget(
    target: &ForgetTarget,
    graph: &GraphStore,
    relational: &RelationalStore,
    vectors: &VectorStore,
    vector_writer: &VectorWriter,
) -> Result<ForgetReport> {
    let mut report = ForgetReport::default();

    let data_items: Vec<(Uuid, Uuid)> = match target {
        ForgetTarget::DataItem { dataset, data_id } => {
            let ds_id = relational.dataset_id(dataset)?;
            vec![(*data_id, ds_id)]
        }
        ForgetTarget::Dataset { dataset } => {
            let ds_id = relational.dataset_id(dataset)?;
            let items = relational.data_ids_for_dataset(ds_id)?;
            if items.is_empty() {
                // dataset exists but empty: still drop the dataset row below
                vec![]
            } else {
                items.into_iter().map(|d| (d, ds_id)).collect()
            }
        }
        ForgetTarget::Everything => {
            let mut items = Vec::new();
            for (ds_id, _name) in relational.all_datasets()? {
                for d in relational.data_ids_for_dataset(ds_id)? {
                    items.push((d, ds_id));
                }
            }
            items
        }
    };

    // Per data item: provenance partition + deletes.
    let mut deleted_node_ids: Vec<String> = Vec::new();
    let mut deleted_edge_keys: Vec<(String, String, String)> = Vec::new();
    let mut deleted_rel_names: HashSet<String> = HashSet::new();
    let mut surviving_rel_names: Option<HashSet<String>> = None;
    let mut vector_deletes: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (data_id, dataset_id) in &data_items {
        let ref_key = format!("source_ref:v1:{dataset_id}:{data_id}");
        report.data_items += 1;

        let nodes = graph.nodes_with_source_ref(&ref_key)?;
        let edges = graph.edges_with_source_ref(&ref_key)?;

        let mut unowned_nodes: Vec<String> = Vec::new();
        let mut surviving_nodes: Vec<String> = Vec::new();
        for n in &nodes {
            // Unowned iff this was the node's ONLY ref: after removal no other
            // data item claims it. Shared nodes are merely detached.
            if ref_count(&n.source_ref_keys, &ref_key) == total_refs(&n.source_ref_keys) {
                unowned_nodes.push(n.id.clone());
                deleted_node_ids.push(n.id.clone());
                let props: Option<Value> = n
                    .properties
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p).ok());
                for table in vector_tables_for(
                    n.type_.as_deref().unwrap_or(""),
                    props.as_ref().unwrap_or(&Value::Null),
                ) {
                    vector_deletes.entry(table).or_default().push(n.id.clone());
                }
            } else {
                surviving_nodes.push(n.id.clone());
            }
        }
        report.nodes_deleted += unowned_nodes.len();
        report.nodes_detached += surviving_nodes.len();

        let mut unowned_edge_keys: Vec<(String, String, String)> = Vec::new();
        let mut surviving_edge_keys: Vec<(String, String, String)> = Vec::new();
        for e in &edges {
            let key = (
                e.from_id.clone(),
                e.to_id.clone(),
                e.relationship_name.clone().unwrap_or_default(),
            );
            if ref_count(&e.source_ref_keys, &ref_key) == total_refs(&e.source_ref_keys) {
                unowned_edge_keys.push(key.clone());
                deleted_edge_keys.push(key.clone());
                if let Some(rel) = &e.relationship_name {
                    deleted_rel_names.insert(rel.clone());
                }
            } else {
                surviving_edge_keys.push(key);
            }
        }
        report.edges_deleted += unowned_edge_keys.len();

        // Detach refs from survivors, hard-delete unowned.
        graph.detach_source_ref_from_nodes(&ref_key, &surviving_nodes)?;
        // Edge deletes must run before node deletes (FK-less but logical order).
        graph.delete_edges(&unowned_edge_keys)?;
        graph.delete_nodes(&unowned_nodes)?;

        // Relational row for this data item.
        relational.delete_data(*data_id)?;

        // Remember surviving relationship names once, after all graph deletes.
        if surviving_rel_names.is_none() {
            surviving_rel_names = Some(graph.surviving_relationship_names()?.into_iter().collect());
        }
        for (from, to, rel) in &surviving_edge_keys {
            let _ = (from, to);
            if let Some(set) = surviving_rel_names.as_mut() {
                set.insert(rel.clone());
            }
        }
    }

    // Orphaned EdgeType vectors: relationship names that no surviving edge uses.
    if let Some(surviving) = surviving_rel_names {
        for rel in &deleted_rel_names {
            if !surviving.contains(rel) {
                let id = ids::edge_type_id(rel).to_string();
                vector_deletes
                    .entry("EdgeType_relationship_name".to_string())
                    .or_default()
                    .push(id);
            }
        }
    }

    // Vector deletes.
    for (table, ids) in &vector_deletes {
        report.vectors_deleted += vectors.delete_by_ids(table, ids).await?;
    }
    let _ = vector_writer;

    // Dataset-level cleanup: drop dataset rows (empty or explicit).
    match target {
        ForgetTarget::Dataset { dataset } => {
            relational.delete_dataset(dataset)?;
            report.datasets_deleted = 1;
        }
        ForgetTarget::Everything => {
            report.datasets_deleted = relational.delete_all_datasets()?;
        }
        ForgetTarget::DataItem { .. } => {}
    }

    Ok(report)
}

/// How many refs a `|a|b|` provenance list carries in total.
fn total_refs(raw: &Option<String>) -> usize {
    raw.as_deref()
        .map(|r| r.split('|').filter(|p| !p.is_empty()).count())
        .unwrap_or(0)
}

/// How many times `ref_key` appears in a `|a|b|` provenance list.
fn ref_count(raw: &Option<String>, ref_key: &str) -> usize {
    raw.as_deref()
        .map(|r| r.split('|').filter(|p| *p == ref_key).count())
        .unwrap_or(0)
}

/// CLI convenience: everything flag needs the all-stores context built anyway.
pub async fn default_context()
-> anyhow::Result<(GraphStore, RelationalStore, VectorStore, VectorWriter)> {
    let graph = GraphStore::open(&config::graph_db_path())?;
    let relational = RelationalStore::open(&config::relational_db_path())?;
    let db = lancedb::connect(&config::lancedb_path().to_string_lossy())
        .execute()
        .await?;
    let vectors = VectorStore::new(db.clone());
    let writer = VectorWriter::new(db);
    Ok((graph, relational, vectors, writer))
}
