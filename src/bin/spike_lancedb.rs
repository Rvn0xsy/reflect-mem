//! Spike: verify the `lancedb` Rust crate can open, read, and vector-search the
//! existing `cognee.lancedb` store written by the Python SDK.
//!
//! Design-doc risk R2. If this passes, the vector layer is reused in place with
//! zero migration: same files, same 1024-dim qwen3-embedding vectors.

use std::path::PathBuf;

use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_root = std::env::var("DATA_ROOT").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.agents/reflect-mem")
    });
    let db_path = PathBuf::from(&data_root).join("system/databases/cognee.lancedb");
    println!("Opening LanceDB at: {}", db_path.display());

    let db = lancedb::connect(db_path.to_str().unwrap())
        .execute()
        .await?;
    let names = db.table_names().execute().await?;
    println!("Found {} tables: {:?}\n", names.len(), names);

    let table = db.open_table("Entity_name").execute().await?;
    let count = table.count_rows(None).await?;
    println!("Entity_name has {count} rows");

    // 1. Pull one real row: its id, and its stored 1024-dim vector.
    let batches: Vec<_> = table
        .query()
        .limit(1)
        .execute()
        .await?
        .try_collect()
        .await?;
    let batch = batches.first().ok_or("empty batch")?;

    let source_id = batch
        .column_by_name("id")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .value(0)
        .to_string();

    let query_vec: Vec<f32> = batch
        .column_by_name("vector")
        .unwrap()
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap()
        .value(0)
        .as_any()
        .downcast_ref::<Float32Array>()
        .unwrap()
        .values()
        .to_vec();

    println!("Read row id={source_id} vector dims={}\n", query_vec.len());

    // 2. Use that vector as a query. Top-1 should be the row itself (distance 0).
    let batches: Vec<_> = table
        .vector_search(query_vec)?
        .limit(3)
        .execute()
        .await?
        .try_collect()
        .await?;

    println!("Nearest neighbours:");
    let mut top1 = String::new();
    for b in &batches {
        let ids = b
            .column_by_name("id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let dists = b
            .column_by_name("_distance")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>().cloned());
        for i in 0..b.num_rows() {
            let id = ids.value(i);
            if top1.is_empty() {
                top1 = id.to_string();
            }
            let d = dists.as_ref().map(|d| d.value(i));
            match d {
                Some(d) => println!("  id={id}  distance={d:.6}"),
                None => println!("  id={id}  distance=<none>"),
            }
        }
    }

    if top1 == source_id {
        println!("\nR2 OK: top-1 matches the seed row ({source_id}).");
    } else {
        println!("\nR2 WARN: top-1={top1} != seed={source_id} (still readable, order unexpected)");
    }
    Ok(())
}
