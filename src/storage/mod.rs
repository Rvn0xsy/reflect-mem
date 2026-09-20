//! Storage backends.
//!
//! - [`graph`] — SQLite property graph (migration target for `LBUG+`)
//! - (later) relational metadata reuse of `cognee_db`
//! - (later) vector layer over `cognee.lancedb`

pub mod graph;
