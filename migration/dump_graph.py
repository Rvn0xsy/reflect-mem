#!/usr/bin/env python3
"""One-time migration: dump the LBUG+ graph into JSONL for reflect-mem to import.

The graph lives in a proprietary ``LBUG+`` file written by the ``ladybug``
package (a compiled C++ Kuzu fork). Rust cannot read it, so we dump once here
and let the Rust importer load the result into ``graph.sqlite``.

This tool is intentionally throwaway: run it before retiring the Python install,
then the Python runtime is no longer needed.

Safety
------
Opening the file with a *different* ladybug version than the one that wrote it
can migrate the on-disk format in place. This script therefore:

  * defaults to ``--copy``, which snapshots the graph to a temp file and dumps
    from the snapshot, leaving the original untouched;
  * pins ``ladybug==0.19.0`` in ``requirements.txt`` (what the original pipeline pins).

Usage
-----
    python -m venv .venv && . .venv/bin/activate
    pip install -r requirements.txt
    python dump_graph.py --out ./out

Then, in Rust:  reflect-mem migrate --input ./out --graph <...>/graph.sqlite
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import shutil
import sys
import tempfile
from pathlib import Path

import ladybug

NODE_COLUMNS = [
    "id",
    "name",
    "type",
    "created_at",
    "updated_at",
    "properties",
    "source_ref_keys",
    "source_dataset_ids",
    "source_run_ids",
    "source_run_refs",
]

EDGE_COLUMNS = [
    "relationship_name",
    "created_at",
    "updated_at",
    "properties",
    "source_ref_keys",
    "source_dataset_ids",
    "source_run_ids",
    "source_run_refs",
]


def _jsonable(value):
    """Coerce a Kuzu value into something JSON can hold, preserving information."""
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, (dt.datetime, dt.date, dt.time)):
        return value.isoformat()
    if isinstance(value, (list, tuple)):
        return [_jsonable(v) for v in value]
    if isinstance(value, dict):
        return {str(k): _jsonable(v) for k, v in value.items()}
    return str(value)


def query(conn, cypher: str) -> list[dict]:
    """Run a Cypher query and return rows as a list of dicts."""
    result = conn.execute(cypher)
    return list(result.rows_as_dict())


def write_jsonl(path: Path, rows) -> int:
    """Write rows as JSONL, returning the count."""
    n = 0
    with path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps({k: _jsonable(v) for k, v in row.items()}, ensure_ascii=False))
            fh.write("\n")
            n += 1
    return n


def dump(graph_path: Path, out_dir: Path, use_copy: bool) -> None:
    work_path = graph_path
    tmpdir: str | None = None

    if use_copy:
        tmpdir = tempfile.mkdtemp(prefix="reflect_mem_dump_")
        work_path = Path(tmpdir) / "graph_snapshot"
        print(f"snapshotting {graph_path} -> {work_path}")
        shutil.copy2(graph_path, work_path)
    else:
        print(f"WARNING: opening the original in place: {graph_path}")

    try:
        db = ladybug.Database(str(work_path))
        try:
            print(f"storage version: {db.get_storage_version()}")
        except Exception as err:  # noqa: BLE001
            print(f"storage version: <unavailable: {err}>")
        conn = ladybug.Connection(db)

        out_dir.mkdir(parents=True, exist_ok=True)

        node_cols = ", ".join(f"n.{c} AS {c}" for c in NODE_COLUMNS)
        nodes = query(conn, f"MATCH (n:Node) RETURN {node_cols}")
        n_nodes = write_jsonl(out_dir / "nodes.jsonl", nodes)
        print(f"nodes: {n_nodes}")

        edge_cols = ", ".join(f"e.{c} AS {c}" for c in EDGE_COLUMNS)
        edges = query(
            conn,
            f"MATCH (a:Node)-[e:EDGE]->(b:Node) "
            f"RETURN a.id AS from_id, b.id AS to_id, {edge_cols}",
        )
        n_edges = write_jsonl(out_dir / "edges.jsonl", edges)
        print(f"edges: {n_edges}")

        meta = query(conn, "MATCH (m:GraphMetadata) RETURN m.key AS key, m.value AS value")
        n_meta = write_jsonl(out_dir / "metadata.jsonl", meta)
        print(f"metadata: {n_meta}")

        summary = {
            "nodes": n_nodes,
            "edges": n_edges,
            "metadata": n_meta,
            "storage_version": _storage_version(db),
            "source": str(graph_path),
        }
        (out_dir / "summary.json").write_text(
            json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        print(f"\nwrote {out_dir}/  ({n_nodes} nodes, {n_edges} edges, {n_meta} metadata)")

        expected = _expected_counts(graph_path)
        if expected is not None and expected != (n_nodes, n_edges):
            print(
                f"MISMATCH: expected {expected} from summary.json, dumped {(n_nodes, n_edges)}",
                file=sys.stderr,
            )
            sys.exit(1)
    finally:
        if tmpdir is not None:
            shutil.rmtree(tmpdir, ignore_errors=True)


def _storage_version(db) -> object:
    try:
        return db.get_storage_version()
    except Exception:  # noqa: BLE001
        return None


def _expected_counts(graph_path: Path):
    """If a previous summary.json sits next to the graph, use it to cross-check."""
    candidate = graph_path.parent / "summary.json"
    if not candidate.exists():
        return None
    try:
        data = json.loads(candidate.read_text(encoding="utf-8"))
        return data.get("nodes"), data.get("edges")
    except Exception:  # noqa: BLE001
        return None


def main() -> None:
    data_root = Path.home() / ".agents" / "reflect-mem"
    default_graph = data_root / "system" / "databases" / "cognee_graph_ladybug"

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--graph", type=Path, default=default_graph, help="path to LBUG+ graph")
    parser.add_argument("--out", type=Path, default=Path("./out"), help="output dir for JSONL")
    parser.add_argument(
        "--no-copy",
        dest="use_copy",
        action="store_false",
        help="dump the original in place instead of snapshotting it first",
    )
    args = parser.parse_args()

    if not args.graph.exists():
        parser.error(f"graph not found: {args.graph}")

    dump(args.graph, args.out.resolve(), args.use_copy)


if __name__ == "__main__":
    main()
