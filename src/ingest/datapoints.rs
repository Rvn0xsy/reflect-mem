//! DataPoint construction — the schema-fidelity core of the write path.
//!
//! Everything here mirrors what cognee's pipeline produces, field by field and
//! in the same JSON key order, so data written by reflect-mem is
//! indistinguishable from data written by the Python pipeline (design decision
//! D12: semantically compatible; ids byte-compatible per §13.2).

use serde_json::{Map, Value};
use uuid::Uuid;

use super::ids;

/// One graph node plus everything needed to store and embed it.
#[derive(Debug, Clone)]
pub struct Datapoint {
    pub id: Uuid,
    /// Graph `name` column (absent from `properties`, like cognee).
    pub name: Option<String>,
    pub type_: String,
    /// Full property map, key order matching cognee.
    pub properties: Map<String, Value>,
    /// `metadata.index_fields` — which properties were embedded.
    pub index_fields: Vec<String>,
    /// The text to embed (joined index-field values).
    pub embeddable_text: String,
}

/// One graph edge with cognee's canonical property set.
#[derive(Debug, Clone)]
pub struct Edge {
    pub from_id: String,
    pub to_id: String,
    pub relationship_name: String,
    pub properties: Map<String, Value>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// provenance columns are pipe-enclosed lists, e.g. `|<dataset_id>|`.
fn provenance_list(values: &[String]) -> String {
    format!("|{}|", values.join("|"))
}

/// Assemble one datapoint: base fields in cognee's order, then type extras.
///
/// `embeddable` is the text cognee would embed (the joined `index_fields`
/// values of the pydantic model — note `name` lives on the model, not in the
/// property map, so it cannot be derived from `extras`).
#[allow(clippy::too_many_arguments)]
pub fn datapoint(
    id: Uuid,
    type_: &str,
    name: Option<String>,
    rank: i64,
    index_fields: &[&str],
    embeddable: String,
    extras: Vec<(&str, Value)>,
    source_task: &str,
    source_content_hash: Option<String>,
    source_node_set: Option<String>,
) -> Datapoint {
    let ts = now_ms();
    let mut p = Map::new();
    p.insert("created_at".into(), Value::from(ts));
    p.insert("updated_at".into(), Value::from(ts));
    p.insert("ontology_valid".into(), Value::from(false));
    p.insert("ontology_uri".into(), Value::Null);
    p.insert("version".into(), Value::from(1));
    p.insert("topological_rank".into(), Value::from(rank));
    p.insert("valid_to".into(), Value::Null);
    p.insert(
        "metadata".into(),
        serde_json::json!({ "index_fields": index_fields }),
    );
    p.insert("belongs_to_set".into(), Value::Null);
    p.insert("source_pipeline".into(), Value::from("cognify_pipeline"));
    p.insert("source_task".into(), Value::from(source_task));
    p.insert(
        "source_node_set".into(),
        source_node_set.map(Value::from).unwrap_or(Value::Null),
    );
    p.insert("source_user".into(), Value::Null);
    p.insert(
        "source_content_hash".into(),
        source_content_hash.map(Value::from).unwrap_or(Value::Null),
    );
    p.insert("feedback_weight".into(), Value::from(0.5));
    p.insert("importance_weight".into(), Value::from(0.5));
    for (k, v) in extras {
        p.insert(k.into(), v);
    }

    let embeddable_text = embeddable;

    Datapoint {
        id,
        name,
        type_: type_.to_string(),
        properties: p,
        index_fields: index_fields.iter().map(|s| s.to_string()).collect(),
        embeddable_text,
    }
}

/// Convert a datapoint into the graph-node row shape.
pub fn to_graph_node(
    dp: &Datapoint,
    dataset_id: &str,
    data_id: &str,
    run_id: &str,
) -> super::super::storage::graph::GraphNode {
    use super::super::storage::graph::GraphNode;
    // The real store keeps TIMESTAMP columns in ISO-8601 with microseconds,
    // derived from the same ms epoch as the property fields.
    let created_ms = dp.properties["created_at"].as_i64().unwrap_or(0);
    let iso = iso_from_ms(created_ms);
    GraphNode {
        id: dp.id.to_string(),
        name: dp.name.clone(),
        type_: Some(dp.type_.clone()),
        created_at: Some(iso.clone()),
        updated_at: Some(iso),
        properties: Some(Value::Object(dp.properties.clone()).to_string()),
        // provenance fold-ins, matching the `|v1:…|` encoding observed in store
        source_ref_keys: Some(provenance_list(&[format!(
            "source_ref:v1:{dataset_id}:{data_id}"
        )])),
        source_dataset_ids: Some(provenance_list(&[dataset_id.replace('-', "")])),
        source_run_ids: Some(provenance_list(&[run_id.replace('-', "")])),
        source_run_refs: Some(provenance_list(&[format!(
            "source_run_ref:v1:{run_id}:source_ref:v1:{dataset_id}:{data_id}"
        )])),
    }
}

/// Edge property map, matching `_create_edge_properties` (cognee merges the
/// `Edge` model dump after the base keys, hence `extra`).
pub fn edge_properties(
    from_id: &str,
    to_id: &str,
    relationship_name: &str,
    extra: Option<Value>,
) -> Map<String, Value> {
    let updated_at = chrono_fmt_now();
    let mut p = Map::new();
    p.insert("source_node_id".into(), Value::from(from_id));
    p.insert("target_node_id".into(), Value::from(to_id));
    p.insert("relationship_name".into(), Value::from(relationship_name));
    p.insert("updated_at".into(), Value::from(updated_at));
    if let Some(Value::Object(extra)) = extra {
        for (k, v) in extra {
            if !v.is_null() {
                p.insert(k, v);
            }
        }
    }
    p.insert(
        "edge_object_id".into(),
        Value::from(ids::edge_object_id(from_id, relationship_name, to_id).to_string()),
    );
    p.insert("feedback_weight".into(), Value::from(0.5));
    p
}

/// `%Y-%m-%d %H:%M:%S` UTC, the exact format cognee stamps on edges.
pub fn chrono_fmt_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    format_utc_secs(secs)
}

/// ISO-8601 with microseconds from a ms epoch, e.g. `2026-08-24T06:38:55.718443`
/// — the format the real store keeps in `graph_nodes.created_at`.
pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let micros = ms.rem_euclid(1000) * 1000;
    let base = format_utc_secs(secs).replace(' ', "T");
    format!("{base}.{micros:06}")
}

/// Minimal `YYYY-MM-DD HH:MM:SS` from unix seconds (UTC, civil-from-days).
pub fn format_utc_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Base property keys, in cognee's pydantic declaration order (verified
    /// against the real store). `id` and `name` are separate columns.
    const BASE_KEYS: &[&str] = &[
        "created_at",
        "updated_at",
        "ontology_valid",
        "ontology_uri",
        "version",
        "topological_rank",
        "valid_to",
        "metadata",
        "belongs_to_set",
        "source_pipeline",
        "source_task",
        "source_node_set",
        "source_user",
        "source_content_hash",
        "feedback_weight",
        "importance_weight",
    ];

    #[test]
    fn timestamp_format_matches_cognee() {
        // 2026-08-24 06:38:55 UTC = 1787553535
        assert_eq!(format_utc_secs(1_787_553_535), "2026-08-24 06:38:55");
    }

    #[test]
    fn provenance_encoding_uses_pipes() {
        assert_eq!(provenance_list(&["a".into()]), "|a|");
        assert_eq!(provenance_list(&["a".into(), "b".into()]), "|a|b|");
    }

    #[test]
    fn entity_properties_key_order_and_content() {
        let dp = datapoint(
            Uuid::nil(),
            "Entity",
            Some("测试".into()),
            0,
            &["name"],
            "测试".into(),
            vec![
                ("description", Value::from("测试描述")),
                ("truth_alignment", Value::Null),
                ("truth_subspace_signature", Value::Null),
                ("truth_epoch", Value::Null),
            ],
            "extract_graph_from_data",
            None,
            None,
        );
        let keys: Vec<_> = dp.properties.keys().map(|k| k.as_str()).collect();
        let mut expected = BASE_KEYS.to_vec();
        expected.extend_from_slice(&[
            "description",
            "truth_alignment",
            "truth_subspace_signature",
            "truth_epoch",
        ]);
        assert_eq!(keys, expected);
        assert_eq!(dp.embeddable_text, "测试");
        assert_eq!(dp.properties["feedback_weight"], Value::from(0.5));
    }

    #[test]
    fn iso_timestamp_matches_store_format() {
        assert_eq!(iso_from_ms(1_787_553_535_718), "2026-08-24T06:38:55.718000");
    }

    #[test]
    fn edge_properties_shape() {
        let p = edge_properties(
            "aaa",
            "bbb",
            "contains",
            Some(json!({
                "relationship_type": "contains",
                "edge_text": "Document chunk mentions x: y",
            })),
        );
        let keys: Vec<_> = p.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "source_node_id",
                "target_node_id",
                "relationship_name",
                "updated_at",
                "relationship_type",
                "edge_text",
                "edge_object_id",
                "feedback_weight",
            ]
        );
    }
}
