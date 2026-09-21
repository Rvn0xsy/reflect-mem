//! Data-root layout.
//!
//! By design (`docs/design.md` §4.2) we reuse the existing cognee data root in
//! place: source text, `cognee_db` (relational) and `cognee.lancedb` (vectors)
//! are read where they already are. Only the graph is migrated, into a new
//! `graph.sqlite` that sits beside them.
//!
//! The root was renamed from `~/.agents/cognee-memory` to `~/.agents/reflect-mem`
//! on 2026-09-20; the default below tracks the new location.

use std::path::PathBuf;

/// Root of the memory data directory. Override with `DATA_ROOT`.
pub fn data_root() -> PathBuf {
    if let Ok(v) = std::env::var("DATA_ROOT")
        && !v.trim().is_empty()
    {
        return PathBuf::from(v);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".agents").join("reflect-mem")
}

/// `<root>/system/databases`
pub fn databases_dir() -> PathBuf {
    data_root().join("system").join("databases")
}

/// Migrated property graph (new; written by `reflect-mem migrate`).
pub fn graph_db_path() -> PathBuf {
    databases_dir().join("graph.sqlite")
}

/// Existing relational metadata (SQLite, reused in place).
pub fn relational_db_path() -> PathBuf {
    databases_dir().join("cognee_db")
}

/// Existing vector store (LanceDB, reused in place).
pub fn lancedb_path() -> PathBuf {
    databases_dir().join("cognee.lancedb")
}

/// Existing content-addressed source text directory.
pub fn text_dir() -> PathBuf {
    data_root().join("data")
}

/// Session cache (new; we do not touch the legacy `cache.db`).
pub fn session_db_path() -> PathBuf {
    databases_dir().join("reflect_session.db")
}
