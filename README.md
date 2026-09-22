<div align="center">

# reflect-mem

**Long-term memory for AI agents — self-hosted, one binary, MCP-native.**

[![CI](https://github.com/Rvn0xsy/reflect-mem/actions/workflows/ci.yml/badge.svg)](https://github.com/Rvn0xsy/reflect-mem/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/Rvn0xsy/reflect-mem?sort=semver)](https://github.com/Rvn0xsy/reflect-mem/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![MCP](https://img.shields.io/badge/MCP-server-blue)](https://modelcontextprotocol.io)

**English** · [简体中文](README-zh.md)

</div>

`reflect-mem` gives an AI agent a memory that survives the conversation. It speaks the Model Context
Protocol, so any MCP-capable client can point at it and get three tools: `remember` to store a fact,
`recall` to answer a question from stored memory, and `forget` to delete it.

It is one Rust binary plus a directory of files you own — SQLite for the knowledge graph and metadata,
LanceDB for embeddings. No cloud service, no Python runtime, no external database.

Retrieval has two modes: **`SUMMARIES`** for fast lookups over summarized memory, and
**`GRAPH_COMPLETION`** for questions whose answer has to be stitched together from several memories.

---

## Contents

[Features](#features) · [Tools](#tools) · [How it works](#how-it-works) · [Quick start](#quick-start) ·
[Docker](#docker) · [CLI reference](#cli-reference) · [Configuration](#configuration) ·
[Benchmark](#benchmark) · [Data layout](#data-layout) · [Documentation](#documentation) ·
[License](#license)

---

## Features

- **Two retrieval modes.** `SUMMARIES` for fast lookups over pre-computed summaries; `GRAPH_COMPLETION`
  when the answer needs facts connected across several memories (multi-hop reasoning).
- **A graph, not just vectors.** Ingestion extracts entities and relations, so retrieval can walk the
  graph. K-hop expansion is a SQLite recursive CTE — **tens of microseconds** over ~18k edges
  (see [Benchmark](#benchmark)).
- **Self-hosted by default.** All state is files under `DATA_ROOT`. The only network calls go to the LLM
  and embedding endpoints you configure.
- **One static binary.** `curl | sh` and run. `rustls` instead of OpenSSL, SQLite bundled — no runtime
  dependencies.
- **Model-agnostic.** Any OpenAI-compatible chat endpoint for extraction and synthesis; any embedding
  backend — a local Ollama, or a hosted API such as OpenAI, Voyage or Cohere, with an API key.
- **Idempotent writes.** Entities, types and edges use deterministic `uuid5` ids, so re-ingesting the same
  text is a no-op and duplicates collapse.
- **stdio or streamable HTTP.** stdio for local agents; HTTP with bearer-token auth for remote or shared
  deployments.
- **Ships as a container.** Multi-stage `Dockerfile` (non-root, healthcheck) plus a `docker-compose.yml`
  that starts a token-protected HTTP endpoint in one command.

---

## Tools

| Tool | What it does |
|------|--------------|
| **`remember`** | Ingest text: chunk → extract entities and relations → write the knowledge graph and embeddings. Ingesting identical content twice is a no-op. |
| **`recall`** | Answer a question from stored memory. `search_type` picks `SUMMARIES` (default) or `GRAPH_COMPLETION`; `top_k` bounds how much is retrieved, `hops` sets graph expansion depth. |
| **`forget`** | Delete by `data_id` + `dataset`, by `dataset`, or `everything`. Provenance-scoped: entities still referenced by surviving memories are detached, not destroyed. |

```jsonc
// teach it a fact
{ "name": "remember", "arguments": { "data": "The user's blog lives at blog.example.com." } }

// ask for it back — fast vector lookup
{ "name": "recall", "arguments": { "query": "Where does the user's blog live?" } }

// a question that needs facts connected across hops
{ "name": "recall", "arguments": { "query": "Which platform hosts that blog?", "search_type": "GRAPH_COMPLETION", "hops": 2 } }

// delete it
{ "name": "forget", "arguments": { "dataset": "main_dataset" } }
```

`SUMMARIES` returns the closest pre-computed summaries — cheap, and usually sufficient. `GRAPH_COMPLETION`
seeds from the same vector index, then walks the knowledge graph out to `hops` and synthesises an answer;
that is what you want when no single memory holds the answer on its own.

---

## How it works

```
┌──────────────────────────────────────────────────────────────┐
│                     reflect-mem (Rust)                       │
│                                                              │
│   MCP layer (rmcp)   remember · recall · forget              │
│   transports         stdio · streamable HTTP + bearer token  │
│  ─────────────────────────────────────────────────────────── │
│   remember   chunk ─► entity extraction ─► graph + embeddings│
│   recall     SUMMARIES ──────────► vector search (fast)      │
│              GRAPH_COMPLETION ──► K-hop graph walk + synth.  │
│   forget     provenance-scoped, cross-store delete           │
│  ─────────────────────────────────────────────────────────── │
│   SQLite    knowledge graph · metadata · session cache        │
│   LanceDB   embeddings                                       │
└───────────────────────────────┬──────────────────────────────┘
                                │ HTTP
                     ┌──────────┴───────────┐
                     │ LLM (OpenAI-compat)  │  extraction + synthesis
                     │ Embeddings (Ollama)  │  any model
                     └──────────────────────┘
```

Ingestion produces three kinds of records — `DocumentChunk` nodes, the entities and relations extracted from
them, and a summary per chunk. Retrieval reads back whichever kind answers the question best.

All external dependencies are plain HTTP. Only the LLM and embedding endpoints are contacted; everything
else stays on disk.

---

## Quick start

### 1. Install

**Prebuilt binary** — Linux x86_64, macOS arm64/x86_64:

```bash
curl -fsSL https://raw.githubusercontent.com/Rvn0xsy/reflect-mem/main/install.sh | sh
```

Or fetch the archive directly (a `.sha256` is published next to each one):

```bash
# macOS (Apple Silicon) / (Intel)
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-aarch64-apple-darwin.tar.gz | tar xz
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-x86_64-apple-darwin.tar.gz | tar xz
sudo install -m 0755 reflect-mem /usr/local/bin/   # + LICENSE, NOTICE

# Linux (x86_64)
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo install -m 0755 reflect-mem /usr/local/bin/
```

Pin a version with `| sh -s -- v0.0.1`, or set `INSTALL_DIR=~/.local/bin`.

**From source** — any platform, needs a recent stable Rust (`edition 2024`):

```bash
cargo build --release
# binary at target/release/reflect-mem
```

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
model = "qwen3-embedding:0.6b" # any Ollama embedding model
dimensions = 1024              # must match that model
```

The data root (`~/.agents/reflect-mem` by default) is created on first run — schema and all — so there is
nothing to initialise by hand.

Environment variables still work and **override** the file, so containers/CI can inject secrets without
editing config:

```bash
LLM_API_KEY=sk-... reflect-mem mcp
```

Point at a different file with `--config /path/to/config.toml` or `$REFLECT_MEM_CONFIG`.

> `embedding.model` and `embedding.dimensions` have to stay stable once you have stored anything:
> vectors from different models are not comparable, so changing either makes existing embeddings
> unreadable.

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
reflect-mem remember --data "The user's blog lives at blog.example.com."
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
| `vectors` | List LanceDB tables and row counts |
| `recall <query>` | Search memory and synthesise an answer (`--search-type`, `--top-k`, `--hops`) |
| `remember --data <text>` | Store permanent memory (extract entities, write graph + vectors) |
| `forget --data-id <uuid>` \| `--dataset <name>` \| `--everything` | Delete memory |
| `doctor [--dump <dir>] [--heal-vectors]` | Repair the store: restore nodes from a dump and/or re-embed missing vectors |
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
| `embedding.endpoint` | `EMBEDDING_ENDPOINT` | `http://localhost:11434/api/embed` | Ollama `/api/embed` or any OpenAI-compatible `/v1/embeddings` (`host.docker.internal` auto-rewritten outside Docker) |
| `embedding.model` | `EMBEDDING_MODEL` | `qwen3-embedding:0.6b` | Any embedding model |
| `embedding.dimensions` | `EMBEDDING_DIMENSIONS` | `1024` | Must match that model |
| `embedding.api_key` | `EMBEDDING_API_KEY` | *(unset)* | Sent as `Authorization: Bearer`; most hosted APIs need it |
| `embedding.headers` | — | `{}` | Extra request headers, for providers that do not use Bearer |
| `mcp.transport` | `MCP_TRANSPORT` | `stdio` | `stdio` or `streamable-http` |
| `mcp.bind` | `MCP_BIND` | `127.0.0.1:8080` | Address for `streamable-http` |
| `mcp.token` | `MCP_TOKEN` | *(unset)* | Bearer token required on HTTP requests |
| — | `REFLECT_MEM_CONFIG` | `<data_root>/config.toml` | Config file path |

---

## Benchmark

A single-machine reference, not a guarantee. Environment: **Apple M5 Max (18 cores), macOS 27, arm64**,
release build with `rustc 1.96.1`, against a store of **6,365 nodes / 18,479 edges** and a 524 MB LanceDB
copied to `/tmp` so the live store is never touched.

Local storage and graph timings exclude process startup. End-to-end figures include the whole pipeline
(embedding → retrieval → LLM synthesis), so they are dominated by the model, not by reflect-mem.

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

### End-to-end recall (real LLM)

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

## Data layout

Everything lives under `DATA_ROOT` (`~/.agents/reflect-mem` by default). It is created on first run and is
safe to copy, back up or move as a whole:

```
<DATA_ROOT>/
  system/databases/
    reflect-mem.sqlite         # datasets, data rows, pipeline status
    reflect-mem.graph.sqlite   # knowledge graph (nodes / edges)
    reflect-mem.lancedb/       # embeddings
  data/text_<hash>.txt         # source text, content-addressed
```

Point `DATA_ROOT` at an existing directory to pick up where you left off — stores are opened in place, not
imported. Deleting the directory resets everything.

Because ids are derived deterministically from content, re-ingesting text you already stored is a no-op
rather than a duplicate.

---

## Documentation

- [`skills/reflect-mem-memory/SKILL.md`](skills/reflect-mem-memory/SKILL.md) — operator guide for the AI
  agent using these tools (remember / recall / forget semantics).

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
  llm.rs        OpenAI-compatible LLM client
  embed.rs      Ollama embedding client
  config.rs     data-root path layout
  storage/      graph / vector / relational / session-cache
  ingest/       chunking + entity extraction
skills/         operator skill for the AI agent
reflect-mem.example.toml      annotated config template
Dockerfile / .dockerignore    container image
.env.example / docker-compose.yml   compose deployment
```

## Contributing

Issues and pull requests are welcome. CI runs the same three checks:

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

One gotcha: a dependency's build script generates protobuf bindings, so `protoc` must be on the machine.
On Debian/Ubuntu that is `apt-get install protobuf-compiler libprotobuf-dev`; on macOS `brew install protobuf`.

## Acknowledgements

The store layout, graph schema, and ingestion pipeline are informed by
[cognee](https://www.cognee.ai), the reference implementation this project reimplements in Rust.
cognee is licensed under the [Apache License 2.0](https://github.com/topoteretes/cognee/blob/main/LICENSE)
(Copyright 2024 Topoteretes UG). `reflect-mem` is an independent implementation and does not bundle
cognee source code. See [NOTICE](NOTICE).

## License

MIT — see [`LICENSE`](LICENSE). Upstream attribution lives in [`NOTICE`](NOTICE).
