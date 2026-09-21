use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use reflect_mem::embed::EmbeddingClient;
use reflect_mem::llm::LlmClient;
use reflect_mem::recall::{RecallOptions, SearchType};
use reflect_mem::storage::graph::GraphStore;
use reflect_mem::storage::vector::VectorStore;
use reflect_mem::{config, migrate, recall};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "reflect-mem",
    version,
    about = "Rust long-term memory-management MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import a dumped graph (migration/out/*.jsonl) into reflect-mem.graph.sqlite.
    Migrate {
        /// Directory holding nodes.jsonl / edges.jsonl / metadata.jsonl.
        #[arg(long, default_value = "./migration/out")]
        input: PathBuf,
        /// Target graph db. Defaults to <DATA_ROOT>/system/databases/reflect-mem.graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
    /// Show counts and histograms for the migrated graph.
    Inspect {
        /// Graph db to inspect. Defaults to <DATA_ROOT>/system/databases/reflect-mem.graph.sqlite.
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
        /// Graph db. Defaults to <DATA_ROOT>/system/databases/reflect-mem.graph.sqlite.
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
        /// Graph db. Defaults to <DATA_ROOT>/system/databases/reflect-mem.graph.sqlite.
        #[arg(long)]
        graph: Option<PathBuf>,
    },
    /// Store text as permanent memory (extracts entities + writes graph/vectors).
    Remember {
        /// Text to store.
        #[arg(long)]
        data: Option<String>,
        /// Read the content from this file instead of --data.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Target dataset.
        #[arg(long, default_value = "main_dataset")]
        dataset: String,
        /// Optional hint steering entity extraction.
        #[arg(long)]
        custom_prompt: Option<String>,
    },
    /// Remove memory (graph + vectors + relational rows).
    Forget {
        /// Specific data item to remove (requires --dataset).
        #[arg(long)]
        data_id: Option<String>,
        /// Dataset to forget (all its data).
        #[arg(long)]
        dataset: Option<String>,
        /// Everything the user owns.
        #[arg(long, default_value_t = false)]
        everything: bool,
    },
    /// Verify and repair store consistency (needs a migration dump to restore).
    Doctor {
        /// Directory with nodes.jsonl (the migration dump) to restore from.
        #[arg(long, default_value = "./migration/out")]
        dump: PathBuf,
        #[arg(long, default_value_t = false)]
        heal_vectors: bool,
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
        Commands::Remember {
            data,
            file,
            dataset,
            custom_prompt,
        } => {
            let content = match (data, file) {
                (Some(d), _) => d,
                (None, Some(f)) => std::fs::read_to_string(&f)
                    .with_context(|| format!("reading {}", f.display()))?,
                _ => anyhow::bail!("provide --data or --file"),
            };
            let embedder = reflect_mem::embed::EmbeddingClient::from_env()?;
            let llm = LlmClient::from_env()?;
            let graph = GraphStore::open(&config::graph_db_path())?;
            let relational = reflect_mem::storage::relational::RelationalStore::open(
                &config::relational_db_path(),
            )?;
            let lance = lancedb::connect(&config::lancedb_path().to_string_lossy())
                .execute()
                .await?;
            let ctx = reflect_mem::remember::WriteContext {
                embedder: &embedder,
                llm: &llm,
                graph: &graph,
                relational: &relational,
                vectors: &reflect_mem::storage::vector_writer::VectorWriter::new(lance),
            };
            let report = reflect_mem::remember::remember(
                &content,
                &dataset,
                custom_prompt.as_deref(),
                &ctx,
                &reflect_mem::remember::StderrProgress::new(),
            )
            .await?;
            if report.skipped_existing {
                println!(
                    "already stored: dataset={} data_id={}",
                    report.dataset, report.data_id
                );
            } else {
                println!(
                    "stored: dataset={} chunks={} +nodes={} +edges={} vectors={} data_id={}",
                    report.dataset,
                    report.chunks,
                    report.graph_nodes,
                    report.graph_edges,
                    report.vectors,
                    report.data_id
                );
            }
        }
        Commands::Forget {
            data_id,
            dataset,
            everything,
        } => {
            let data_id = match data_id {
                Some(h) => Some(Uuid::parse_str(&h).context("data_id must be a UUID")?),
                None => None,
            };
            let target = reflect_mem::forget::resolve_target(data_id, dataset, None, everything)?;
            let (graph, relational, vectors, writer) =
                reflect_mem::forget::default_context().await?;
            let report =
                reflect_mem::forget::forget(&target, &graph, &relational, &vectors, &writer)
                    .await?;
            println!(
                "forgot: {} data item(s), -{} nodes ({} detached), -{} edges, -{} vectors, -{} datasets",
                report.data_items,
                report.nodes_deleted,
                report.nodes_detached,
                report.edges_deleted,
                report.vectors_deleted,
                report.datasets_deleted
            );
        }
        Commands::Doctor { dump, heal_vectors } => {
            let graph = GraphStore::open(&config::graph_db_path())?;
            let nodes_path = dump.join("nodes.jsonl");
            let dump_text = std::fs::read_to_string(&nodes_path)
                .with_context(|| format!("reading {}", nodes_path.display()))?;
            let dump_nodes: Vec<serde_json::Value> = dump_text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()
                .context("parsing dump")?;
            let report = reflect_mem::doctor::repair_graph_from_dump(&graph, &dump_nodes)?;
            println!(
                "repair: restored {} nodes, removed {} dangling edges",
                report.nodes_restored, report.edges_deleted
            );
            if heal_vectors {
                let embedder = reflect_mem::embed::EmbeddingClient::from_env()?;
                let vectors =
                    reflect_mem::storage::vector::VectorStore::open(&config::lancedb_path())
                        .await?;
                let healed = reflect_mem::doctor::heal_vectors(&graph, &vectors, &embedder).await?;
                println!(
                    "heal: +{} vectors, -{} stale vectors",
                    healed.vectors_added, healed.vectors_purged
                );
            }
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
