<div align="center">

# reflect-mem

**给 AI Agent 的长期记忆 —— 一个 Rust 单二进制，通过 MCP 提供 `remember` / `recall` / `forget`。**

[![Rust 1.96](https://img.shields.io/badge/rust-1.96-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![edition 2024](https://img.shields.io/badge/edition-2024-orange)]()
[![MCP](https://img.shields.io/badge/MCP-ready-blue)]()
[![SQLite](https://img.shields.io/badge/SQLite-graph%2Frelational-003B57?logo=sqlite&logoColor=white)]()
[![LanceDB](https://img.shields.io/badge/LanceDB-vectors-8A2BE2)]()
[![version 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey)]()

[English](README.md) · **简体中文**

</div>

`reflect-mem` 是一个用 Rust 实现的长期记忆 MCP 服务。它通过 Model Context Protocol 暴露
`remember` / `recall` / `forget`，让 AI Agent 能存取跨会话存活的事实 —— **不需要 Python 运行时、
不需要 venv、也不依赖 Kuzu 的 C++ 链接**。

它可以直接架在已有的记忆库之上：源文本、`reflect-mem.sqlite`（关系层）和 520 MB 的
`reflect-mem.lancedb`（向量层）全部**原地**打开，字节级复用。只有私有的 `LBUG+` 图被迁移成
一个 SQLite 属性图。

---

## 特性

- **单一静态二进制。** 无 Python、无系统 OpenSSL（用 `rustls`）、无 Kuzu 链接。每个存储引擎要么是
  Rust 原生，要么是 bundled SQLite。
- **原地复用数据。** `data/text_*.txt`、`reflect-mem.sqlite`、`reflect-mem.lancedb` 全部字节级原地
  打开 —— embedding 模型不变（`qwen3-embedding:0.6b`，1024 维），所以已有向量继续有效。
- **亚毫秒级图遍历。** `GRAPH_COMPLETION` 的 K 跳扩展是 SQLite 递归 CTE，跑在约 6.3k 节点 /
  18.5k 边上，**几十微秒**返回（见[性能测试](#性能测试)）。
- **两种检索模式。** `SUMMARIES`（对层级摘要做快速向量检索）和 `GRAPH_COMPLETION`
  （在知识图上做多跳推理）。
- **三个记忆工具。** `remember`（永久记忆 ETL 或会话快路径）、`recall`、`forget`
  （按溯源分区、跨库删除），外加 `doctor` 做一致性修复。
- **确定性 ID。** 实体、类型、边都是 `uuid5` 推导，所以重复写入同一批事实是幂等且去重的。
- **配置文件 + 两种传输。** 一个 TOML 文件（环境变量可覆盖）驱动全部配置；MCP 服务支持
  **stdio** 与 **streamable HTTP**，后者可选 Bearer Token 认证。
- **提供容器镜像。** 多阶段 `Dockerfile`（非 root、带 healthcheck）加一个 `docker-compose.yml`，
  一条命令拉起带 Token 保护的 HTTP 端点。

---

## 工作原理

```
┌──────────────────────────────────────────────────────────────┐
│                     reflect-mem (Rust)                       │
│                                                              │
│   MCP 层 (rmcp)  ·  remember / recall / forget               │
│   transports: stdio · streamable HTTP + bearer token          │
│  ─────────────────────────────────────────────────────────── │
│   recall     SUMMARIES ──────────► LanceDB 向量检索           │
│              GRAPH_COMPLETION ──► 向量 → SQLite K 跳 CTE      │
│   remember   切块 → MiniMax 抽取 → 写图 + 向量                │
│   forget     跨库删除，按溯源分区                             │
│  ─────────────────────────────────────────────────────────── │
│   SQLite   图 · 关系层 · 会话缓存                             │
│   LanceDB  向量（原地复用）                                   │
└───────────────────────────────┬──────────────────────────────┘
                                │ 仅 HTTP
                     ┌──────────┴───────────┐
                     │ MiniMax LLM          │  实体抽取 + 答案综合
                     │ Ollama embedding     │  qwen3-embedding:0.6b (1024d)
                     └──────────────────────┘
```

所有外部依赖都是普通 HTTP。宿主机上除这个二进制外无需安装任何东西。

---

## 快速开始

### 1. 构建

```bash
cargo build --release
# 产物：target/release/reflect-mem
```

需要较新的稳定版 Rust（`edition 2024`）。

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
model = "qwen3-embedding:0.6b" # 必须与产出向量的模型一致
dimensions = 1024
```

环境变量依然可用，且**优先级高于配置文件**，因此容器/CI 无需改配置文件就能注入密钥：

```bash
LLM_API_KEY=sk-... reflect-mem mcp
```

用 `--config /path/to/config.toml` 或 `$REFLECT_MEM_CONFIG` 指定其它配置文件。

> `embedding.model` **必须**与产出已有向量的模型一致，否则复用的 `reflect-mem.lancedb` 会失效。

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
| `vectors` | 列出复用的 LanceDB 表及行数 |
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
| `embedding.model` | `EMBEDDING_MODEL` | `qwen3-embedding:0.6b` | 必须与已有向量一致 |
| `embedding.dimensions` | `EMBEDDING_DIMENSIONS` | `1024` | 必须与已有向量一致 |
| `mcp.transport` | `MCP_TRANSPORT` | `stdio` | `stdio` 或 `streamable-http` |
| `mcp.bind` | `MCP_BIND` | `127.0.0.1:8080` | `streamable-http` 的监听地址 |
| `mcp.token` | `MCP_TOKEN` | *(未设置)* | HTTP 请求必须携带的 Bearer Token |
| — | `REFLECT_MEM_CONFIG` | `<data_root>/config.toml` | 配置文件路径 |

---

## 性能测试

环境：release 构建，真实记忆库（**6,365 节点 / 18,479 边**，524 MB LanceDB），复制到 `/tmp`
运行，真实库不受影响。本地存储与图的数字不含进程启动开销；端到端数字包含完整链路
（embedding → 检索 → LLM 综合）。

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

### 端到端 recall（真实 MiniMax LLM）

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

## 架构与文档

- [`docs/design.md`](docs/design.md) —— 完整设计：决策、存储 schema、ETL 规格、迁移、风险。
- [`skills/reflect-mem-memory/SKILL.md`](skills/reflect-mem-memory/SKILL.md) —— 给使用这些工具的
  AI Agent 看的操作说明（remember / recall / forget 语义）。
- [`migration/`](migration/README.md) —— 一次性的 `LBUG+` → JSONL 图导出（Python，跑一次即弃）。

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
  llm.rs        MiniMax 客户端（OpenAI 兼容）
  embed.rs      Ollama embedding 客户端
  config.rs     数据根目录路径布局
  storage/      图 / 向量 / 关系层 / 会话缓存
  ingest/       切块 + 实体抽取
docs/           设计文档
migration/      一次性图迁移工具
skills/         给 AI Agent 的操作 skill
reflect-mem.example.toml      带注释的配置模板
Dockerfile / .dockerignore    容器镜像
.env.example / docker-compose.yml   compose 部署
```

## 致谢

本项目的存储布局、图 schema 与写入管线参考了
[cognee](https://www.cognee.ai) —— 也就是本项目用 Rust 重新实现的参考实现。
cognee 采用 [Apache License 2.0](https://github.com/topoteretes/cognee/blob/main/LICENSE)
（Copyright 2024 Topoteretes UG）。`reflect-mem` 是独立实现，不打包 cognee 源码。
详见 [NOTICE](NOTICE)。

## 许可证

尚未确定许可证。在选定之前保留所有权利。
