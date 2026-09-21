# reflect-mem

用 Rust 重写 cognee 的记忆管理 MCP 服务，产出**单一静态二进制**，彻底移除 Python 运行时，复用现有记忆数据。

- 完整设计见 [`docs/design.md`](docs/design.md)
- 面向使用者的记忆工具说明见 [`skills/reflect-mem-memory/SKILL.md`](skills/reflect-mem-memory/SKILL.md)

## 它是什么

`reflect-mem` 是一个长期记忆 MCP 服务，通过 `rmcp` 暴露 `remember` / `recall` / `forget` 三个工具，让 AI 助手能跨会话存取用户说过的事实。底层是 Rust 原生实现，替代了此前的 Python `cognee-mcp`。

对外只依赖 HTTP（MiniMax LLM + Ollama embedding），存储全部走 Rust 原生库或 bundled SQLite，因此可以打成单一二进制，无 Python 运行时。

## 当前状态

| 模块 | 状态 |
|------|------|
| 图迁移（`LBUG+` → SQLite 属性图） | ✅ 已完成并对账 |
| 存储层（图 / 向量 / 关系层 / 会话） | ✅ 图与向量；关系层已接 |
| `recall`（SUMMARIES + GRAPH_COMPLETION） | ✅ 端到端跑通 |
| `remember`（永久记忆 ETL） | ✅ 已实现 |
| `forget`（跨库删除 + provenance 分区） | ✅ 已实现 |
| `doctor`（一致性校验与修复） | ✅ 已实现 |
| MCP 层 | 🟡 stdio 已通，streamable HTTP 待做 |

## 构建

```bash
cargo build --release
# 产物 target/release/reflect-mem
```

## CLI 子命令

| 命令 | 说明 |
|------|------|
| `migrate --input <dir>` | 导入迁移 dump（`nodes.jsonl` / `edges.jsonl`）到 `graph.sqlite` |
| `inspect` | 打印图的节点/边计数与类型直方图 |
| `traverse <id> --hops N` | 展示某节点的 K 跳邻域（GRAPH_COMPLETION 的图半边） |
| `vectors` | 列出复用的 LanceDB 向量表及行数 |
| `recall <query> --search-type SUMMARIES\|GRAPH_COMPLETION` | 查记忆并综合答案 |
| `remember --data <text>` | 存永久记忆（实体抽取 + 写图/向量） |
| `forget --data-id <uuid> \| --dataset <name> \| --everything` | 删记忆 |
| `doctor --dump <dir> [--heal-vectors]` | 校验并修复存储一致性 |
| `mcp --transport stdio` | 以 MCP 协议提供服务 |

## 配置

环境变量（对齐 Python 版命名）：

```bash
# LLM（实体抽取 + recall 综合）
LLM_PROVIDER=openai
LLM_MODEL=openai/MiniMax-M2.7-highspeed
LLM_ENDPOINT=https://api.minimaxi.com/v1
LLM_API_KEY=...

# Embedding（必须与历史数据一致，否则向量作废）
EMBEDDING_PROVIDER=ollama
EMBEDDING_MODEL=qwen3-embedding:0.6b
EMBEDDING_ENDPOINT=http://host.docker.internal:11434/api/embed
EMBEDDING_DIMENSIONS=1024

# 数据根目录（默认复用旧数据目录）
DATA_ROOT=~/.agents/reflect-mem
```

## 目录结构

```
src/
  main.rs       CLI 入口
  mcp.rs        rmcp 服务 + 工具注册
  recall.rs     读取管线（SUMMARIES / GRAPH_COMPLETION）
  remember.rs   写入管线（永久记忆 ETL）
  forget.rs     跨库删除
  doctor.rs     一致性校验与修复
  migrate.rs    图迁移导入器
  llm.rs        MiniMax 客户端（OpenAI 兼容）
  embed.rs      Ollama embedding 客户端
  config.rs     环境变量与路径
  storage/      图 / 向量 / 关系层 / 会话缓存
  ingest/       chunk + 实体抽取
docs/design.md      设计文档（决策与架构）
migration/           一次性图迁移工具（Python，跑完即弃）
skills/reflect-mem-memory/   AI 助手的记忆工具使用说明（skill）
```

## 维护 skill

`skills/reflect-mem-memory/SKILL.md` 是给使用本服务的 AI 助手看的操作说明，随项目一起维护。改完它之后，需要同步到助手的 skill 目录（`~/.agents/skills/reflect-mem-memory/`）才会生效。
