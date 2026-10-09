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
//!
//! [`audit`] is the read-only counterpart: it cross-checks relational rows
//! against graph provenance and the legacy source-text files, and reports
//! without touching anything. It runs on its own (`doctor --audit`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::embed::EmbeddingClient;
use crate::ingest::ids;
use crate::storage::graph::{GraphNode, GraphStore};
use crate::storage::relational::RelationalStore;
use crate::storage::vector::VectorStore;
use crate::storage::vector_writer::{self, VectorRow};

#[derive(Debug, Default)]
pub struct DoctorReport {
    pub nodes_restored: usize,
    pub edges_deleted: usize,
    pub vectors_added: usize,
    pub vectors_purged: usize,
}

/// One `data` row the audit is complaining about.
#[derive(Debug, Clone)]
pub struct AuditItem {
    pub dataset: String,
    pub data_id: Uuid,
    pub content_hash: String,
    pub created_at: Option<String>,
}

/// Outcome of [`audit`].
#[derive(Debug, Default)]
pub struct AuditReport {
    pub data_items: usize,
    /// Rows whose provenance ref is stamped on at least one node/edge.
    pub with_graph_content: usize,
    /// Rows with no graph content at all: the write never landed, or its graph
    /// half was lost. Re-running `remember` with the same text repairs each of
    /// them in place (same `data_id`).
    pub without_graph_content: Vec<AuditItem>,
    /// Legacy `text_*.txt` files no `data` row refers to.
    pub orphan_text_files: Vec<PathBuf>,
    /// Rows whose legacy file path no longer exists on disk.
    pub missing_text_files: Vec<AuditItem>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.without_graph_content.is_empty()
            && self.orphan_text_files.is_empty()
            && self.missing_text_files.is_empty()
    }
}

/// Cross-check the stores without changing anything: relational rows ↔ graph
/// provenance ↔ legacy source-text files.
///
/// The judge for “this memory has content” is the **graph**, never
/// `pipeline_status`: rows have been observed carrying
/// `DATA_ITEM_PROCESSING_COMPLETED` while the graph held nothing at all for
/// them.
///
/// `text_dir` is passed in rather than read from [`crate::config`] so this stays
/// testable against a temporary directory.
pub fn audit(
    graph: &GraphStore,
    relational: &RelationalStore,
    text_dir: &Path,
) -> Result<AuditReport> {
    let mut report = AuditReport::default();

    // Every ref that has content, dash-normalized: the graph encodes dashed
    // uuids, the relational store keeps them dashless, and comparing the raw
    // strings would match nothing.
    let refs: HashSet<String> = graph
        .all_source_ref_keys()?
        .iter()
        .flat_map(|raw| raw.split('|'))
        .filter(|part| !part.is_empty())
        .map(|part| part.replace('-', ""))
        .collect();

    let items = relational.all_data_items()?;
    report.data_items = items.len();
    let mut referenced_files: HashSet<String> = HashSet::new();

    for row in &items {
        let data_id = row.data_id()?;
        let dataset_id = row.dataset_uuid()?;
        let ref_key = ids::source_ref_key(&dataset_id.to_string(), &data_id.to_string());
        if refs.contains(&ref_key.replace('-', "")) {
            report.with_graph_content += 1;
        } else {
            report.without_graph_content.push(AuditItem {
                dataset: row.dataset_name.clone(),
                data_id,
                content_hash: row.content_hash.clone().unwrap_or_default(),
                created_at: row.created_at.clone(),
            });
        }

        // Source-text bookkeeping, for rows that still point at a path. Rows
        // written since the file was retired carry the logical name
        // `text_<md5>`, which is not expected on disk.
        if let Some(name) = legacy_file_name(row.raw_data_location.as_deref()) {
            if text_dir.join(&name).exists() {
                referenced_files.insert(name);
            } else {
                report.missing_text_files.push(AuditItem {
                    dataset: row.dataset_name.clone(),
                    data_id,
                    content_hash: row.content_hash.clone().unwrap_or_default(),
                    created_at: row.created_at.clone(),
                });
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(text_dir) {
        let mut orphans: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                name.starts_with("text_") && name.ends_with(".txt")
            })
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                !referenced_files.contains(name)
            })
            .collect();
        orphans.sort();
        report.orphan_text_files = orphans;
    }

    Ok(report)
}

/// The source-text file a legacy `raw_data_location` points at, or `None` when
/// the row carries a logical name instead of a path.
fn legacy_file_name(location: Option<&str>) -> Option<String> {
    let location = location?;
    if !location.contains('/') {
        return None;
    }
    let name = location.rsplit('/').next()?.to_string();
    (name.starts_with("text_") && name.ends_with(".txt")).then_some(name)
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

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::storage::relational::{DataInsert, RelationalStore};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reflect-mem-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One `data` row. `location` mimics either a legacy path/URI or the
    /// logical name rows carry today.
    fn insert_row(store: &RelationalStore, dataset: &str, hash: &str, location: &str) -> Uuid {
        let dataset_id = store.ensure_dataset(dataset).unwrap();
        let data_id = Uuid::new_v4();
        store
            .insert_data(&DataInsert {
                id: data_id,
                name: &format!("text_{hash}"),
                mime_type: "text/plain",
                extension: "txt",
                raw_data_location: location,
                owner_id: store.default_user_id().unwrap(),
                dataset_id,
                content_hash: hash,
                raw_content_hash: hash,
                token_count: 1,
                data_size: 1,
                run_id: Uuid::new_v4(),
            })
            .unwrap();
        data_id
    }

    /// A node owned by one data item, i.e. what a landed write leaves behind.
    fn chunk_node(ref_key: &str) -> GraphNode {
        GraphNode {
            id: Uuid::new_v4().to_string(),
            name: None,
            type_: Some("DocumentChunk".into()),
            created_at: None,
            updated_at: None,
            properties: None,
            source_ref_keys: Some(format!("|{ref_key}|")),
            source_dataset_ids: None,
            source_run_ids: None,
            source_run_refs: None,
        }
    }

    fn stores(tag: &str) -> (PathBuf, GraphStore, RelationalStore) {
        let dir = temp_dir(tag);
        let graph = GraphStore::open_in_memory().unwrap();
        let relational = RelationalStore::open(&dir.join("reflect-mem.sqlite")).unwrap();
        (dir, graph, relational)
    }

    /// The case that motivated the audit: a row whose write never reached the
    /// graph. It must be reported, and the ref comparison must survive the
    /// dashed/dashless split between the two stores.
    #[test]
    fn reports_rows_without_graph_content_and_clears_once_they_land() {
        let (dir, graph, relational) = stores("audit-halfwritten");
        let text_dir = temp_dir("audit-halfwritten-text");
        let data_id = insert_row(&relational, "main_dataset", "hash1", "text_hash1");
        let dataset_id = relational.dataset_id("main_dataset").unwrap();

        let report = audit(&graph, &relational, &text_dir).unwrap();
        assert_eq!(report.data_items, 1);
        assert_eq!(report.with_graph_content, 0);
        assert_eq!(report.without_graph_content.len(), 1);
        assert_eq!(report.without_graph_content[0].data_id, data_id);
        assert!(!report.is_clean());

        // The same write, now landed: the graph stores dashed refs, the
        // relational store dashless ids.
        let ref_key = ids::source_ref_key(&dataset_id.to_string(), &data_id.to_string());
        graph.insert_nodes(&[chunk_node(&ref_key)]).unwrap();

        let report = audit(&graph, &relational, &text_dir).unwrap();
        assert_eq!(report.with_graph_content, 1);
        assert!(report.without_graph_content.is_empty());
        assert!(report.is_clean());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&text_dir).ok();
    }

    /// Legacy files are still reported so they can be cleaned up: leftovers
    /// with no row, and rows pointing at a path that is gone.
    #[test]
    fn reports_orphan_and_missing_source_text_files() {
        let (dir, graph, relational) = stores("audit-files");
        let text_dir = temp_dir("audit-files-text");

        insert_row(
            &relational,
            "main_dataset",
            "hash2",
            "file:///legacy/text_2.txt",
        );
        insert_row(&relational, "main_dataset", "hash3", "text_hash3");
        std::fs::write(text_dir.join("text_2.txt"), "still here").unwrap();
        std::fs::write(text_dir.join("text_orphan.txt"), "nobody owns me").unwrap();

        let report = audit(&graph, &relational, &text_dir).unwrap();
        assert_eq!(report.orphan_text_files.len(), 1);
        assert!(
            report.orphan_text_files[0]
                .to_string_lossy()
                .ends_with("text_orphan.txt")
        );
        assert!(report.missing_text_files.is_empty());

        // A row whose file is gone is reported; the row with a logical name is
        // not, because nothing is expected on disk for it.
        std::fs::remove_file(text_dir.join("text_2.txt")).unwrap();
        let report = audit(&graph, &relational, &text_dir).unwrap();
        assert_eq!(report.missing_text_files.len(), 1);
        assert_eq!(report.missing_text_files[0].dataset, "main_dataset");
        assert_eq!(report.missing_text_files[0].content_hash, "hash2");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&text_dir).ok();
    }

    #[test]
    fn legacy_file_name_ignores_logical_names() {
        assert_eq!(
            legacy_file_name(Some("file:///a/b/text_x.txt")).as_deref(),
            Some("text_x.txt")
        );
        assert_eq!(legacy_file_name(Some("text_x")), None);
        assert_eq!(legacy_file_name(None), None);
    }
}
