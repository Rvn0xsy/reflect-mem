use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reflect_mem::{config, migrate, storage::graph::GraphStore};

#[derive(Parser)]
#[command(
    name = "reflect-mem",
    version,
    about = "Rust memory-management MCP for cognee-shaped memory"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import a dumped cognee graph (migration/out/*.jsonl) into graph.sqlite.
    Migrate {
        /// Directory holding nodes.jsonl / edges.jsonl / metadata.jsonl.
        #[arg(long, default_value = "./migration/out")]
        input: PathBuf,
        /// Target graph db. Defaults to <DATA_ROOT>/system/databases/graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
    /// Show counts and histograms for the migrated graph.
    Inspect {
        /// Graph db to inspect. Defaults to <DATA_ROOT>/system/databases/graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
    /// Show the K-hop neighbourhood of one node (the graph half of GRAPH_COMPLETION).
    Traverse {
        /// Node id to start from.
        id: String,
        /// How many hops to expand.
        #[arg(long, default_value_t = 2)]
        hops: u32,
        /// Graph db. Defaults to <DATA_ROOT>/system/databases/graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Migrate { input, graph } => {
            let graph = graph.unwrap_or_else(config::graph_db_path);
            migrate::import(&input, &graph)?;
        }
        Commands::Inspect { graph } => {
            let graph = graph.unwrap_or_else(config::graph_db_path);
            let store = GraphStore::open(&graph)?;
            println!("graph: {}", graph.display());
            println!("nodes: {}", store.node_count()?);
            println!("edges: {}", store.edge_count()?);
            println!("\nnode types:");
            for (t, c) in store.node_type_counts()? {
                println!("  {c:>8}  {t}");
            }
            println!("\ntop relationships:");
            for (r, c) in store.relationship_counts()?.into_iter().take(20) {
                println!("  {c:>8}  {r}");
            }
        }
        Commands::Traverse { id, hops, graph } => {
            let graph = graph.unwrap_or_else(config::graph_db_path);
            let store = GraphStore::open(&graph)?;
            match store.node_by_id(&id)? {
                Some(n) => println!(
                    "seed: {} [{}] {}\n",
                    n.id,
                    n.type_.as_deref().unwrap_or("?"),
                    n.name.as_deref().unwrap_or("")
                ),
                None => println!("seed {id} not found in graph\n"),
            }
            let mut reached = store.traverse(std::slice::from_ref(&id), hops, None)?;
            reached.sort_by_key(|r| (r.depth, r.node.id.clone()));
            println!("reached {} nodes within {hops} hops:", reached.len());
            for r in &reached {
                println!(
                    "  d{}  {:<14} {:<40} {}",
                    r.depth,
                    r.node.type_.as_deref().unwrap_or("?"),
                    r.node.id,
                    r.node.name.as_deref().unwrap_or("")
                );
            }
            let ids: Vec<String> = reached.iter().map(|r| r.node.id.clone()).collect();
            let edges = store.edges_within(&ids)?;
            println!("\n{} edges inside that subgraph", edges.len());
            for e in edges.iter().take(15) {
                println!(
                    "  {} -[{}]-> {}",
                    e.from_id,
                    e.relationship_name.as_deref().unwrap_or("?"),
                    e.to_id
                );
            }
        }
    }
    Ok(())
}
