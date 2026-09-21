//! Benchmark harness for reflect-mem's core hot paths.
//!
//! Runs against an isolated copy of the data (default `/tmp/reflect-mem-bench`)
//! so the real memory store is never touched. Measures:
//!
//!   1. graph K-hop traversal — GRAPH_COMPLETION's graph half, pure local SQLite
//!   2. Ollama embedding latency
//!   3. LanceDB vector-search latency
//!
//! Usage:
//!   BENCH_ROOT=/tmp/reflect-mem-bench cargo run --release --bin bench

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use reflect_mem::embed::EmbeddingClient;
use reflect_mem::storage::graph::GraphStore;
use reflect_mem::storage::vector::VectorStore;

/// mean / p50 / p95 / p99 / max, in the input unit (milliseconds here).
fn stats(mut xs: Vec<f64>) -> (f64, f64, f64, f64, f64) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    let mean = xs.iter().sum::<f64>() / n as f64;
    let pct = |p: f64| xs[((n as f64 - 1.0) * p).round() as usize];
    (mean, pct(0.50), pct(0.95), pct(0.99), xs[n - 1])
}

fn report(name: &str, lat: Vec<f64>, extra: &str) {
    let n = lat.len();
    let (mean, p50, p95, p99, max) = stats(lat);
    println!(
        "{name:<32} n={n:>4}  mean={mean:>8.2}ms  p50={p50:>8.2}ms  p95={p95:>8.2}ms  p99={p99:>8.2}ms  max={max:>8.2}ms  {extra}"
    );
}

fn bench_traverse(store: &GraphStore, seeds: &[String], hops: u32, reps: usize) {
    let mut lat = Vec::with_capacity(seeds.len() * reps);
    for _ in 0..reps {
        for seed in seeds {
            let t = Instant::now();
            let _reached = store.traverse(std::slice::from_ref(seed), hops, None).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    // Report reached-node / edge scale separately (not timed).
    let mut nodes = 0usize;
    let mut edges = 0usize;
    for seed in seeds {
        let reached = store.traverse(std::slice::from_ref(seed), hops, None).unwrap();
        nodes += reached.len();
        let ids: Vec<String> = reached.iter().map(|r| r.node.id.clone()).collect();
        edges += store.edges_within(&ids).unwrap().len();
    }
    report(
        &format!("traverse hops={hops}"),
        lat,
        &format!(
            "avg nodes={:.1} edges={:.1}",
            nodes as f64 / seeds.len() as f64,
            edges as f64 / seeds.len() as f64
        ),
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::var("BENCH_ROOT").unwrap_or_else(|_| "/tmp/reflect-mem-bench".into());
    let graph_path = PathBuf::from(&root).join("graph.sqlite");
    let lancedb_path = PathBuf::from(&root).join("cognee.lancedb");

    println!("== reflect-mem benchmark ==");
    println!("BENCH_ROOT={root}");

    // ---- graph traversal (pure local) ----
    let store = GraphStore::open(&graph_path)?;
    println!("\n-- graph --");
    println!("nodes={} edges={}", store.node_count()?, store.edge_count()?);

    let entities = store.nodes_by_type("Entity")?;
    let step = (entities.len() / 50).max(1);
    let seeds: Vec<String> = entities
        .iter()
        .step_by(step)
        .take(50)
        .map(|n| n.id.clone())
        .collect();
    println!("sampled {} entity seeds", seeds.len());
    for hops in [1u32, 2, 3] {
        bench_traverse(&store, &seeds, hops, 3);
    }

    // ---- embedding (Ollama) ----
    let embedder = EmbeddingClient::from_env()?;
    println!("\n-- embedding ({}) --", embedder.model());
    let q = "用户的博客地址、域名和技术栈是什么？";
    let _ = embedder.embed(q).await?; // warm up
    {
        let mut lat = Vec::new();
        for _ in 0..10 {
            let t = Instant::now();
            embedder.embed(q).await?;
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        report("embed 1 text", lat, "");
    }

    // ---- vector search (LanceDB) ----
    let vectors = VectorStore::open(&lancedb_path).await?;
    println!("\n-- vector search --");
    let qvec = embedder.embed(q).await?;
    for table in ["Entity_name", "TextSummary_text", "DocumentChunk_text"] {
        let count = vectors.count_rows(table).await.unwrap_or(0);
        let _ = vectors.search(table, qvec.clone(), 5).await?; // warm up
        let mut lat = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            vectors.search(table, qvec.clone(), 5).await?;
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        report(
            &format!("search {table} k=5"),
            lat,
            &format!("rows={count}"),
        );
    }

    println!("\ndone");
    Ok(())
}
