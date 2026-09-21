//! SQLite property graph — the migration target for the `LBUG+` graph store.
//!
//! Schema mirrors the ladybug `Node` / `EDGE` tables 1:1 so the migration is a
//! straight copy, and K-hop traversal (what `GRAPH_COMPLETION` needs) is a
//! recursive CTE over `graph_edges`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

/// Column list shared by every node SELECT, so row indices stay in sync.
const NODE_COLUMNS: &str = "id, name, type, created_at, updated_at, properties, \
     source_ref_keys, source_dataset_ids, source_run_ids, source_run_refs";

const EDGE_COLUMNS: &str = "from_id, to_id, relationship_name, created_at, updated_at, properties, \
     source_ref_keys, source_dataset_ids, source_run_ids, source_run_refs";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS graph_nodes (
    id                 TEXT PRIMARY KEY,
    name               TEXT,
    type               TEXT,
    created_at         TEXT,
    updated_at         TEXT,
    properties         TEXT,
    source_ref_keys    TEXT,
    source_dataset_ids TEXT,
    source_run_ids     TEXT,
    source_run_refs    TEXT
);

CREATE TABLE IF NOT EXISTS graph_edges (
    from_id            TEXT NOT NULL,
    to_id              TEXT NOT NULL,
    relationship_name  TEXT,
    created_at         TEXT,
    updated_at         TEXT,
    properties         TEXT,
    source_ref_keys    TEXT,
    source_dataset_ids TEXT,
    source_run_ids     TEXT,
    source_run_refs    TEXT
);

CREATE INDEX IF NOT EXISTS idx_edges_from ON graph_edges (from_id);
CREATE INDEX IF NOT EXISTS idx_edges_to   ON graph_edges (to_id);
CREATE INDEX IF NOT EXISTS idx_edges_rel  ON graph_edges (relationship_name);
CREATE INDEX IF NOT EXISTS idx_nodes_type ON graph_nodes (type);

CREATE TABLE IF NOT EXISTS graph_metadata (
    key   TEXT PRIMARY KEY,
    value TEXT
);
"#;

/// A graph node. Field names match the `nodes.jsonl` produced by the migration
/// dumper, and the ladybug `Node` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphNode {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Raw JSON string (Kuzu JSON extension output), kept verbatim.
    #[serde(default)]
    pub properties: Option<String>,
    #[serde(default)]
    pub source_ref_keys: Option<String>,
    #[serde(default)]
    pub source_dataset_ids: Option<String>,
    #[serde(default)]
    pub source_run_ids: Option<String>,
    #[serde(default)]
    pub source_run_refs: Option<String>,
}

/// A directed edge between two [`GraphNode`]s.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphEdge {
    pub from_id: String,
    pub to_id: String,
    #[serde(default)]
    pub relationship_name: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub properties: Option<String>,
    #[serde(default)]
    pub source_ref_keys: Option<String>,
    #[serde(default)]
    pub source_dataset_ids: Option<String>,
    #[serde(default)]
    pub source_run_ids: Option<String>,
    #[serde(default)]
    pub source_run_refs: Option<String>,
}

/// A node with the traversal depth at which it was reached.
#[derive(Debug, Clone, PartialEq)]
pub struct ReachedNode {
    pub node: GraphNode,
    pub depth: u32,
}

fn row_to_node(row: &Row<'_>) -> rusqlite::Result<GraphNode> {
    Ok(GraphNode {
        id: row.get(0)?,
        name: row.get(1)?,
        type_: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        properties: row.get(5)?,
        source_ref_keys: row.get(6)?,
        source_dataset_ids: row.get(7)?,
        source_run_ids: row.get(8)?,
        source_run_refs: row.get(9)?,
    })
}

/// Prefix every column in a comma-separated list with a table alias.
///
/// `graph_nodes` and `graph_edges` share column names (`properties`,
/// `created_at`, `source_*`, ...), so any query joining them must qualify.
fn qualified(columns: &str, alias: &str) -> String {
    columns
        .split(", ")
        .map(|c| format!("{alias}.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Append `ref_key` to a `|a|b|` provenance list unless already present.
fn merge_ref(raw: Option<&str>, ref_key: &str) -> String {
    let kept: Vec<&str> = raw
        .unwrap_or("")
        .split('|')
        .filter(|p| !p.is_empty())
        .collect();
    if kept.contains(&ref_key) {
        return raw.unwrap_or("").to_string();
    }
    let mut all = kept;
    all.push(ref_key);
    format!("|{}|", all.join("|"))
}

/// Remove one entry from a `|a|b|` provenance list.
fn strip_ref(raw: &str, ref_key: &str) -> String {
    let kept: Vec<&str> = raw
        .split('|')
        .filter(|p| !p.is_empty() && *p != ref_key)
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("|{}|", kept.join("|"))
    }
}

fn row_to_edge(row: &Row<'_>) -> rusqlite::Result<GraphEdge> {
    Ok(GraphEdge {
        from_id: row.get(0)?,
        to_id: row.get(1)?,
        relationship_name: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        properties: row.get(5)?,
        source_ref_keys: row.get(6)?,
        source_dataset_ids: row.get(7)?,
        source_run_ids: row.get(8)?,
        source_run_refs: row.get(9)?,
    })
}

/// A SQLite-backed property graph.
///
/// The connection sits behind a `Mutex` because `rusqlite::Connection` is
/// `Send` but not `Sync`; without this the server's request futures would not
/// be `Send` and could not be spawned. The lock is only ever held for a
/// synchronous statement — never across an `await`.
pub struct GraphStore {
    conn: Mutex<Connection>,
}

impl GraphStore {
    /// Open (creating if absent) a graph at `path`, ensuring the schema exists.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating graph dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening graph db {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Open an in-memory graph (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL keeps concurrent readers happy while we hold the writer.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)
            .context("initialising graph schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the connection. A poisoned mutex still yields a usable connection:
    /// the earlier panic did not corrupt SQLite's state.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bulk-insert nodes inside a single transaction.
    pub fn insert_nodes(&self, nodes: &[GraphNode]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO graph_nodes (
                    id, name, type, created_at, updated_at, properties,
                    source_ref_keys, source_dataset_ids, source_run_ids, source_run_refs
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for n in nodes {
                stmt.execute(params![
                    n.id,
                    n.name,
                    n.type_,
                    n.created_at,
                    n.updated_at,
                    n.properties,
                    n.source_ref_keys,
                    n.source_dataset_ids,
                    n.source_run_ids,
                    n.source_run_refs,
                ])?;
            }
        }
        tx.commit()?;
        Ok(nodes.len())
    }

    /// Bulk-insert edges inside a single transaction.
    pub fn insert_edges(&self, edges: &[GraphEdge]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO graph_edges (
                    from_id, to_id, relationship_name, created_at, updated_at, properties,
                    source_ref_keys, source_dataset_ids, source_run_ids, source_run_refs
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for e in edges {
                stmt.execute(params![
                    e.from_id,
                    e.to_id,
                    e.relationship_name,
                    e.created_at,
                    e.updated_at,
                    e.properties,
                    e.source_ref_keys,
                    e.source_dataset_ids,
                    e.source_run_ids,
                    e.source_run_refs,
                ])?;
            }
        }
        tx.commit()?;
        Ok(edges.len())
    }

    /// Insert `(key, value)` metadata rows.
    pub fn insert_metadata(&self, rows: &[(String, String)]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO graph_metadata (key, value) VALUES (?1, ?2)",
            )?;
            for (k, v) in rows {
                stmt.execute(params![k, v])?;
            }
        }
        tx.commit()?;
        Ok(rows.len())
    }

    pub fn node_count(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT count(*) FROM graph_nodes", [], |r| r.get(0))?)
    }

    pub fn edge_count(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT count(*) FROM graph_edges", [], |r| r.get(0))?)
    }

    pub fn metadata(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT value FROM graph_metadata WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn node_by_id(&self, id: &str) -> Result<Option<GraphNode>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM graph_nodes WHERE id = ?1");
        let conn = self.conn();
        Ok(conn.query_row(&sql, params![id], row_to_node).optional()?)
    }

    /// All nodes of a given `type` (Entity, DocumentChunk, ...).
    pub fn nodes_by_type(&self, type_: &str) -> Result<Vec<GraphNode>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM graph_nodes WHERE type = ?1");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![type_], row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Node type histogram, descending by count.
    pub fn node_type_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT coalesce(type, '') AS t, count(*) AS c FROM graph_nodes
             GROUP BY t ORDER BY c DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Relationship histogram, descending by count.
    pub fn relationship_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT coalesce(relationship_name, '') AS r, count(*) AS c FROM graph_edges
             GROUP BY r ORDER BY c DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Merge a new provenance ref into existing node/edge rows (append if
    /// absent); rows not yet in the store keep the ref they were given.
    /// Shared entities accumulate refs, so `forget` can never hard-delete a
    /// node another data item still owns.
    pub fn merge_provenance(
        &self,
        nodes: &mut [GraphNode],
        edges: &mut [GraphEdge],
        ref_key: &str,
    ) {
        let conn = self.conn();
        for n in nodes.iter_mut() {
            if let Ok(Some(existing)) = conn
                .query_row(
                    "SELECT source_ref_keys FROM graph_nodes WHERE id = ?1",
                    params![n.id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
            {
                n.source_ref_keys = Some(merge_ref(existing.as_deref(), ref_key));
            }
        }
        for e in edges.iter_mut() {
            if let Ok(Some(existing)) = conn
                .query_row(
                    "SELECT source_ref_keys FROM graph_edges WHERE from_id = ?1 AND to_id = ?2 AND relationship_name = ?3",
                    params![e.from_id, e.to_id, e.relationship_name],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
            {
                e.source_ref_keys = Some(merge_ref(existing.as_deref(), ref_key));
            }
        }
    }

    /// Out-neighbours of a node, optionally restricted to relationship names.
    pub fn neighbours(&self, id: &str, rel_types: Option<&[String]>) -> Result<Vec<GraphNode>> {
        let node_cols = qualified(NODE_COLUMNS, "n");
        let sql = match rel_types {
            Some(rels) if !rels.is_empty() => format!(
                "SELECT {node_cols} FROM graph_nodes n
                 JOIN graph_edges e ON e.to_id = n.id
                 WHERE e.from_id = ?1
                   AND e.relationship_name IN (SELECT value FROM json_each(?2))"
            ),
            _ => format!(
                "SELECT {node_cols} FROM graph_nodes n
                 JOIN graph_edges e ON e.to_id = n.id
                 WHERE e.from_id = ?1"
            ),
        };
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = match rel_types {
            Some(rels) if !rels.is_empty() => {
                let json = serde_json::to_string(rels)?;
                stmt.query_map(params![id, json], row_to_node)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            _ => stmt
                .query_map(params![id], row_to_node)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Out-edges of a node, with all columns. Recall uses these to show the
    /// LLM the actual relationships between reached nodes.
    pub fn edges_from(&self, id: &str) -> Result<Vec<GraphEdge>> {
        let sql = format!("SELECT {EDGE_COLUMNS} FROM graph_edges WHERE from_id = ?1");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![id], row_to_edge)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Edges whose both endpoints are in `ids` (the subgraph induced by a
    /// traversal result).
    pub fn edges_within(&self, ids: &[String]) -> Result<Vec<GraphEdge>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(ids)?;
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM graph_edges
             WHERE from_id IN (SELECT value FROM json_each(?1))
               AND to_id   IN (SELECT value FROM json_each(?1))"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![ids_json], row_to_edge)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every node in the store (doctor/repair uses this).
    pub fn all_nodes(&self) -> Result<Vec<GraphNode>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM graph_nodes");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn all_node_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM graph_nodes")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Edges whose either endpoint is gone (ref-less leftovers of pre-fix
    /// writes; provenance-carrying edges are handled by source-ref search).
    pub fn edges_with_missing_endpoint(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT from_id, to_id, coalesce(relationship_name,'') FROM graph_edges e
             WHERE NOT EXISTS (SELECT 1 FROM graph_nodes n WHERE n.id = e.from_id)
                OR NOT EXISTS (SELECT 1 FROM graph_nodes n WHERE n.id = e.to_id)",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Collapse duplicate edge rows (same from/to/relationship) to one copy.
    pub fn dedup_edges(&self) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM graph_edges WHERE rowid NOT IN (
                 SELECT MIN(rowid) FROM graph_edges
                 GROUP BY from_id, to_id, relationship_name
             )",
            [],
        )?)
    }

    /// Delete edges by (from, to) pair, any relationship.
    pub fn delete_edges_by_endpoints(&self, keys: &[(String, String)]) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut n = 0;
        for (from, to) in keys {
            n += conn.execute(
                "DELETE FROM graph_edges WHERE from_id = ?1 AND to_id = ?2",
                params![from, to],
            )?;
        }
        Ok(n)
    }

    /// All nodes carrying a provenance ref (e.g. `source_ref:v1:<ds>:<data>`).
    pub fn nodes_with_source_ref(&self, ref_key: &str) -> Result<Vec<GraphNode>> {
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM graph_nodes
             WHERE source_ref_keys LIKE '%' || ?1 || '%'"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![ref_key], row_to_node)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All edges carrying a provenance ref.
    pub fn edges_with_source_ref(&self, ref_key: &str) -> Result<Vec<GraphEdge>> {
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM graph_edges
             WHERE source_ref_keys LIKE '%' || ?1 || '%'"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![ref_key], row_to_edge)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Drop one provenance ref from surviving nodes (idempotent detach).
    pub fn detach_source_ref_from_nodes(
        &self,
        ref_key: &str,
        node_ids: &[String],
    ) -> Result<usize> {
        if node_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut n = 0;
        for id in node_ids {
            let current: Option<String> = conn
                .query_row(
                    "SELECT source_ref_keys FROM graph_nodes WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(raw) = current {
                let stripped = strip_ref(&raw, ref_key);
                n += conn.execute(
                    "UPDATE graph_nodes SET source_ref_keys = ?2 WHERE id = ?1",
                    params![id, stripped],
                )?;
            }
        }
        Ok(n)
    }

    /// Hard-delete edges by id.
    pub fn delete_edges(&self, edge_keys: &[(String, String, String)]) -> Result<usize> {
        if edge_keys.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut n = 0;
        for (from, to, rel) in edge_keys {
            n += conn.execute(
                "DELETE FROM graph_edges WHERE from_id = ?1 AND to_id = ?2 AND relationship_name = ?3",
                params![from, to, rel],
            )?;
        }
        Ok(n)
    }

    /// Hard-delete nodes by id (their edges should be deleted first).
    pub fn delete_nodes(&self, node_ids: &[String]) -> Result<usize> {
        if node_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut n = 0;
        for id in node_ids {
            n += conn.execute("DELETE FROM graph_nodes WHERE id = ?1", params![id])?;
        }
        Ok(n)
    }

    /// Distinct relationship names still present on any edge.
    pub fn surviving_relationship_names(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT DISTINCT coalesce(relationship_name,'') FROM graph_edges")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// K-hop neighbourhood around `seeds`, following edges forward.
    ///
    /// The seeds are included at depth 0, direct neighbours at depth 1, and so
    /// on up to `max_hops`. This is the graph half of `GRAPH_COMPLETION`:
    /// vector search picks the seed entities, then we expand their
    /// neighbourhood to assemble context. Cycles terminate because recursion is
    /// bounded by `max_hops`, and a node reached at several depths keeps only
    /// its smallest one (`min(depth)`).
    pub fn traverse(
        &self,
        seeds: &[String],
        max_hops: u32,
        rel_types: Option<&[String]>,
    ) -> Result<Vec<ReachedNode>> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let seeds_json = serde_json::to_string(seeds)?;

        let rel_filter = rel_types
            .filter(|r| !r.is_empty())
            .map(|_| " AND e.relationship_name IN (SELECT value FROM json_each(?3))")
            .unwrap_or("");

        let node_cols = qualified(NODE_COLUMNS, "n");
        let sql = format!(
            "WITH RECURSIVE walk(id, depth) AS (
                 SELECT value, 0 FROM json_each(?1)
               UNION
                 SELECT e.to_id, w.depth + 1
                 FROM graph_edges e JOIN walk w ON e.from_id = w.id
                 WHERE w.depth < ?2{rel_filter}
             )
             SELECT DISTINCT {node_cols}, w2.depth
             FROM graph_nodes n
             JOIN (SELECT id, min(depth) AS depth FROM walk GROUP BY id) w2 ON w2.id = n.id"
        );

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &Row<'_>| -> rusqlite::Result<ReachedNode> {
            Ok(ReachedNode {
                node: row_to_node(row)?,
                depth: row.get(10)?,
            })
        };
        let rows = match rel_types.filter(|r| !r.is_empty()) {
            Some(rels) => {
                let rels_json = serde_json::to_string(rels)?;
                stmt.query_map(params![seeds_json, max_hops, rels_json], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => stmt
                .query_map(params![seeds_json, max_hops], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, type_: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            name: Some(name.into()),
            type_: Some(type_.into()),
            created_at: None,
            updated_at: None,
            properties: None,
            source_ref_keys: None,
            source_dataset_ids: None,
            source_run_ids: None,
            source_run_refs: None,
        }
    }

    fn edge(from: &str, to: &str, rel: &str) -> GraphEdge {
        GraphEdge {
            from_id: from.into(),
            to_id: to.into(),
            relationship_name: Some(rel.into()),
            created_at: None,
            updated_at: None,
            properties: None,
            source_ref_keys: None,
            source_dataset_ids: None,
            source_run_ids: None,
            source_run_refs: None,
        }
    }

    #[test]
    fn insert_and_count() {
        let g = GraphStore::open_in_memory().unwrap();
        g.insert_nodes(&[node("a", "A", "Entity"), node("b", "B", "Entity")])
            .unwrap();
        g.insert_edges(&[edge("a", "b", "is_a")]).unwrap();
        assert_eq!(g.node_count().unwrap(), 2);
        assert_eq!(g.edge_count().unwrap(), 1);
    }

    /// Traversal includes the seeds themselves at depth 0 — that is standard BFS
    /// neighbourhood semantics, and recall wants the matched entities in context.
    fn depths(reached: &[ReachedNode]) -> std::collections::BTreeMap<String, u32> {
        reached
            .iter()
            .map(|r| (r.node.id.clone(), r.depth))
            .collect()
    }

    #[test]
    fn traversal_reaches_k_hops_and_stops() {
        let g = GraphStore::open_in_memory().unwrap();
        g.insert_nodes(&[
            node("a", "A", "Entity"),
            node("b", "B", "Entity"),
            node("c", "C", "Entity"),
            node("d", "D", "Entity"),
        ])
        .unwrap();
        g.insert_edges(&[
            edge("a", "b", "is_a"),
            edge("b", "c", "is_a"),
            edge("c", "d", "is_a"),
        ])
        .unwrap();

        let one = depths(&g.traverse(&["a".into()], 1, None).unwrap());
        assert_eq!(one.get("a"), Some(&0));
        assert_eq!(one.get("b"), Some(&1));
        assert_eq!(one.get("c"), None, "2 hops away must be excluded");

        let three = depths(&g.traverse(&["a".into()], 3, None).unwrap());
        assert_eq!(three.get("d"), Some(&3));
        assert_eq!(three.len(), 4);
    }

    #[test]
    fn traversal_handles_cycles() {
        let g = GraphStore::open_in_memory().unwrap();
        g.insert_nodes(&[node("a", "A", "Entity"), node("b", "B", "Entity")])
            .unwrap();
        g.insert_edges(&[edge("a", "b", "x"), edge("b", "a", "x")])
            .unwrap();
        let reached = depths(&g.traverse(&["a".into()], 10, None).unwrap());
        assert_eq!(reached.len(), 2, "cycle must not loop forever or duplicate");
        assert_eq!(reached.get("a"), Some(&0), "seed keeps its depth-0 entry");
    }

    #[test]
    fn traversal_filters_relationship_types() {
        let g = GraphStore::open_in_memory().unwrap();
        g.insert_nodes(&[
            node("a", "A", "Entity"),
            node("b", "B", "Entity"),
            node("c", "C", "Entity"),
        ])
        .unwrap();
        g.insert_edges(&[edge("a", "b", "is_a"), edge("a", "c", "contains")])
            .unwrap();

        let only = depths(
            &g.traverse(&["a".into()], 1, Some(&["is_a".to_string()]))
                .unwrap(),
        );
        assert_eq!(only.get("b"), Some(&1));
        assert_eq!(only.get("c"), None);
    }

    #[test]
    fn metadata_roundtrip() {
        let g = GraphStore::open_in_memory().unwrap();
        g.insert_metadata(&[("provenance_version".into(), "1".into())])
            .unwrap();
        assert_eq!(
            g.metadata("provenance_version").unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(g.metadata("missing").unwrap(), None);
    }
}
