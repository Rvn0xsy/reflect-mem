//! LanceDB vector layer — reads the existing `cognee.lancedb` in place.
//!
//! Confirmed by risk R2 (`src/bin/spike_lancedb.rs`): the Rust crate opens the
//! store written by the Python SDK. Every table has the same shape:
//!
//! ```text
//! id: Utf8
//! vector: FixedSizeList(1024 x Float32)
//! payload: Struct(text, document_id, document_name, type, belongs_to_set, ...)
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray, StructArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

/// Table holding entity-name vectors — the seed source for `GRAPH_COMPLETION`.
pub const TABLE_ENTITY: &str = "Entity_name";
/// Document chunk vectors — the retrieval source for chunk/RAG-style recall.
pub const TABLE_DOCUMENT_CHUNK: &str = "DocumentChunk_text";
/// Document name vectors.
pub const TABLE_TEXT_DOCUMENT: &str = "TextDocument_name";
/// Pre-computed hierarchical summaries — the source for `SUMMARIES`.
pub const TABLE_TEXT_SUMMARY: &str = "TextSummary_text";
/// Entity-type vectors.
pub const TABLE_ENTITY_TYPE: &str = "EntityType_name";
/// Relationship-name vectors.
pub const TABLE_EDGE_TYPE: &str = "EdgeType_relationship_name";
/// Session QA vectors (the session cache's vector half).
pub const TABLE_SESSION_QA: &str = "SessionQAVector_text";

/// One vector-search result.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorHit {
    pub id: String,
    pub distance: f32,
    pub text: Option<String>,
    pub node_type: Option<String>,
    pub document_id: Option<String>,
    pub document_name: Option<String>,
}

fn struct_str(payload: &StructArray, name: &str) -> Option<String> {
    let col = payload.column_by_name(name)?;
    let arr = col.as_any().downcast_ref::<StringArray>()?;
    if arr.is_empty() || arr.is_null(0) {
        None
    } else {
        Some(arr.value(0).to_string())
    }
}

/// Read-only handle on the LanceDB vector store.
pub struct VectorStore {
    db: lancedb::Connection,
}

impl VectorStore {
    /// Wrap an existing connection (shared with the vector writer).
    pub fn new(db: lancedb::Connection) -> Self {
        Self { db }
    }

    /// Open the store at `path` (the `cognee.lancedb` directory).
    pub async fn open(path: &Path) -> Result<Self> {
        let db = lancedb::connect(&path.to_string_lossy())
            .execute()
            .await
            .with_context(|| format!("opening LanceDB at {}", path.display()))?;
        Ok(Self { db })
    }

    pub async fn table_names(&self) -> Result<Vec<String>> {
        Ok(self.db.table_names().execute().await?)
    }

    pub async fn count_rows(&self, table: &str) -> Result<usize> {
        let t = self.db.open_table(table).execute().await?;
        Ok(t.count_rows(None).await?)
    }

    /// Nearest-neighbour search in `table`.
    pub async fn search(&self, table: &str, query: Vec<f32>, k: usize) -> Result<Vec<VectorHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let t = self
            .db
            .open_table(table)
            .execute()
            .await
            .with_context(|| format!("opening vector table {table}"))?;

        let batches: Vec<_> = t
            .vector_search(query)?
            .limit(k)
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut hits = Vec::new();
        for batch in &batches {
            let ids = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .context("vector result is missing an `id` column")?;
            let dists = batch
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>());
            let payloads = batch
                .column_by_name("payload")
                .and_then(|c| c.as_any().downcast_ref::<StructArray>());

            for i in 0..batch.num_rows() {
                let payload = payloads.and_then(|p| {
                    if p.is_empty() {
                        None
                    } else {
                        // Slice the struct array so `column_by_name` on the
                        // result refers to row `i`.
                        Some(p.slice(i, 1))
                    }
                });
                hits.push(VectorHit {
                    id: ids.value(i).to_string(),
                    distance: dists.map(|d| d.value(i)).unwrap_or(f32::NAN),
                    text: payload.as_ref().and_then(|p| struct_str(p, "text")),
                    node_type: payload.as_ref().and_then(|p| struct_str(p, "type")),
                    document_id: payload.as_ref().and_then(|p| struct_str(p, "document_id")),
                    document_name: payload
                        .as_ref()
                        .and_then(|p| struct_str(p, "document_name")),
                });
            }
        }
        Ok(hits)
    }

    /// Pull the stored vector for an id from `table` (used to validate reuse).
    pub async fn vector_of(&self, table: &str, id: &str) -> Result<Option<Vec<f32>>> {
        let t = self.db.open_table(table).execute().await?;
        let batches: Vec<_> = t
            .query()
            .only_if(format!("id = '{}'", id.replace('\'', "''")))
            .limit(1)
            .execute()
            .await?
            .try_collect()
            .await?;
        for batch in &batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let col = batch
                .column_by_name("vector")
                .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>());
            let Some(col) = col else { continue };
            let flat = col.value(0);
            let arr = flat
                .as_any()
                .downcast_ref::<Float32Array>()
                .context("vector column is not Float32")?;
            return Ok(Some(arr.values().to_vec()));
        }
        Ok(None)
    }

    /// Assert a table is usable and every vector has the expected width.
    pub async fn validate(&self, table: &str, expected_dims: usize) -> Result<()> {
        let t = self.db.open_table(table).execute().await?;
        let schema = t.schema().await?;
        let field = schema
            .field_with_name("vector")
            .context("table has no `vector` column")?;
        let arrow_schema::DataType::FixedSizeList(_, size) = field.data_type() else {
            bail!("`vector` column is not a fixed-size list");
        };
        if *size as usize != expected_dims {
            bail!(
                "vector width mismatch: {table} stores {size}, embedding model produces {expected_dims}"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// The payload struct helpers index row 0 of a 1-row slice; the shape of
    /// that assumption is what the live spike (`spike_lancedb`) exercises.
    #[test]
    fn table_constants_are_distinct() {
        use super::*;
        let all = [
            TABLE_ENTITY,
            TABLE_DOCUMENT_CHUNK,
            TABLE_TEXT_DOCUMENT,
            TABLE_TEXT_SUMMARY,
            TABLE_ENTITY_TYPE,
            TABLE_EDGE_TYPE,
            TABLE_SESSION_QA,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
