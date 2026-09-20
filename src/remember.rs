//! `remember` — the permanent-memory write path.
//!
//! Mirrors cognee's cognify pipeline end to end (design doc §13.2):
//!
//! 1. store the source text (`data/text_<md5>.txt`) and the `data` row
//! 2. chunk the text
//! 3. per chunk: LLM-extract a KnowledgeGraph, LLM-summarize
//! 4. build DataPoints (TextDocument / DocumentChunk / TextSummary / Entity /
//!    EntityType) with deterministic ids
//! 5. write graph nodes + edges, then embeddings into the Lance tables
//!
//! Re-ingesting the same text is a no-op: the content hash short-circuits, and
//! entity ids are name-derived so re-extraction merges into existing nodes.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::config;
use crate::embed::EmbeddingClient;
use crate::ingest::chunk::{self, Chunk};
use crate::ingest::datapoints::{self, Datapoint};
use crate::ingest::extract::{self, ExtractedGraph};
use crate::ingest::ids;
use crate::llm::LlmClient;
use crate::storage::graph::{GraphEdge, GraphNode, GraphStore};
use crate::storage::relational::{DataInsert, RelationalStore};
use crate::storage::vector_writer::VectorWriter;

/// A not-yet-finalized edge: endpoints, relationship, properties.
pub type RawEdge = (String, String, String, Map<String, Value>);

/// Handles on the stores one write touches.
pub struct WriteContext<'a> {
    pub embedder: &'a EmbeddingClient,
    pub llm: &'a LlmClient,
    pub graph: &'a GraphStore,
    pub relational: &'a RelationalStore,
    pub vectors: &'a VectorWriter,
}

/// Progress sink so long writes are observable instead of looking hung.
pub trait Progress: Send + Sync {
    fn stage(&self, msg: &str);
}

/// Log stages to stderr with elapsed time.
pub struct StderrProgress {
    start: std::time::Instant,
}

impl StderrProgress {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for StderrProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress for StderrProgress {
    fn stage(&self, msg: &str) {
        eprintln!("[{:>7.1}s] {msg}", self.start.elapsed().as_secs_f32());
    }
}

/// What `remember` did.
#[derive(Debug)]
pub struct RememberReport {
    pub dataset: String,
    pub data_id: String,
    pub content_hash: String,
    pub chunks: usize,
    pub graph_nodes: usize,
    pub graph_edges: usize,
    pub vectors: usize,
    pub skipped_existing: bool,
}

fn md5_hex(data: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One sentence about this module: never store unless the content is new.
pub async fn remember(
    data: &str,
    dataset_name: &str,
    custom_prompt: Option<&str>,
    ctx: &WriteContext<'_>,
    progress: &dyn Progress,
) -> Result<RememberReport> {
    let WriteContext {
        embedder,
        llm,
        graph,
        relational,
        vectors,
    } = *ctx;
    if data.trim().is_empty() {
        bail!("nothing to remember: content is empty");
    }

    progress.stage("hashing content");
    let content_hash = md5_hex(data);

    // 1. idempotency: same content into the same dataset is a no-op
    progress.stage("checking for existing content");
    if let Some(existing) = relational.find_data_by_hash(dataset_name, &content_hash)? {
        return Ok(RememberReport {
            dataset: dataset_name.to_string(),
            data_id: existing,
            content_hash,
            chunks: 0,
            graph_nodes: 0,
            graph_edges: 0,
            vectors: 0,
            skipped_existing: true,
        });
    }

    // 2. persist source text + relational rows
    progress.stage("writing source text");
    let text_file = config::text_dir().join(format!("text_{content_hash}.txt"));
    std::fs::create_dir_all(config::text_dir())?;
    std::fs::write(&text_file, data)
        .with_context(|| format!("writing source text {}", text_file.display()))?;

    let data_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    progress.stage("ensuring dataset + data row");
    let dataset_id = relational.ensure_dataset(dataset_name)?;
    progress.stage("inserting data row");
    relational.insert_data(&DataInsert {
        id: data_id,
        name: &format!("text_{content_hash}"),
        mime_type: "text/plain",
        extension: "txt",
        raw_data_location: &format!(
            "file://{}",
            text_file
                .canonicalize()
                .unwrap_or(text_file.clone())
                .display()
        ),
        dataset_id,
        owner_id: relational.default_user_id()?,
        content_hash: &content_hash,
        raw_content_hash: &content_hash,
        token_count: chunk::estimate_tokens(data) as i64,
        data_size: data.len() as i64,
        run_id,
    })?;

    // 3. chunk
    progress.stage("chunking");
    let chunks = chunk::chunk_text(data, 1500);
    if chunks.is_empty() {
        bail!("content produced no chunks");
    }

    // 4. per-chunk extraction + summarization
    let document_id = Uuid::new_v4();
    let doc_name = format!("text_{content_hash}");
    let mut extracted: Vec<ExtractedGraph> = Vec::with_capacity(chunks.len());
    let mut summaries: Vec<String> = Vec::with_capacity(chunks.len());
    for (i, c) in chunks.iter().enumerate() {
        let t0 = std::time::Instant::now();
        let g = extract::extract_graph(llm, &c.text, custom_prompt).await?;
        progress.stage(&format!(
            "chunk {}/{}: extracted {} nodes / {} edges in {:.1}s",
            i + 1,
            chunks.len(),
            g.nodes.len(),
            g.edges.len(),
            t0.elapsed().as_secs_f32()
        ));
        let t1 = std::time::Instant::now();
        summaries.push(extract::summarize(llm, &c.text).await?);
        progress.stage(&format!(
            "chunk {}/{}: summarized in {:.1}s",
            i + 1,
            chunks.len(),
            t1.elapsed().as_secs_f32()
        ));
        extracted.push(g);
    }

    // 5. build datapoints
    let (dps, edges) = build_datapoints(
        &chunks,
        &extracted,
        &summaries,
        document_id,
        &doc_name,
        &content_hash,
        dataset_name,
    );

    progress.stage(&format!(
        "built {} datapoints, {} edges; writing graph",
        dps.len(),
        edges.len()
    ));
    let (mut nodes, mut edges) = to_rows(
        &dps,
        &edges,
        &dataset_id.to_string(),
        &data_id.to_string(),
        &run_id.to_string(),
    );
    // CRITICAL: shared entities already carry refs from earlier ingests.
    // Merging (not replacing) keeps forget from ever hard-deleting a node
    // another data item still owns.
    let new_ref = format!("source_ref:v1:{dataset_id}:{data_id}");
    graph.merge_provenance(&mut nodes, &mut edges, &new_ref);
    let before_nodes = graph.node_count()?;
    let before_edges = graph.edge_count()?;
    graph.insert_nodes(&nodes)?;
    graph.insert_edges(&edges)?;
    progress.stage("graph written");

    // 6. embeddings
    progress.stage("writing vector embeddings");
    let written = vectors.write(embedder, &dps).await?;

    relational.mark_data_processed(data_id, dataset_id, run_id)?;

    Ok(RememberReport {
        dataset: dataset_name.to_string(),
        data_id: data_id.to_string(),
        content_hash,
        chunks: chunks.len(),
        graph_nodes: (graph.node_count()? - before_nodes) as usize,
        graph_edges: (graph.edge_count()? - before_edges) as usize,
        vectors: written,
        skipped_existing: false,
    })
}

// ---------------------------------------------------------------------------
// DataPoint construction (§13.2 of the design doc)
// ---------------------------------------------------------------------------

fn build_datapoints(
    chunks: &[Chunk],
    graphs: &[ExtractedGraph],
    summaries: &[String],
    document_id: Uuid,
    doc_name: &str,
    content_hash: &str,
    dataset_name: &str,
) -> (Vec<Datapoint>, Vec<RawEdge>) {
    let mut dps: Vec<Datapoint> = Vec::new();
    let mut raw_edges: Vec<RawEdge> = Vec::new();

    // TextDocument
    dps.push(datapoints::datapoint(
        document_id,
        "TextDocument",
        Some(doc_name.to_string()),
        1,
        &["name"],
        doc_name.to_string(),
        vec![
            ("raw_data_location", json!(format!("file://{}", doc_name))),
            ("external_metadata", Value::Null),
            ("mime_type", json!("text/plain")),
        ],
        "extract_chunks_from_documents",
        Some(content_hash.to_string()),
        Some(dataset_name.to_string()),
    ));

    let mut entities: HashMap<String, Datapoint> = HashMap::new(); // entity name-key
    let mut entity_types: HashMap<String, Datapoint> = HashMap::new();

    for (chunk, graph) in chunks.iter().zip(graphs) {
        // DocumentChunk
        let chunk_dp = datapoints::datapoint(
            chunk.id,
            "DocumentChunk",
            None,
            2,
            &["text"],
            chunk.text.clone(),
            vec![
                ("text", json!(chunk.text)),
                ("chunk_size", json!(chunk.chunk_size as i64)),
                ("chunk_index", json!(chunk.chunk_index as i64)),
                ("cut_type", json!(chunk.cut_type)),
                ("document_id", json!(document_id.to_string())),
                ("document_name", json!(doc_name)),
                ("truth_alignment", Value::Null),
                ("truth_epoch", Value::Null),
            ],
            "extract_chunks_from_documents",
            Some(content_hash.to_string()),
            Some(dataset_name.to_string()),
        );
        dps.push(chunk_dp);

        // TextSummary (id = uuid5(chunk_id, "TextSummary"))
        let summary_text = summaries
            .get(chunk.chunk_index)
            .cloned()
            .unwrap_or_default();
        dps.push(datapoints::datapoint(
            ids::text_summary_id(chunk.id),
            "TextSummary",
            None,
            3,
            &["text"],
            summary_text.clone(),
            vec![
                ("text", json!(summary_text)),
                ("source_chunk_id", json!(chunk.id.to_string())),
                ("summarized_in", Value::Null),
                ("global_context_bucket_id", Value::Null),
            ],
            "extract_graph_and_summarize",
            Some(content_hash.to_string()),
            Some(dataset_name.to_string()),
        ));

        // chunk -[is_part_of]-> document
        raw_edges.push(edge_raw(chunk.id, document_id, "is_part_of", None));

        // TextSummary -[made_from]-> chunk
        raw_edges.push(edge_raw(
            ids::text_summary_id(chunk.id),
            chunk.id,
            "made_from",
            None,
        ));

        // entities from the LLM graph
        let mut chunk_entity_ids: Vec<(String, Uuid)> = Vec::new();
        for node in &graph.nodes {
            let name = if node.name.trim().is_empty() {
                node.id.clone()
            } else {
                node.name.clone()
            };
            let etype_raw = if node.type_.trim().is_empty() {
                "Entity".to_string()
            } else {
                node.type_.clone()
            };

            // EntityType (dedup by normalized type name)
            let et_id = ids::entity_type_id(&etype_raw);
            let et_key = et_id.to_string();
            if !entity_types.contains_key(&et_key) {
                let etype_name = ids::node_name(&etype_raw);
                entity_types.insert(
                    et_key.clone(),
                    datapoints::datapoint(
                        et_id,
                        "EntityType",
                        Some(etype_name.clone()),
                        0,
                        &["name"],
                        etype_name.clone(),
                        vec![("description", json!(etype_name)), ("relations", json!([]))],
                        "extract_graph_from_data",
                        None,
                        None,
                    ),
                );
            }

            // Entity (dedup by name-derived id)
            let e_id = ids::entity_id(&name);
            let e_key = e_id.to_string();
            let entity_name = ids::node_name(&name);
            if !entities.contains_key(&e_key) {
                entities.insert(
                    e_key.clone(),
                    datapoints::datapoint(
                        e_id,
                        "Entity",
                        Some(entity_name.clone()),
                        0,
                        &["name"],
                        entity_name.clone(),
                        vec![
                            ("description", json!(node.description)),
                            ("truth_alignment", Value::Null),
                            ("truth_subspace_signature", Value::Null),
                            ("truth_epoch", Value::Null),
                        ],
                        "extract_graph_from_data",
                        None,
                        None,
                    ),
                );
            }

            // Entity -[is_a]-> EntityType
            raw_edges.push(edge_raw(e_id, et_id, "is_a", None));
            // chunk -[contains]-> Entity (with cognee's edge_text shape)
            let contains_extra = if node.description.trim().is_empty() {
                None
            } else {
                Some(json!({
                    "relationship_type": "contains",
                    "edge_text": format!(
                        "Document chunk mentions {}: {}",
                        entity_name, node.description
                    ),
                }))
            };
            raw_edges.push(edge_raw(chunk.id, e_id, "contains", contains_extra));

            chunk_entity_ids.push((node.id.clone(), e_id));
        }

        // LLM edges between entities; cognee stamps relationship_type + edge_text
        for edge in &graph.edges {
            let Some((_, src)) = chunk_entity_ids
                .iter()
                .find(|(k, _)| *k == edge.source_node_id)
            else {
                continue;
            };
            let Some((_, tgt)) = chunk_entity_ids
                .iter()
                .find(|(k, _)| *k == edge.target_node_id)
            else {
                continue;
            };
            let rel = ids::edge_name(&edge.relationship_name);
            let extra = json!({
                "relationship_type": rel,
                "edge_text": edge.description,
            });
            raw_edges.push(edge_raw(*src, *tgt, &rel, Some(extra)));
        }
    }

    dps.extend(entity_types.into_values());
    dps.extend(entities.into_values());

    // dedup edges by (from, rel, to)
    let mut seen = std::collections::HashSet::new();
    raw_edges.retain(|e| seen.insert((e.0.clone(), e.2.clone(), e.1.clone())));

    (dps, raw_edges)
}

fn edge_raw(from: Uuid, to: Uuid, rel: &str, extra: Option<Value>) -> RawEdge {
    (
        from.to_string(),
        to.to_string(),
        rel.to_string(),
        datapoints::edge_properties(&from.to_string(), &to.to_string(), rel, extra),
    )
}

/// Default edge text when none was given: "{src} {rel sans _} {tgt}."
fn fallback_edge_text(from: &str, to: &str, rel: &str, names: &HashMap<String, String>) -> String {
    let label = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());
    format!("{} {} {}.", label(from), rel.replace('_', " "), label(to))
}

fn to_rows(
    dps: &[Datapoint],
    raw_edges: &[(String, String, String, Map<String, Value>)],
    dataset_id: &str,
    data_id: &str,
    run_id: &str,
) -> (Vec<GraphNode>, Vec<GraphEdge>) {
    let names: HashMap<String, String> = dps
        .iter()
        .filter_map(|d| d.name.clone().map(|n| (d.id.to_string(), n)))
        .collect();

    let nodes = dps
        .iter()
        .map(|d| datapoints::to_graph_node(d, dataset_id, data_id, run_id))
        .collect();

    let edges = raw_edges
        .iter()
        .map(|(from, to, rel, props)| {
            let mut props = props.clone();
            if props
                .get("edge_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
            {
                props.insert(
                    "edge_text".into(),
                    Value::from(fallback_edge_text(from, to, rel, &names)),
                );
            }
            // provenance stamped here; merge_provenance appends for rows
            // that already exist in the store
            GraphEdge {
                from_id: from.clone(),
                to_id: to.clone(),
                relationship_name: Some(rel.clone()),
                created_at: None,
                updated_at: None,
                properties: Some(Value::Object(props).to_string()),
                source_ref_keys: Some(format!("|source_ref:v1:{dataset_id}:{data_id}|")),
                source_dataset_ids: Some(format!("|{}|", dataset_id.replace('-', ""))),
                source_run_ids: Some(format!("|{}|", run_id.replace('-', ""))),
                source_run_refs: Some(format!(
                    "|source_run_ref:v1:{run_id}:source_ref:v1:{dataset_id}:{data_id}|"
                )),
            }
        })
        .collect();

    (nodes, edges)
}
