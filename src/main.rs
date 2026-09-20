use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reflect_mem::embed::EmbeddingClient;
use reflect_mem::llm::LlmClient;
use reflect_mem::recall::{RecallOptions, SearchType};
use reflect_mem::storage::graph::GraphStore;
use reflect_mem::storage::vector::VectorStore;
use reflect_mem::{config, migrate, recall};

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
    /// List the reused LanceDB vector tables and their row counts.
    Vectors,
    /// Search memory and synthesise an answer.
    Recall {
        /// Natural-language question.
        query: String,
        /// SUMMARIES or GRAPH_COMPLETION.
        #[arg(long, default_value = "SUMMARIES")]
        search_type: String,
        /// Vector hits to retrieve.
        #[arg(long, default_value_t = 5)]
        top_k: usize,
        /// Graph expansion depth (GRAPH_COMPLETION only).
        #[arg(long, default_value_t = 2)]
        hops: u32,
        /// Graph db. Defaults to <DATA_ROOT>/system/databases/graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
    /// Serve the memory API over MCP.
    Mcp {
        /// Transport to serve on. Supported: stdio.
        #[arg(long, default_value = "stdio")]
        transport: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        Commands::Vectors => {
            let path = config::lancedb_path();
            let store = VectorStore::open(&path).await?;
            println!("lancedb: {}", path.display());
            for table in store.table_names().await? {
                println!("  {:>8}  {table}", store.count_rows(&table).await?);
            }
        }
        Commands::Recall {
            query,
            search_type,
            top_k,
            hops,
            graph,
        } => {
            let search_type = SearchType::parse(&search_type)?;
            let embedder = EmbeddingClient::from_env()?;
            let vectors = VectorStore::open(&config::lancedb_path()).await?;
            let graph = GraphStore::open(&graph.unwrap_or_else(config::graph_db_path))?;
            let llm = LlmClient::from_env()?;

            let opts = RecallOptions {
                search_type,
                top_k,
                max_hops: hops,
                system_prompt: None,
            };
            let out = recall::recall(&query, &opts, &embedder, &vectors, &graph, &llm).await?;

            eprintln!(
                "[{}] seeds={} nodes={} edges={} context={}ch",
                out.search_type.as_str(),
                out.seeds.len(),
                out.reached_nodes,
                out.reached_edges,
                out.context_chars
            );
            println!("{}", out.answer);
        }
        Commands::Mcp { transport } => match transport.as_str() {
            "stdio" => {
                // stdout is the protocol channel here: never print to it.
                eprintln!("reflect-mem MCP serving on stdio");
                reflect_mem::mcp::MemoryServer::from_env()
                    .await?
                    .serve_stdio()
                    .await?;
            }
            other => anyhow::bail!("unsupported transport {other:?}; expected stdio"),
        },
    }
    Ok(())
}
