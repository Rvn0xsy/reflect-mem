//! Relational metadata (`cognee_db`, SQLite) — read + minimal writes.
//!
//! Reused in place. Writes cover only what ingestion needs (datasets, data,
//! pipeline status); tenants/ACLs stay read-only in single-user mode.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;

const DEFAULT_USER_EMAIL: &str = "default_user@example.com";

/// Handle on the relational store.
pub struct RelationalStore {
    conn: Mutex<Connection>,
}

/// Columns for the `data` table, matching what cognee writes for a plain
/// text ingestion (verified against live rows).
pub struct DataInsert<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub mime_type: &'a str,
    pub extension: &'a str,
    pub raw_data_location: &'a str,
    pub owner_id: String,
    pub dataset_id: Uuid,
    pub content_hash: &'a str,
    pub raw_content_hash: &'a str,
    pub token_count: i64,
    pub data_size: i64,
    pub run_id: Uuid,
}

impl RelationalStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening relational db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the connection for a synchronous statement (never held across await).
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The single-user owner id (cognee's bootstrap user).
    pub fn default_user_id(&self) -> Result<String> {
        let conn = self.conn();
        Self::default_user_id_conn(&conn)
    }

    /// Inner helper taking an already-locked connection — the outer lock must
    /// NOT be re-entered, `std::sync::Mutex` is not reentrant (a prior version
    /// of `ensure_dataset` self-deadlocked exactly that way).
    fn default_user_id_conn(conn: &Connection) -> Result<String> {
        conn.query_row(
            "SELECT id FROM users WHERE email = ?1",
            params![DEFAULT_USER_EMAIL],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .context("default user row missing from cognee_db; run the Python setup once")
    }

    /// Existing dataset id (dashless hex, as cognee stores it) or create one.
    ///
    /// Takes the connection lock exactly once for the whole read-or-insert;
    /// every inner step uses the held guard, never `self.conn()` again.
    pub fn ensure_dataset(&self, name: &str) -> Result<Uuid> {
        let conn = self.conn();
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM datasets WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(hex) = existing {
            return parse_dashless(&hex);
        }
        let id = Uuid::new_v4();
        let owner = Self::default_user_id_conn(&conn)?;
        conn.execute(
            "INSERT INTO datasets (id, name, created_at, updated_at, owner_id, tenant_id)
             VALUES (?1, ?2, ?3, ?3, ?4, NULL)",
            params![dashless(id), name, sqlite_now_micros(), owner,],
        )?;
        Ok(id)
    }

    /// Find an existing data id by content hash inside a dataset (idempotency).
    pub fn find_data_by_hash(&self, dataset_name: &str, hash: &str) -> Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT d.id FROM data d JOIN datasets ds ON ds.id = d.dataset_id
                 WHERE d.content_hash = ?1 AND ds.name = ?2",
                params![hash, dataset_name],
                |r| r.get(0),
            )
            .optional()
            .context("looking up data by content hash")
    }

    /// Insert the `data` row for one ingestion.
    pub fn insert_data(&self, d: &DataInsert) -> Result<()> {
        let now = sqlite_now_micros();
        self.conn().execute(
            "INSERT INTO data (
                id, label, name, extension, mime_type,
                raw_data_location, owner_id, tenant_id, dataset_id,
                content_hash, raw_content_hash, external_metadata, system_metadata,
                node_set, pipeline_status, token_count, data_size,
                created_at, updated_at, last_accessed, importance_weight
             ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?8,
                       NULL, NULL, NULL, ?9, ?10, ?11, ?12, ?12, NULL, 0.5)",
            params![
                dashless(d.id),
                d.name,
                d.extension,
                d.mime_type,
                d.raw_data_location,
                d.owner_id,
                dashless(d.dataset_id),
                d.content_hash,
                // pipeline_status: run recorded, marked in-progress until cognify lands
                json_pipeline_status(&run_key(d.run_id), "DATA_ITEM_PROCESSING_STARTED"),
                d.token_count,
                d.data_size,
                now,
            ],
        )?;
        Ok(())
    }

    /// Resolve a dataset name to its (dashed) UUID.
    pub fn dataset_id(&self, name: &str) -> Result<Uuid> {
        let conn = self.conn();
        let hex: String = conn
            .query_row(
                "SELECT id FROM datasets WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .with_context(|| format!("dataset {name:?} not found"))?;
        parse_dashless(&hex)
    }

    /// All data ids (dashed UUIDs) inside a dataset.
    pub fn data_ids_for_dataset(&self, dataset_id: Uuid) -> Result<Vec<Uuid>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM data WHERE dataset_id = ?1")?;
        let rows = stmt.query_map(params![dashless(dataset_id)], |r| r.get::<_, String>(0))?;
        let hexes = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        hexes.iter().map(|h| parse_dashless(h)).collect()
    }

    /// Delete one `data` row.
    pub fn delete_data(&self, data_id: Uuid) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM data WHERE id = ?1", params![dashless(data_id)])?;
        Ok(())
    }

    /// Delete a dataset row (caller is responsible for its data).
    pub fn delete_dataset(&self, name: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM datasets WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// All datasets as (dashed uuid, name).
    pub fn all_datasets(&self) -> Result<Vec<(Uuid, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name FROM datasets")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let pairs = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        pairs
            .iter()
            .map(|(h, n)| Ok((parse_dashless(h)?, n.clone())))
            .collect()
    }

    /// Delete every dataset row (Everything mode); returns the count.
    pub fn delete_all_datasets(&self) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM datasets", [])?)
    }

    /// Flip `pipeline_status` to completed after the graph/vector writes land.
    pub fn mark_data_processed(&self, data_id: Uuid, dataset_id: Uuid, run_id: Uuid) -> Result<()> {
        let conn = self.conn();
        let status: Option<String> = conn
            .query_row(
                "SELECT pipeline_status FROM data WHERE id = ?1",
                params![dashless(data_id)],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let key = run_key(run_id);
        let mut value: Value = match status.as_deref() {
            Some(s) if !s.trim().is_empty() => {
                serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
            }
            _ => Value::Object(Default::default()),
        };
        // add_pipeline + cognify_pipeline both completed; cognee keys runs by
        // a per-pipeline run id but a single run id keeps the shape valid.
        for pipeline in ["add_pipeline", "cognify_pipeline"] {
            let obj = value
                .as_object_mut()
                .context("pipeline_status is not an object")?;
            let entry = obj
                .entry(pipeline)
                .or_insert_with(|| Value::Object(Default::default()));
            entry
                .as_object_mut()
                .context("pipeline entry is not an object")?
                .insert(key.clone(), Value::from("DATA_ITEM_PROCESSING_COMPLETED"));
        }
        conn.execute(
            "UPDATE data SET pipeline_status = ?2, updated_at = ?3 WHERE id = ?1",
            params![dashless(data_id), value.to_string(), sqlite_now_micros(),],
        )?;
        let _ = dataset_id;
        Ok(())
    }
}

fn run_key(run_id: Uuid) -> String {
    run_id.to_string() // pipeline_status keys keep the dashes (verified)
}

fn json_pipeline_status(run_key: &str, state: &str) -> String {
    serde_json::json!({
        "add_pipeline": { run_key: "DATA_ITEM_PROCESSING_COMPLETED" },
        "cognify_pipeline": { run_key: state },
    })
    .to_string()
}

/// cognee stores most relational ids as dashless hex.
fn dashless(id: Uuid) -> String {
    id.simple().to_string()
}

fn parse_dashless(hex: &str) -> Result<Uuid> {
    Uuid::parse_str(hex)
        .or_else(|_| Uuid::parse_str(&insert_dashes(hex)))
        .with_context(|| format!("parsing dataset id {hex:?}"))
}

fn insert_dashes(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `YYYY-MM-DD HH:MM:SS.ffffff` — SQLite DATETIME as SQLAlchemy writes it.
fn sqlite_now_micros() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
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
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}.{micros:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashless_roundtrip() {
        let id = Uuid::nil();
        assert_eq!(parse_dashless(&dashless(id)).unwrap(), id);
        assert_eq!(
            parse_dashless("58ab7a916767556dbe2c624d9c12128d").unwrap(),
            Uuid::parse_str("58ab7a91-6767-556d-be2c-624d9c12128d").unwrap()
        );
    }

    #[test]
    fn timestamp_matches_sqlite_datetime_string() {
        // 2026-08-24 06:28:29.770371 has 6-digit microseconds
        let s = sqlite_now_micros();
        assert_eq!(s.len(), 26);
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[19..20], ".");
    }

    #[test]
    fn pipeline_status_shape() {
        let v: Value = serde_json::from_str(&json_pipeline_status(
            "run-1",
            "DATA_ITEM_PROCESSING_STARTED",
        ))
        .unwrap();
        assert!(
            v["add_pipeline"]["run-1"]
                .as_str()
                .unwrap()
                .contains("COMPLETED")
        );
    }
}
