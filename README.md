<div align="center">

# reflect-mem

**Long-term memory for AI agents — a single Rust binary that serves `remember` / `recall` / `forget` over MCP.**

[![Rust 1.96](https://img.shields.io/badge/rust-1.96-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![edition 2024](https://img.shields.io/badge/edition-2024-orange)]()
[![MCP](https://img.shields.io/badge/MCP-ready-blue)]()
[![SQLite](https://img.shields.io/badge/SQLite-graph%2Frelational-003B57?logo=sqlite&logoColor=white)]()
[![LanceDB](https://img.shields.io/badge/LanceDB-vectors-8A2BE2)]()
[![version 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey)]()

**English** · [简体中文](README-zh.md)

</div>

`reflect-mem` is a long-term memory MCP server written in Rust. It exposes `remember` / `recall` / `forget` over
the Model Context Protocol so an AI agent can store and retrieve facts that survive across conversations — **without
a Python runtime, a venv, or a Kuzu C++ dependency**.

It drops in on top of an existing memory store: source text, the `reflect-mem.sqlite` relational layer and
the 520 MB `reflect-mem.lancedb` vector store are all opened **in place**, byte-for-byte. Only the private
`LBUG+` graph is migrated, into a SQLite property graph.

---

## Features

- **Single static binary.** No Python, no system OpenSSL (`rustls`), no Kuzu linkage. Every storage
  engine is either Rust-native or bundled SQLite.
- **In-place data reuse.** Opens `data/text_*.txt`, `reflect-mem.sqlite`, and `reflect-mem.lancedb` byte-for-byte in
  place — the embedding model is unchanged (`qwen3-embedding:0.6b`, 1024-dim), so existing vectors stay
  valid.
- **Sub-millisecond graph traversal.** `GRAPH_COMPLETION`'s K-hop expansion runs as a SQLite recursive
  CTE over ~6.3k nodes / ~18.5k edges and returns in **tens of microseconds** (see [Benchmark](#benchmark)).
- **Two retrieval modes.** `SUMMARIES` (fast vector search over hierarchical summaries) and
  `GRAPH_COMPLETION` (multi-hop reasoning over the knowledge graph).
- **Three memory tools.** `remember` (permanent ETL or session fast-path), `recall`, `forget`
  (provenance-scoped, cross-store deletion), plus `doctor` for consistency repair.
- **Deterministic IDs.** Entities, types and edges are `uuid5`-derived, so re-ingesting the same facts is
  idempotent and deduplicated.
- **Config file + two transports.** One TOML file (env vars override it) drives everything, and the MCP
  server speaks **stdio** or **streamable HTTP** with optional bearer-token auth.
- **Ships as a container.** Multi-stage `Dockerfile` (non-root, healthcheck) plus a `docker-compose.yml`
  that starts a token-protected HTTP endpoint in one command.

---

## How it works

```
┌──────────────────────────────────────────────────────────────┐
│                     reflect-mem (Rust)                       │
│                                                              │
│   MCP layer (rmcp)  ·  remember / recall / forget            │
│   transports: stdio · streamable HTTP + bearer token          │
│  ─────────────────────────────────────────────────────────── │
│   recall     SUMMARIES ──────────► LanceDB vector search      │
│              GRAPH_COMPLETION ──► vectors → SQLite K-hop CTE │
│   remember   chunk → MiniMax extraction → graph + vectors    │
│   forget     cross-store, provenance-scoped                   │
│  ─────────────────────────────────────────────────────────── │
│   SQLite   graph · relational · session-cache                 │
│   LanceDB  vectors (reused in place)                          │
└───────────────────────────────┬──────────────────────────────┘
                                │ HTTP only
                     ┌──────────┴───────────┐
                     │ MiniMax LLM          │  entity extraction + synthesis
                     │ Ollama embedding     │  qwen3-embedding:0.6b (1024d)
                     └──────────────────────┘
```

All external dependencies are plain HTTP. Nothing on the host needs to be installed beyond the binary
itself.

---

## Quick start

### 1. Build

```bash
cargo build --release
# binary at target/release/reflect-mem
```

Requires a recent stable Rust (`edition 2024`).

### 2. Configure

Everything can live in one TOML file (see [`reflect-mem.example.toml`](reflect-mem.example.toml)):

```bash
cp reflect-mem.example.toml ~/.agents/reflect-mem/config.toml
$EDITOR ~/.agents/reflect-mem/config.toml
```

```toml
[llm]
model = "MiniMax-M3"
api_key = "sk-..."
thinking = "disabled"          # skip chain-of-thought (faster)

[embedding]
model = "qwen3-embedding:0.6b" # must match the model that produced the vectors
dimensions = 1024
```

Environment variables still work and **override** the file, so containers/CI can inject secrets without
editing config:

```bash
LLM_API_KEY=sk-... reflect-mem mcp
```

Point at a different file with `--config /path/to/config.toml` or `$REFLECT_MEM_CONFIG`.

> `embedding.model` **must** match the model that produced the existing vectors, or the reused
> `reflect-mem.lancedb` becomes unusable.

### 3. Serve over MCP

**stdio** (local agents):

```bash
reflect-mem mcp --transport stdio
```

**Streamable HTTP + bearer token** (remote / shared deployments):

```bash
reflect-mem mcp --transport streamable-http --bind 127.0.0.1:8080 --token "$SECRET"
# -> reflect-mem MCP on http://127.0.0.1:8080/mcp (bearer token required)
```

Every request must then carry `Authorization: Bearer $SECRET`; requests without a valid token get
`401`. With no token configured the endpoint is **open** — only do that on loopback.

Register with an MCP client (stdio):

```json
{
  "mcpServers": {
    "reflect-mem": {
      "transport": "stdio",
      "command": "/path/to/reflect-mem",
      "args": ["mcp", "--config", "/home/me/.agents/reflect-mem/config.toml"]
    }
  }
}
```

or point it at the HTTP endpoint:

```json
{
  "mcpServers": {
    "reflect-mem": {
      "type": "http",
      "url": "http://127.0.0.1:8080/mcp",
      "headers": { "Authorization": "Bearer ${SECRET}" }
    }
  }
}
```

### 4. Try the CLI

```bash
reflect-mem recall "what did I say about my blog?" --search-type SUMMARIES
reflect-mem recall "what did I say about my blog?" --search-type GRAPH_COMPLETION --hops 2
reflect-mem remember --data "用户的博客主站是 blog.example.com。"
reflect-mem forget --dataset main_dataset
```

---

## Docker

The image runs the HTTP transport behind a bearer token, as a non-root user with a `/healthz` probe —
see [`Dockerfile`](Dockerfile) and [`docker-compose.yml`](docker-compose.yml).

```bash
cp .env.example .env          # then set LLM_API_KEY and MCP_TOKEN
# point DATA_DIR at an existing store (defaults to a fresh ./data)
docker compose up -d
docker compose logs -f
```

The endpoint is `http://127.0.0.1:8080/mcp`:

```bash
curl -X POST http://127.0.0.1:8080/mcp \
  -H 'Authorization: Bearer '"$MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

| | |
|---|---|
| Data | `DATA_DIR` (host) → `/data` (container). In the image `DATA_ROOT=/data`, so `config.toml` lives at `/data/config.toml`. |
| Auth | `MCP_TOKEN` is required; requests without it get `401`. `GET /healthz` stays open for probes. |
| Embeddings | Defaults to `host.docker.internal:11434` (an Ollama on the host). Or bundle one: `docker compose --profile ollama up -d`. |
| Port | Published on host loopback only (`127.0.0.1:${MCP_HOST_PORT}:8080`) — change deliberately. |

> The container runs as uid `10001` and needs **write** access to `DATA_DIR` (SQLite WAL, LanceDB).
> If it cannot write, add `user: "$(id -u):$(id -g)"` to the compose service.

Run without compose:

```bash
docker build -t reflect-mem .
docker run --rm -p 127.0.0.1:8080:8080 -v "$HOME/.agents/reflect-mem:/data" \
  -e LLM_API_KEY=... -e MCP_TOKEN=... reflect-mem
```

---

## CLI reference

| Command | Description |
|---------|-------------|
| `migrate --input <dir>` | Import a graph dump (`nodes.jsonl` / `edges.jsonl`) into `reflect-mem.graph.sqlite` |
| `inspect` | Node/edge counts and type histograms |
| `traverse <id> --hops N` | Print the K-hop neighbourhood of a node |
| `vectors` | List reused LanceDB tables and row counts |
| `recall <query>` | Search memory and synthesise an answer (`--search-type`, `--top-k`, `--hops`) |
| `remember --data <text>` | Store permanent memory (extract entities, write graph + vectors) |
| `forget --data-id <uuid>` \| `--dataset <name>` \| `--everything` | Delete memory |
| `doctor --dump <dir> [--heal-vectors]` | Verify and repair store consistency |
| `mcp --transport stdio` | Serve the memory API over stdio |
| `mcp --transport streamable-http --bind <addr> --token <t>` | Serve over HTTP at `/mcp`, requiring `Authorization: Bearer <t>` |

---

## Configuration

Resolution order, highest priority first:

1. CLI flags (`--config`, `mcp --bind` / `--token`)
2. environment variables
3. the TOML config file
4. built-in defaults

The config file defaults to `<data_root>/config.toml`; override the path with `--config` or
`REFLECT_MEM_CONFIG`. Start from [`reflect-mem.example.toml`](reflect-mem.example.toml).

| TOML key | Env var | Default | Description |
|----------|---------|---------|-------------|
| `data_root` | `DATA_ROOT` | `~/.agents/reflect-mem` | Data root (`system/databases/…` underneath) |
| `llm.endpoint` | `LLM_ENDPOINT` | `https://api.minimaxi.com/v1` | OpenAI-compatible chat-completions endpoint |
| `llm.model` | `LLM_MODEL` | `MiniMax-M2.7-highspeed` | Model id (an `openai/` prefix is stripped) |
| `llm.api_key` | `LLM_API_KEY` | — | Required |
| `llm.args` | `LLM_ARGS` | `{}` | Extra request fields (e.g. `{ reasoning_split = true }`) |
| `llm.thinking` | `LLM_THINKING` | *(unset)* | `disabled` skips chain-of-thought; `adaptive`/unset keeps it on |
| `embedding.endpoint` | `EMBEDDING_ENDPOINT` | `http://localhost:11434/api/embed` | Ollama embed endpoint (`host.docker.internal` auto-rewritten outside Docker) |
| `embedding.model` | `EMBEDDING_MODEL` | `qwen3-embedding:0.6b` | Must match the existing vectors |
| `embedding.dimensions` | `EMBEDDING_DIMENSIONS` | `1024` | Must match the existing vectors |
| `mcp.transport` | `MCP_TRANSPORT` | `stdio` | `stdio` or `streamable-http` |
| `mcp.bind` | `MCP_BIND` | `127.0.0.1:8080` | Address for `streamable-http` |
| `mcp.token` | `MCP_TOKEN` | *(unset)* | Bearer token required on HTTP requests |
| — | `REFLECT_MEM_CONFIG` | `<data_root>/config.toml` | Config file path |

---

## Benchmark

Measured on a release build against a real memory store (**6,365 nodes / 18,479 edges**, 524 MB LanceDB),
copied to `/tmp` so the live store is never touched. Local storage and graph numbers exclude process
startup; end-to-end numbers include the full pipeline (embedding → retrieval → LLM synthesis).

### Graph traversal (pure local, SQLite recursive CTE)

50 entity seeds × 3 rounds, `GRAPH_COMPLETION`'s graph half:

| hops | mean | p50 | p95 | p99 | avg. nodes | avg. edges |
|------|------|-----|-----|-----|-----------|-----------|
| 1 | **0.08 ms** | 0.02 ms | 0.25 ms | 0.30 ms | 2.8 | 1.9 |
| 2 | **0.05 ms** | 0.03 ms | 0.14 ms | 0.46 ms | 5.6 | 6.0 |
| 3 | **0.06 ms** | 0.03 ms | 0.26 ms | 0.47 ms | 10.8 | 16.0 |

K-hop traversal is **sub-millisecond** — the SQLite graph is not the bottleneck.

### Vector search (LanceDB, k=5, 20 rounds)

| Table | Rows | mean |
|-------|------|------|
| `Entity_name` | 5,125 | **13.5 ms** |
| `TextSummary_text` | 274 | **5.3 ms** |
| `DocumentChunk_text` | 274 | **4.9 ms** |

Embedding (`qwen3-embedding:0.6b`, 1024-dim): **15.4 ms** mean.

### End-to-end recall (real MiniMax LLM)

| Scenario | thinking on | thinking off | |
|----------|-------------|--------------|--|
| `SUMMARIES` | 2.75 s | **2.28 s** | −17% |
| `GRAPH_COMPLETION` | 3.26 s | **2.55 s** | −22% |

The dominant cost is LLM generation; local storage + graph + vectors together stay under ~20 ms.
`LLM_THINKING=disabled` shaves 17–22% off end-to-end latency with no measurable answer-quality loss.

### Reproduce

```bash
# 1. isolate a copy of the data (never touch the live store)
cp -R ~/.agents/reflect-mem/system/databases/reflect-mem.graph.sqlite /tmp/reflect-mem-bench/
cp -R ~/.agents/reflect-mem/system/databases/reflect-mem.lancedb /tmp/reflect-mem-bench/

# 2. run the local hot-path harness (graph + embedding + vector search)
BENCH_ROOT=/tmp/reflect-mem-bench cargo run --release --bin bench

# 3. end-to-end recall is measured via the CLI (needs LLM + Ollama)
```

---

## Architecture & docs

- [`docs/design.md`](docs/design.md) — full design: decisions, storage schema, ETL spec, migration, risks.
- [`skills/reflect-mem-memory/SKILL.md`](skills/reflect-mem-memory/SKILL.md) — operator guide for the AI
  agent using these tools (remember / recall / forget semantics).
- [`migration/`](migration/README.md) — one-shot `LBUG+` → JSONL graph dump (Python, run-once).

## Project layout

```
src/
  main.rs       CLI entry point
  mcp.rs        rmcp server: tools + stdio / streamable-HTTP + bearer auth
  settings.rs   config file (TOML) + env resolution
  recall.rs     read path (SUMMARIES / GRAPH_COMPLETION)
  remember.rs   write path (permanent-memory ETL)
  forget.rs     cross-store deletion
  doctor.rs     consistency check & repair
  migrate.rs    graph-dump importer
  llm.rs        MiniMax client (OpenAI-compatible)
  embed.rs      Ollama embedding client
  config.rs     data-root path layout
  storage/      graph / vector / relational / session-cache
  ingest/       chunking + entity extraction
docs/           design
migration/      one-shot graph migration tooling
skills/         operator skill for the AI agent
reflect-mem.example.toml      annotated config template
Dockerfile / .dockerignore    container image
.env.example / docker-compose.yml   compose deployment
```

## Acknowledgements

The store layout, graph schema, and ingestion pipeline are informed by
[cognee](https://www.cognee.ai), the reference implementation this project reimplements in Rust.
cognee is licensed under the [Apache License 2.0](https://github.com/topoteretes/cognee/blob/main/LICENSE)
(Copyright 2024 Topoteretes UG). `reflect-mem` is an independent implementation and does not bundle
cognee source code. See [NOTICE](NOTICE).

## License

Not yet licensed. All rights reserved until a license is chosen.
