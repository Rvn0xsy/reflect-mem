//! Data-root layout.
//!
//! By design (`docs/design.md` §4.2) we reuse the existing data root in place:
//! source text, `reflect-mem.sqlite` (relational) and `reflect-mem.lancedb`
//! (vectors) are read where they already are. Only the graph is migrated, into
//! `reflect-mem.graph.sqlite` beside them.
//!
//! The root comes from [`crate::settings`] (config file, then `DATA_ROOT`, then
//! `~/.agents/reflect-mem`).

use std::path::PathBuf;

/// Root of the memory data directory.
pub fn data_root() -> PathBuf {
    crate::settings::get().data_root.clone()
}

/// `<root>/system/databases`
pub fn databases_dir() -> PathBuf {
    data_root().join("system").join("databases")
}

/// Migrated property graph (new; written by `reflect-mem migrate`).
pub fn graph_db_path() -> PathBuf {
    databases_dir().join("reflect-mem.graph.sqlite")
}

/// Relational metadata (SQLite).
pub fn relational_db_path() -> PathBuf {
    databases_dir().join("reflect-mem.sqlite")
}

/// Vector store (LanceDB).
pub fn lancedb_path() -> PathBuf {
    databases_dir().join("reflect-mem.lancedb")
}

/// Existing content-addressed source text directory.
pub fn text_dir() -> PathBuf {
    data_root().join("data")
}

/// Session cache.
pub fn session_db_path() -> PathBuf {
    databases_dir().join("reflect-mem.session.sqlite")
}
