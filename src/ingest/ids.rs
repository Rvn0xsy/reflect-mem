//! Deterministic identity for graph entities — byte-for-byte compatible with
//! the legacy `DataPoint.id_for` / `generate_edge_object_id`.
//!
//! Verified against the real store: recomputing these ids for existing nodes
//! ("开发习惯（通用版）" → `4bd4116f-…`, EntityType "concept" → `d2a381fa-…`)
//! reproduces the ids the legacy store wrote. This is what makes re-ingesting the same
//! fact idempotent instead of duplicative.

use uuid::Uuid;

/// the legacy store hardcodes the OID namespace (`uuid.NAMESPACE_OID`).
const NAMESPACE_OID: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x12, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

/// the legacy `_normalize_identity_value` / `generate_node_id` normalisation:
/// lower-case, spaces to underscores, apostrophes stripped.
pub fn normalize(value: &str) -> String {
    value.to_lowercase().replace(' ', "_").replace('\'', "")
}

/// `uuid5(NAMESPACE_OID, name)`.
pub fn oid_uuid5(name: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE_OID, name.as_bytes())
}

/// `DataPoint.id_for(cls, *values)` = `uuid5(OID, "{cls}:{norm(v1)}|{norm(v2)}…")`.
pub fn id_for(class: &str, values: &[&str]) -> Uuid {
    let joined = values
        .iter()
        .map(|v| normalize(v))
        .collect::<Vec<_>>()
        .join("|");
    oid_uuid5(&format!("{class}:{joined}"))
}

pub fn entity_id(name: &str) -> Uuid {
    id_for("Entity", &[name])
}

pub fn entity_type_id(name: &str) -> Uuid {
    id_for("EntityType", &[name])
}

pub fn edge_type_id(relationship_name: &str) -> Uuid {
    id_for("EdgeType", &[relationship_name])
}

/// Stable id for one edge (source, target, relationship).
pub fn edge_object_id(source_id: &str, relationship_name: &str, target_id: &str) -> Uuid {
    let identifier = format!("{source_id}{relationship_name}{target_id}");
    oid_uuid5(&normalize(&identifier))
}

/// `generate_node_name`: lower-case, apostrophes stripped (no underscore swap).
pub fn node_name(name: &str) -> String {
    name.to_lowercase().replace('\'', "")
}

/// `generate_edge_name`: same as [`normalize`].
pub fn edge_name(name: &str) -> String {
    normalize(name)
}

/// `TextSummary` id: `uuid5(chunk_id, "TextSummary")`.
pub fn text_summary_id(chunk_id: Uuid) -> Uuid {
    Uuid::new_v5(&chunk_id, b"TextSummary")
}

/// Provenance ref stamped on every node/edge one write owns, and the key
/// `forget` deletes by.
///
/// `<dataset_id>` and `<data_id>` are **dashed** uuids here: the relational
/// store keeps ids dashless (`data.id`), the graph keeps them dashed in
/// `source_ref_keys`. Callers that read an id from the relational store must
/// parse it to [`Uuid`] before building the ref, or nothing will match.
pub fn source_ref_key(dataset_id: &str, data_id: &str) -> String {
    format!("source_ref:v1:{dataset_id}:{data_id}")
}

#[cfg(test)]
mod ref_tests {
    use super::*;

    #[test]
    fn source_ref_key_is_dashed() {
        let ds = Uuid::parse_str("58ab7a91-6767-556d-be2c-624d9c12128d").unwrap();
        let data = Uuid::parse_str("4f439f5f-5376-4f1b-901c-29ca54b5a5f1").unwrap();
        assert_eq!(
            source_ref_key(&ds.to_string(), &data.to_string()),
            "source_ref:v1:58ab7a91-6767-556d-be2c-624d9c12128d:4f439f5f-5376-4f1b-901c-29ca54b5a5f1"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These ids exist in the real store; they pin the whole identity scheme.
    #[test]
    fn reproduces_ids_legacy_wrote() {
        assert_eq!(
            entity_id("开发习惯（通用版）").to_string(),
            "4bd4116f-24c2-505b-8bbd-4d942097c02a"
        );
        assert_eq!(
            entity_id("测试与验证").to_string(),
            "6ae274a7-a13c-5a75-89dc-548c96637e10"
        );
        assert_eq!(
            entity_type_id("concept").to_string(),
            "d2a381fa-3658-5ca6-ada9-2109cfc9331d"
        );
    }

    #[test]
    fn normalization_matches_legacy() {
        assert_eq!(normalize("Alice Smith"), "alice_smith");
        assert_eq!(normalize("O'Brien's"), "obriens");
        assert_eq!(node_name("Alice's"), "alices");
        assert_eq!(edge_name("Works At"), "works_at");
    }

    #[test]
    fn edge_object_id_is_deterministic() {
        let a = "4bd4116f-24c2-505b-8bbd-4d942097c02a";
        let b = "d2a381fa-3658-5ca6-ada9-2109cfc9331d";
        assert_eq!(
            edge_object_id(a, "is_a", b).to_string(),
            edge_object_id(a, "is_a", b).to_string()
        );
    }
}
