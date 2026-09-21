//! Appending datapoint vectors to the existing `cognee.lancedb` tables.
//!
//! Table naming follows the `{type}_{index_field}` convention, and each row is
//! `{id, vector, payload}` where `payload` is a fixed 22-field struct (the
//! union of DataPoint fields, verified against the live store). Existing tables
//! keep their on-disk schema; new rows are built against whatever the table
//! already declares, so we can never drift.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int64Array, ListArray, RecordBatch,
    StringArray, StructArray,
};
use arrow_schema::{DataType, Field, Fields, Schema};
use lancedb::table::AddDataMode;
use serde_json::Value;

use crate::embed::EmbeddingClient;
use crate::ingest::datapoints::Datapoint;

/// Canonical table schema used only when a table does not exist yet.
fn canonical_payload_fields() -> Fields {
    Fields::from(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("created_at", DataType::Int64, true),
        Field::new("updated_at", DataType::Int64, true),
        Field::new("ontology_valid", DataType::Boolean, true),
        Field::new("ontology_uri", DataType::Utf8, true),
        Field::new("version", DataType::Int64, true),
        Field::new("topological_rank", DataType::Int64, true),
        Field::new("valid_to", DataType::Int64, true),
        Field::new("type", DataType::Utf8, true),
        Field::new(
            "belongs_to_set",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        Field::new("source_pipeline", DataType::Utf8, true),
        Field::new("source_task", DataType::Utf8, true),
        Field::new("source_node_set", DataType::Utf8, true),
        Field::new("source_user", DataType::Utf8, true),
        Field::new("source_content_hash", DataType::Utf8, true),
        Field::new("feedback_weight", DataType::Float64, true),
        Field::new("importance_weight", DataType::Float64, true),
        Field::new("text", DataType::Utf8, true),
        Field::new("document_id", DataType::Utf8, true),
        Field::new("document_name", DataType::Utf8, true),
        Field::new("chunk_index", DataType::Int64, true),
        Field::new("source_chunk_id", DataType::Utf8, true),
    ])
}

fn canonical_schema(dims: usize) -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dims as i32,
            ),
            false,
        ),
        Field::new(
            "payload",
            DataType::Struct(canonical_payload_fields()),
            true,
        ),
    ])
}

/// A row to append: datapoint id, embedding, and its payload as JSON.
pub struct VectorRow {
    pub id: String,
    pub vector: Vec<f32>,
    pub payload: Value,
}

/// One column of the target schema populated from a JSON value.
fn array_for(field: &Field, values: &[Option<Value>]) -> anyhow::Result<Arc<dyn Array>> {
    match field.data_type() {
        DataType::Utf8 | DataType::LargeUtf8 => {
            let parsed: Vec<Option<String>> = values
                .iter()
                .map(|v| match v {
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            Ok(Arc::new(StringArray::from(parsed)))
        }
        DataType::Int64 => {
            let parsed: Vec<Option<i64>> = values
                .iter()
                .map(|v| v.as_ref().and_then(Value::as_i64))
                .collect();
            Ok(Arc::new(Int64Array::from(parsed)))
        }
        DataType::Boolean => {
            let parsed: Vec<Option<bool>> = values
                .iter()
                .map(|v| v.as_ref().and_then(Value::as_bool))
                .collect();
            Ok(Arc::new(BooleanArray::from(parsed)))
        }
        DataType::Float64 => {
            let parsed: Vec<Option<f64>> = values
                .iter()
                .map(|v| v.as_ref().and_then(Value::as_f64))
                .collect();
            Ok(Arc::new(Float64Array::from(parsed)))
        }
        DataType::List(element) => {
            let mut offsets = vec![0i32];
            let mut flat: Vec<Option<String>> = Vec::new();
            for v in values {
                if let Some(Value::Array(items)) = v {
                    for item in items {
                        flat.push(item.as_str().map(str::to_string));
                    }
                    offsets.push(offsets.last().unwrap() + items.len() as i32);
                } else {
                    offsets.push(*offsets.last().unwrap());
                }
            }
            let values_arr = StringArray::from(flat);
            let list = ListArray::new(
                element.clone(),
                arrow_buffer::OffsetBuffer::new(offsets.into()),
                Arc::new(values_arr),
                None,
            );
            Ok(Arc::new(list))
        }
        other => bail!(
            "unsupported payload column type for {}: {other}",
            field.name()
        ),
    }
}

/// Build a RecordBatch against `schema` from generic rows.
pub fn build_batch(schema: &Schema, rows: &[VectorRow]) -> Result<RecordBatch> {
    if schema.fields().len() != 3 {
        bail!("unexpected table shape: {} columns", schema.fields().len());
    }
    let id_arr = Arc::new(StringArray::from(
        rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
    ));
    // arrow 58: the constructor takes the CHILD field (Float32), and derives
    // FixedSizeList(child, size) itself.
    let (child_field, dims) = match schema.field(1).data_type() {
        DataType::FixedSizeList(child, size) => (child.clone(), *size as usize),
        other => bail!("vector column is not fixed-size: {other}"),
    };
    let mut flat: Vec<f32> = Vec::with_capacity(rows.len() * dims);
    for r in rows {
        if r.vector.len() != dims {
            bail!("vector dim mismatch: {} vs table's {dims}", r.vector.len());
        }
        flat.extend_from_slice(&r.vector);
    }
    let values_arr = Float32Array::from(flat);
    let vector_arr =
        arrow_array::FixedSizeListArray::new(child_field, dims as i32, Arc::new(values_arr), None);

    let payload_field = schema.field(2);
    let DataType::Struct(payload_fields) = payload_field.data_type() else {
        bail!("payload column is not a struct");
    };
    let mut columns: Vec<Arc<dyn Array>> = Vec::new();
    for f in payload_fields.iter() {
        let values: Vec<Option<Value>> = rows
            .iter()
            .map(|r| r.payload.get(f.name()).cloned())
            .collect();
        columns.push(array_for(f, &values)?);
    }
    let payload_arr = StructArray::new(payload_fields.clone(), columns, None);

    Ok(RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![id_arr, Arc::new(vector_arr), Arc::new(payload_arr)],
    )?)
}

/// Append pre-embedded rows to `table`, creating it with the canonical schema
/// when absent. Used by the writer and by `doctor` healing.
pub async fn append_rows(
    db: &lancedb::Connection,
    table: &str,
    dims: usize,
    rows: &[VectorRow],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let existing = db.open_table(table).execute().await.ok();
    let batch = match &existing {
        Some(t) => {
            let schema = t.schema().await?;
            build_batch(&schema, rows)?
        }
        None => {
            let schema = canonical_schema(dims);
            let empty = RecordBatch::new_empty(Arc::new(schema.clone()));
            db.create_table(table, empty)
                .execute()
                .await
                .with_context(|| format!("creating vector table {table}"))?;
            build_batch(&schema, rows)?
        }
    };
    let t = db.open_table(table).execute().await?;
    t.add(batch)
        .mode(AddDataMode::Append)
        .execute()
        .await
        .with_context(|| format!("appending {} rows to {table}", rows.len()))?;
    Ok(rows.len())
}

/// Writes datapoint embeddings, grouped per table.
pub struct VectorWriter {
    db: lancedb::Connection,
}

impl VectorWriter {
    pub fn new(db: lancedb::Connection) -> Self {
        Self { db }
    }

    /// Embed and write every datapoint into its `{type}_{index_field}` table.
    pub async fn write(&self, embedder: &EmbeddingClient, dps: &[Datapoint]) -> Result<usize> {
        if dps.is_empty() {
            return Ok(0);
        }
        let dims = embedder.dimensions();

        let mut groups: std::collections::BTreeMap<String, Vec<&Datapoint>> =
            std::collections::BTreeMap::new();
        for dp in dps {
            let field = dp
                .index_fields
                .first()
                .map(String::as_str)
                .unwrap_or("name");
            groups
                .entry(format!("{}_{}", dp.type_, field))
                .or_default()
                .push(dp);
        }

        let mut written = 0usize;
        for (table, group) in groups {
            let texts: Vec<String> = group.iter().map(|d| d.embeddable_text.clone()).collect();
            let vectors = embedder.embed_batch(&texts).await?;
            let rows: Vec<VectorRow> = group
                .iter()
                .zip(vectors)
                .map(|(dp, v)| VectorRow {
                    id: dp.id.to_string(),
                    vector: v,
                    payload: Value::Object(dp.properties.clone()),
                })
                .collect();
            written += append_rows(&self.db, &table, dims, &rows).await?;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Schema {
        canonical_schema(4)
    }

    #[test]
    fn builds_batch_against_canonical_schema() {
        let rows = vec![VectorRow {
            id: "abc".into(),
            vector: vec![0.1, 0.2, 0.3, 0.4],
            payload: json!({
                "id": "abc",
                "created_at": 1_787_553_535_718i64,
                "updated_at": 1_787_553_535_718i64,
                "ontology_valid": false,
                "version": 1,
                "topological_rank": 0,
                "type": "Entity",
                "feedback_weight": 0.5,
                "importance_weight": 0.5,
                "text": Value::Null,
            }),
        }];
        let batch = build_batch(&schema(), &rows).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(ids.value(0), "abc");
        let payload = batch
            .column(2)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        let version = payload
            .column_by_name("version")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(version.value(0), 1);
        // missing keys become null
        let text = payload
            .column_by_name("text")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(text.is_null(0));
    }

    #[test]
    fn rejects_dimension_mismatch() {
        let rows = vec![VectorRow {
            id: "abc".into(),
            vector: vec![0.1, 0.2],
            payload: json!({}),
        }];
        assert!(build_batch(&schema(), &rows).is_err());
    }
}
