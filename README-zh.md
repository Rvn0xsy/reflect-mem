<div align="center">

# reflect-mem

**给 AI Agent 的长期记忆 —— 自托管、单二进制、原生 MCP。**

[![CI](https://github.com/Rvn0xsy/reflect-mem/actions/workflows/ci.yml/badge.svg)](https://github.com/Rvn0xsy/reflect-mem/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/Rvn0xsy/reflect-mem?sort=semver)](https://github.com/Rvn0xsy/reflect-mem/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.96%2B-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![MCP](https://img.shields.io/badge/MCP-server-blue)](https://modelcontextprotocol.io)

[English](README.md) · **简体中文**

</div>

`reflect-mem` 给 AI Agent 一份跨会话存活的记忆。它说 Model Context Protocol，任何支持 MCP 的客户端
接上就能拿到三个工具：`remember` 存下事实，`recall` 从已存内容里回答问题，`forget` 删掉它。

它就是**一个 Rust 二进制 + 一个你自己掌控的目录**：知识图谱和元数据用 SQLite，向量用 LanceDB。
不依赖云服务、不需要 Python 运行时、也没有外部数据库。

检索有两种模式：**`SUMMARIES`** 面向已生成摘要的快速查找，**`GRAPH_COMPLETION`** 面向那些需要把
多条记忆拼起来才能回答的问题。

---

## 目录

[特性](#特性) · [工具](#工具) · [工作原理](#工作原理) · [快速开始](#快速开始) ·
[Docker](#docker) · [CLI 参考](#cli-参考) · [配置](#配置) · [性能测试](#性能测试) ·
[数据布局](#数据布局) · [文档](#文档) · [许可证](#许可证)

---

## 特性

- **两种检索模式。** `SUMMARIES` 对已生成的摘要做快速查找；`GRAPH_COMPLETION` 适合答案需要跨多条
  记忆串联的情况（多跳推理）。
- **不只是向量，还有图。** 写入时会抽取实体与关系，所以检索可以沿着图走。K 跳扩展是 SQLite
  递归 CTE —— 在约 18k 条边上只要**几十微秒**（见[性能测试](#性能测试)）。
- **默认自托管。** 所有状态都是 `DATA_ROOT` 下的文件。唯一的网络请求发往你自己配置的 LLM 与
  embedding 端点。
- **单一静态二进制。** `curl | sh` 就能用。用 `rustls` 而非 OpenSSL，SQLite 内置 —— 无运行时依赖。
- **模型无关。** 抽取与综合用任意 OpenAI 兼容端点；embedding 用任意 Ollama 模型。
- **写入幂等。** 实体、类型、边都用确定性的 `uuid5` id，所以重复写入同一段文本是空操作，不会产生
  重复。
- **stdio 或 streamable HTTP。** 本地 Agent 走 stdio；远程/共享部署走 HTTP + Bearer Token 认证。
- **提供容器镜像。** 多阶段 `Dockerfile`（非 root、带 healthcheck）加一个 `docker-compose.yml`，
  一条命令拉起带 Token 保护的 HTTP 端点。

---

## 工具

| 工具 | 作用 |
|------|------|
| **`remember`** | 写入文本：切块 → 抽取实体与关系 → 写知识图谱和向量。同一内容写两次是空操作。 |
| **`recall`** | 用已存记忆回答问题。`search_type` 选 `SUMMARIES`（默认）或 `GRAPH_COMPLETION`；`top_k` 限制取回量，`hops` 控制图扩展深度。 |
| **`forget`** | 按 `data_id` + `dataset`、按 `dataset`、或 `everything` 删除。按溯源分区：仍被其他记忆引用的实体只解绑，不销毁。 |

```jsonc
// 存一个事实
{ "name": "remember", "arguments": { "data": "用户的博客主站是 blog.example.com。" } }

// 问回来 —— 快速向量检索
{ "name": "recall", "arguments": { "query": "用户的博客地址是什么？", "search_type": "SUMMARIES" } }

// 需要跨跳串起来的问题
{ "name": "recall", "arguments": { "query": "这个博客托管在哪个平台？", "search_type": "GRAPH_COMPLETION", "hops": 2 } }

// 删掉
{ "name": "forget", "arguments": { "dataset": "main_dataset" } }
```

`SUMMARIES` 直接返回最接近的预生成摘要 —— 便宜，而且多数情况够用。`GRAPH_COMPLETION` 先用同一个
向量索引找种子，再沿知识图谱向外走 `hops` 跳并综合答案；当没有单条记忆能独立回答时，它就是你要的。

---

## 工作原理

```
┌──────────────────────────────────────────────────────────────┐
│                     reflect-mem (Rust)                       │
│                                                              │
│   MCP 层 (rmcp)    remember · recall · forget                │
│   transports       stdio · streamable HTTP + bearer token     │
│  ─────────────────────────────────────────────────────────── │
│   remember   切块 ─► 实体抽取 ─► 写图 + 向量                  │
│   recall     SUMMARIES ──────────► 向量检索（快）             │
│              GRAPH_COMPLETION ──► K 跳图遍历 + 综合           │
│   forget     跨库删除，按溯源分区                             │
│  ─────────────────────────────────────────────────────────── │
│   SQLite    知识图谱 · 元数据 · 会话缓存                      │
│   LanceDB   向量                                              │
└───────────────────────────────┬──────────────────────────────┘
                                │ HTTP
                     ┌──────────┴───────────┐
                     │ LLM (OpenAI 兼容)    │  实体抽取 + 答案综合
                     │ Embeddings (Ollama)  │  任意模型
                     └──────────────────────┘
```

写入会把文本变成三样东西 —— `DocumentChunk` 节点、从其中抽取的实体与关系、以及每个 chunk 的摘要。
检索时看哪一样最能回答问题就读回哪一样。

所有外部依赖都是普通 HTTP，只会联系 LLM 与 embedding 端点，其余全部留在磁盘上。

---

## 快速开始

### 1. 安装

**预编译二进制** —— Linux x86_64、macOS arm64/x86_64：

```bash
curl -fsSL https://raw.githubusercontent.com/Rvn0xsy/reflect-mem/main/install.sh | sh
```

或者自己下载压缩包（每个包旁边都发布了 `.sha256` 校验文件）：

```bash
# macOS（Apple Silicon / Intel）
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-aarch64-apple-darwin.tar.gz | tar xz
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-x86_64-apple-darwin.tar.gz | tar xz
sudo install -m 0755 reflect-mem /usr/local/bin/   # 包内还有 LICENSE、NOTICE

# Linux（x86_64）
curl -fsSL https://github.com/Rvn0xsy/reflect-mem/releases/latest/download/reflect-mem-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo install -m 0755 reflect-mem /usr/local/bin/
```

指定版本用 `| sh -s -- v0.0.1`，自定义目录用 `INSTALL_DIR=~/.local/bin`。

**从源码构建** —— 任意平台，需要较新的稳定版 Rust（`edition 2024`）：

```bash
cargo build --release
# 产物：target/release/reflect-mem
```

### 2. 配置

所有配置可以放在一个 TOML 文件里（模板见 [`reflect-mem.example.toml`](reflect-mem.example.toml)）：

```bash
cp reflect-mem.example.toml ~/.agents/reflect-mem/config.toml
$EDITOR ~/.agents/reflect-mem/config.toml
```

```toml
[llm]
model = "MiniMax-M3"
api_key = "sk-..."
thinking = "disabled"          # 跳过 chain-of-thought（更快）

[embedding]
model = "qwen3-embedding:0.6b" # 任意 Ollama embedding 模型
dimensions = 1024              # 必须与该模型一致
```

数据根目录（默认 `~/.agents/reflect-mem`）会在**首次运行时自动创建**（含 schema），无需手动初始化。

环境变量依然可用，且**优先级高于配置文件**，因此容器/CI 无需改配置文件就能注入密钥：

```bash
LLM_API_KEY=sk-... reflect-mem mcp
```

用 `--config /path/to/config.toml` 或 `$REFLECT_MEM_CONFIG` 指定其它配置文件。

> `embedding.model` 与 `embedding.dimensions` 一旦存了东西就不要再改：不同模型的向量不可比较，
> 改动任何一个都会让已存的 embedding 失效。

### 3. 以 MCP 方式提供服务

**stdio**（本地 Agent）：

```bash
reflect-mem mcp --transport stdio
```

**Streamable HTTP + Bearer Token**（远程 / 多人共享）：

```bash
reflect-mem mcp --transport streamable-http --bind 127.0.0.1:8080 --token "$SECRET"
# -> reflect-mem MCP on http://127.0.0.1:8080/mcp (bearer token required)
```

此后每个请求都必须带 `Authorization: Bearer $SECRET`，没有有效 Token 的请求返回 `401`。
未配置 Token 时端点**完全开放** —— 只应在 loopback 上这么做。

在 MCP 客户端里注册（stdio）：

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

或指向 HTTP 端点：

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

### 4. 试试 CLI

```bash
reflect-mem recall "我之前的博客地址是什么？" --search-type SUMMARIES
reflect-mem recall "我之前的博客地址是什么？" --search-type GRAPH_COMPLETION --hops 2
reflect-mem remember --data "用户的博客主站是 blog.example.com。"
reflect-mem forget --dataset main_dataset
```

---

## Docker

镜像以 HTTP 传输运行、带 Bearer Token 认证、以非 root 用户执行，并提供 `/healthz` 探针 ——
见 [`Dockerfile`](Dockerfile) 与 [`docker-compose.yml`](docker-compose.yml)。

```bash
cp .env.example .env          # 然后填 LLM_API_KEY 和 MCP_TOKEN
# 把 DATA_DIR 指向已有记忆库（默认是全新的 ./data）
docker compose up -d
docker compose logs -f
```

端点为 `http://127.0.0.1:8080/mcp`：

```bash
curl -X POST http://127.0.0.1:8080/mcp \
  -H 'Authorization: Bearer '"$MCP_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

| | |
|---|---|
| 数据 | `DATA_DIR`（宿主机）→ `/data`（容器）。镜像内 `DATA_ROOT=/data`，因此 `config.toml` 位于 `/data/config.toml`。 |
| 认证 | `MCP_TOKEN` 必填；不带 Token 的请求返回 `401`。`GET /healthz` 保持开放供探针使用。 |
| Embedding | 默认指向 `host.docker.internal:11434`（宿主机上的 Ollama）。也可内置一个：`docker compose --profile ollama up -d`。 |
| 端口 | 仅发布到宿主机 loopback（`127.0.0.1:${MCP_HOST_PORT}:8080`）—— 要对外开放请明确修改。 |

> 容器以 uid `10001` 运行，需要对 `DATA_DIR` 有**写权限**（SQLite WAL、LanceDB）。
> 若无法写入，在 compose 服务里加 `user: "$(id -u):$(id -g)"`。

不用 compose 直接跑：

```bash
docker build -t reflect-mem .
docker run --rm -p 127.0.0.1:8080:8080 -v "$HOME/.agents/reflect-mem:/data" \
  -e LLM_API_KEY=... -e MCP_TOKEN=... reflect-mem
```

---

## CLI 参考

| 命令 | 说明 |
|------|------|
| `migrate --input <dir>` | 导入图 dump（`nodes.jsonl` / `edges.jsonl`）到 `reflect-mem.graph.sqlite` |
| `inspect` | 节点/边计数与类型直方图 |
| `traverse <id> --hops N` | 打印某节点的 K 跳邻域 |
| `vectors` | 列出 LanceDB 表及行数 |
| `recall <query>` | 检索记忆并综合答案（`--search-type`、`--top-k`、`--hops`） |
| `remember --data <text>` | 存永久记忆（抽取实体，写图 + 向量） |
| `forget --data-id <uuid>` \| `--dataset <name>` \| `--everything` | 删除记忆 |
| `doctor --dump <dir> [--heal-vectors]` | 校验并修复存储一致性 |
| `mcp --transport stdio` | 通过 stdio 提供记忆 API |
| `mcp --transport streamable-http --bind <addr> --token <t>` | 在 `/mcp` 上提供 HTTP 服务，要求 `Authorization: Bearer <t>` |

---

## 配置

解析优先级，从高到低：

1. CLI 参数（`--config`、`mcp --bind` / `--token`）
2. 环境变量
3. TOML 配置文件
4. 内置默认值

配置文件默认位于 `<data_root>/config.toml`；用 `--config` 或 `REFLECT_MEM_CONFIG` 覆盖路径。
可从 [`reflect-mem.example.toml`](reflect-mem.example.toml) 起步。

| TOML 键 | 环境变量 | 默认值 | 说明 |
|---------|----------|--------|------|
| `data_root` | `DATA_ROOT` | `~/.agents/reflect-mem` | 数据根目录（其下是 `system/databases/…`） |
| `llm.endpoint` | `LLM_ENDPOINT` | `https://api.minimaxi.com/v1` | OpenAI 兼容的 chat-completions 端点 |
| `llm.model` | `LLM_MODEL` | `MiniMax-M2.7-highspeed` | 模型 id（`openai/` 前缀会被剥掉） |
| `llm.api_key` | `LLM_API_KEY` | — | 必填 |
| `llm.args` | `LLM_ARGS` | `{}` | 额外请求字段（如 `{ reasoning_split = true }`） |
| `llm.thinking` | `LLM_THINKING` | *(未设置)* | `disabled` 跳过 chain-of-thought；`adaptive`/未设置则保持开启 |
| `embedding.endpoint` | `EMBEDDING_ENDPOINT` | `http://localhost:11434/api/embed` | Ollama embed 端点（非 Docker 环境下 `host.docker.internal` 会被自动改写） |
| `embedding.model` | `EMBEDDING_MODEL` | `qwen3-embedding:0.6b` | 任意 Ollama embedding 模型 |
| `embedding.dimensions` | `EMBEDDING_DIMENSIONS` | `1024` | 必须与该模型一致 |
| `mcp.transport` | `MCP_TRANSPORT` | `stdio` | `stdio` 或 `streamable-http` |
| `mcp.bind` | `MCP_BIND` | `127.0.0.1:8080` | `streamable-http` 的监听地址 |
| `mcp.token` | `MCP_TOKEN` | *(未设置)* | HTTP 请求必须携带的 Bearer Token |
| — | `REFLECT_MEM_CONFIG` | `<data_root>/config.toml` | 配置文件路径 |

---

## 性能测试

单机参考值，不代表保证。环境：**Apple M5 Max（18 核），macOS 27，arm64**，`rustc 1.96.1` release 构建，
数据为 **6,365 节点 / 18,479 边** + 524 MB LanceDB，复制到 `/tmp` 运行，真实库不受影响。

本地存储与图的数字不含进程启动开销。端到端数字包含完整链路（embedding → 检索 → LLM 综合），
因此主要由模型决定，而不是 reflect-mem。

### 图遍历（纯本地，SQLite 递归 CTE）

50 个实体种子 × 3 轮，即 `GRAPH_COMPLETION` 的图半边：

| 跳数 | 均值 | p50 | p95 | p99 | 平均节点 | 平均边 |
|------|------|-----|-----|-----|----------|--------|
| 1 | **0.08 ms** | 0.02 ms | 0.25 ms | 0.30 ms | 2.8 | 1.9 |
| 2 | **0.05 ms** | 0.03 ms | 0.14 ms | 0.46 ms | 5.6 | 6.0 |
| 3 | **0.06 ms** | 0.03 ms | 0.26 ms | 0.47 ms | 10.8 | 16.0 |

K 跳遍历是**亚毫秒级**的 —— SQLite 图不是瓶颈。

### 向量检索（LanceDB，k=5，20 轮）

| 表 | 行数 | 均值 |
|----|------|------|
| `Entity_name` | 5,125 | **13.5 ms** |
| `TextSummary_text` | 274 | **5.3 ms** |
| `DocumentChunk_text` | 274 | **4.9 ms** |

Embedding（`qwen3-embedding:0.6b`，1024 维）：均值 **15.4 ms**。

### 端到端 recall（真实 LLM）

| 场景 | 开启 thinking | 关闭 thinking | |
|------|---------------|---------------|--|
| `SUMMARIES` | 2.75 s | **2.28 s** | −17% |
| `GRAPH_COMPLETION` | 3.26 s | **2.55 s** | −22% |

主要开销在 LLM 生成；本地存储 + 图 + 向量加起来不到 ~20 ms。
`LLM_THINKING=disabled` 能砍掉 17–22% 的端到端延迟，且答案质量无可见退化。

### 复现

```bash
# 1. 复制一份隔离数据（绝不碰真实库）
cp -R ~/.agents/reflect-mem/system/databases/reflect-mem.graph.sqlite /tmp/reflect-mem-bench/
cp -R ~/.agents/reflect-mem/system/databases/reflect-mem.lancedb /tmp/reflect-mem-bench/

# 2. 跑本地热路径压测（图 + embedding + 向量检索）
BENCH_ROOT=/tmp/reflect-mem-bench cargo run --release --bin bench

# 3. 端到端 recall 用 CLI 测（需要 LLM + Ollama）
```

---

## 数据布局

所有东西都在 `DATA_ROOT` 下（默认 `~/.agents/reflect-mem`）。它会在首次运行时创建，整体拷贝、备份、
迁移都是安全的：

```
<DATA_ROOT>/
  system/databases/
    reflect-mem.sqlite         # datasets、data 行、流水线状态
    reflect-mem.graph.sqlite   # 知识图谱（节点 / 边）
    reflect-mem.lancedb/       # 向量
  data/text_<hash>.txt         # 源文本，按内容寻址
```

把 `DATA_ROOT` 指向一个已有目录就能接着用 —— store 是**原地打开**的，不是导入。删掉目录即重置一切。

由于 id 是从内容确定性推导的，重复写入已存过的文本是空操作，不会产生重复。

---

## 文档

- [`skills/reflect-mem-memory/SKILL.md`](skills/reflect-mem-memory/SKILL.md) —— 给使用这些工具的
  AI Agent 看的操作说明（remember / recall / forget 语义）。

## 项目结构

```
src/
  main.rs       CLI 入口
  mcp.rs        rmcp 服务：工具 + stdio / streamable-HTTP + Bearer 认证
  settings.rs   配置文件（TOML）+ 环境变量解析
  recall.rs     读取路径（SUMMARIES / GRAPH_COMPLETION）
  remember.rs   写入路径（永久记忆 ETL）
  forget.rs     跨库删除
  doctor.rs     一致性校验与修复
  migrate.rs    图 dump 导入器
  llm.rs        OpenAI 兼容的 LLM 客户端
  embed.rs      Ollama embedding 客户端
  config.rs     数据根目录路径布局
  storage/      图 / 向量 / 关系层 / 会话缓存
  ingest/       切块 + 实体抽取
skills/         给 AI Agent 的操作 skill
reflect-mem.example.toml      带注释的配置模板
Dockerfile / .dockerignore    容器镜像
.env.example / docker-compose.yml   compose 部署
```

## 参与贡献

欢迎提 Issue 和 PR。CI 跑的就是这三条：

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

一个坑：某个依赖的 build script 会生成 protobuf 绑定，所以机器上必须有 `protoc`。
Debian/Ubuntu 上是 `apt-get install protobuf-compiler libprotobuf-dev`；macOS 上是 `brew install protobuf`。

## 致谢

本项目的存储布局、图 schema 与写入管线参考了
[cognee](https://www.cognee.ai) —— 也就是本项目用 Rust 重新实现的参考实现。
cognee 采用 [Apache License 2.0](https://github.com/topoteretes/cognee/blob/main/LICENSE)
（Copyright 2024 Topoteretes UG）。`reflect-mem` 是独立实现，不打包 cognee 源码。
详见 [NOTICE](NOTICE)。

## 许可证

MIT —— 见 [`LICENSE`](LICENSE)。上游归属声明见 [`NOTICE`](NOTICE)。
