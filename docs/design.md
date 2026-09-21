# reflect-mem 设计文档

> 目标：用 Rust 重写 cognee 的记忆管理 MCP 服务，产出**单一静态二进制**，彻底移除 Python 运行时，复用现有记忆数据，迁移成本最小化。

---

## 0. 文档状态

- 决策来源：设计 grilling 会话 + 用户逐条确认。
- 状态：已达成共享理解，frontier 清空。
- 范围：本文件描述**记忆管理 MCP**（`remember` / `recall` / `forget`），不覆盖 cognee 的完整流水线（`cognify` 全部搜索类型、`improve`、自定义图模型、多租户隔离等）。

---

## 1. 背景

现状：Python 的 `cognee-mcp` 通过 FastMCP 暴露 MCP 工具，底层是 Python `cognee` 库（LLM 实体抽取、chunking、Kuzu 图库、LanceDB 向量库）。它有三个连接模式（direct / API / cloud），三个 transport（stdio / SSE / streamable HTTP）。

问题：部署需要 Python 环境 + venv，冷启动慢，二进制体积大，且记忆引擎与 MCP 协议层耦合在一个 Python 进程里。

---

## 2. 决策摘要（设计树结论）

| # | 决策 | 结论 |
|---|------|------|
| D1 | 重写范围 | 忠实复刻，但只保留记忆管理（remember/recall/forget） |
| D2 | Rust 是否写永久记忆 | 是，复刻 cognify 写入路径 |
| D3 | recall 是否要多跳 | 是，需要 GRAPH_COMPLETION |
| D4 | 图库方案 | 一次性迁移（不留 Python sidecar） |
| D5 | 交付物形态 | 单一二进制，无 Python 运行时 |
| D6 | 图存储引擎 | SQLite 属性图（递归 CTE 做 K 跳遍历） |
| D7 | 图迁移方式 | 一次性 Python 脚本 dump `LBUG+` → JSONL → 导入 |
| D8 | MCP SDK | `rmcp` |
| D9 | transports | stdio + streamable HTTP（不要 SSE） |
| D10 | 会话缓存 | Rust 自建独立 SQLite（不碰旧 `cache.db`） |
| D11 | 替换策略 | 迁移完成后 Rust 单进程独占，Python 退役 |
| D12 | ETL 保真度 | 语义兼容（同 LLM、同 embedding、同 schema），不追求字节对齐 |

---

## 3. 总体架构

```
┌─────────────────────────────────────────────────────────────┐
│              reflect-mem（单一 Rust 二进制）                  │
│                                                             │
│  MCP 层（rmcp, tokio）                                       │
│    工具: remember / recall / forget / cognify_status         │
│    transports: stdio + streamable HTTP                       │
│                                                             │
│  服务层                                                     │
│    remember: ETL 编排（chunk → 抽取 → 写各库）                │
│    recall:   路由（session / SUMMARIES / GRAPH_COMPLETION）   │
│    forget:   跨库删除（SQLite + LanceDB + 图）                │
│                                                             │
│  存储层                                                     │
│    ├─ 关系/元数据: SQLite (rusqlite)      → 复用 cognee_db    │
│    ├─ 向量:        LanceDB (lancedb crate)→ 复用 cognee.lancedb│
│    ├─ 图:          SQLite 属性图          → 迁移自 LBUG+       │
│    ├─ 会话缓存:     SQLite (自建)          → reflect_session.db│
│    └─ 源文本:       content-addressed 文件 → 复用 data/        │
│                                                             │
│  外部依赖（纯 HTTP，无本地 Python）                            │
│    ├─ LLM:       MiniMax (OpenAI 兼容, reqwest)              │
│    └─ Embedding: Ollama qwen3-embedding:0.6b (reqwest, 1024d)│
└─────────────────────────────────────────────────────────────┘
```

关键点：**所有外部依赖都是 HTTP**（MiniMax、Ollama），存储全部是 Rust 原生或 bundled SQLite，因此能打成单一二进制。

---

## 4. 数据层设计

### 4.1 现状数据清单（实测）

数据根目录：`~/.agents/reflect-mem/`（可配置；2026-09-20 由 `~/.agents/cognee-memory/` 改名而来）

| 路径 | 格式 | 大小 | 角色 | 复用策略 |
|------|------|------|------|----------|
| `data/text_<md5>.txt`（127 个） | 纯文本 | ~几 MB | 源文档（**不可再生**） | ✅ 原地复用 |
| `system/databases/cognee_db` | SQLite | 4.4 MB | 关系元数据（**不可再生**） | ✅ 原地复用（rusqlite 只读+写） |
| `system/databases/cache.db` | SQLite | 576 KB | 旧会话缓存 | ⚠️ 不碰，自建新库 |
| `system/databases/cognee.lancedb/` | LanceDB | 520 MB | embedding 向量（派生） | ✅ 原地复用（同 embedding 模型） |
| `system/databases/cognee_graph_ladybug` | **LBUG+ 私有** | 84 MB | 知识图谱（派生） | 🔁 **唯一迁移项** |
| `graph_*.html` | HTML | 33 MB | 可视化导出 | ❌ 忽略（可再生） |

### 4.2 复用 vs 迁移的边界

**原地复用（字节不动）**：源文本、`cognee_db`（关系层）、`cognee.lancedb`（向量层）。
**唯一迁移**：`cognee_graph_ladybug` 的图数据，因为其魔数为 `LBUG+`（`4c 42 55 47 2b`），是 ladybug（Kuzu 的 C++ fork）的私有落盘格式，Rust 没有能打开它的库。

向量层能原地复用的前提是 **embedding 模型不变**：`qwen3-embedding:0.6b`，1024 维，Ollama `http://host.docker.internal:11434/api/embed`。若换模型/维度，520 MB 向量全部作废，必须重算。

### 4.3 图存储：SQLite 属性图

迁移目标是一个新的 SQLite 文件（`system/databases/graph.sqlite`），schema 与 ladybug 的 `Node`/`EDGE` 表 1:1 对齐，迁移就是直搬：

```sql
CREATE TABLE graph_nodes (
  id TEXT PRIMARY KEY,
  name TEXT,
  type TEXT,              -- 判别列: Entity / DocumentChunk / TextDocument / TextSummary / EntityType / ...
  created_at TEXT,
  updated_at TEXT,
  properties TEXT,        -- JSON
  source_ref_keys TEXT,
  source_dataset_ids TEXT,
  source_run_ids TEXT,
  source_run_refs TEXT
);

CREATE TABLE graph_edges (
  from_id TEXT NOT NULL,
  to_id   TEXT NOT NULL,
  relationship_name TEXT,
  created_at TEXT,
  updated_at TEXT,
  properties TEXT,        -- JSON
  source_ref_keys TEXT,
  source_dataset_ids TEXT,
  source_run_ids TEXT,
  source_run_refs TEXT
);

CREATE INDEX idx_edges_from ON graph_edges (from_id);
CREATE INDEX idx_edges_to   ON graph_edges (to_id);
CREATE INDEX idx_edges_rel  ON graph_edges (relationship_name);
CREATE INDEX idx_nodes_type ON graph_nodes (type);
```

**K 跳遍历（GRAPH_COMPLETION 的核心）用递归 CTE：**

```sql
WITH RECURSIVE walk(id, depth) AS (
    SELECT to_id, 1 FROM graph_edges WHERE from_id = :seed
  UNION
    SELECT e.to_id, w.depth + 1
    FROM graph_edges e JOIN walk w ON e.from_id = w.id
    WHERE w.depth < :max_hops
)
SELECT DISTINCT n.* FROM graph_nodes n JOIN walk w ON n.id = w.id;
```

- 需要「只沿某些关系类型走」时，在 CTE 里加 `AND e.relationship_name IN (...)`。
- 需要方向控制（双向/正向/反向）时，加一条反向递归分支或 WHERE 过滤。

> 选 SQLite 而非 vanilla Kuzu 的理由：GRAPH_COMPLETION 实际是「向量命中实体 → 沿边走 1~N 跳取邻居子图」，是递归 CTE 的舒适区；SQLite 让「单一二进制 + 只依赖 bundled SQLite」最干净，避免 Kuzu 的 C++ 静态链接复杂度。

### 4.4 向量层：LanceDB（原地复用）

`lancedb` crate 直接打开 `cognee.lancedb/`。现有表：

| Lance 表 | 对应内容 |
|----------|----------|
| `Entity_name` | 实体名向量 |
| `DocumentChunk_text` | 文档块向量 |
| `TextDocument_name` | 文档名向量 |
| `TextSummary_text` | 层级摘要向量（SUMMARIES 检索） |
| `EntityType_name` | 实体类型向量 |
| `EdgeType_relationship_name` | 边类型向量 |
| `SessionQAVector_text` | 会话 QA 向量 |

**已在 R2 spike 中实测通过**（`src/bin/spike_lancedb.rs`）：

- `lancedb = "=0.37.1"` 可打开 Python 写的库，7 张表全部可读。
  - ⚠️ **版本坑**：`0.38.0` / `0.39.0` 的 `pub mod job` 无条件引用被 `remote` feature 门控的 `Error::Http`，默认 feature 下编译不过。`0.37.1` 是能编译的最新版，必须锁定。
  - ⚠️ `arrow-array` / `arrow-schema` 必须与 lancedb 内部的 arrow 版本一致（`=58.4.0`），否则传给 lancedb 的 `RecordBatch` 类型不统一。
- 实测行数：`Entity_name` 5109、`EdgeType_relationship_name` 22343、`EntityType_name` 578、`DocumentChunk_text` 271、`TextSummary_text` 271、`SessionQAVector_text` 122、`TextDocument_name` 120。
- 统一 schema：`id: Utf8` + `vector: FixedSizeList(1024 x Float32)` + `payload: Struct(...)`；`payload` 含 `text`、`document_id`、`document_name`、`belongs_to_set`、`created_at`、`type`、`source_*` 等。
- 向量检索可用：`table.vector_search(vec)?.limit(k).execute()`，需要 `use lancedb::query::{ExecutableQuery, QueryBase}`。

**写入**：新 embedding 必须走同一个模型（qwen3-embedding:0.6b / 1024d），否则与旧向量不可比。

### 4.5 关系层：SQLite（原地复用）

`rusqlite` 直接打开 `cognee_db`。关键表（实测 schema）：

- `data`：数据条目（`id UUID PK`、`name`、`content_hash`、`dataset_id`、`node_set JSON`、`pipeline_status JSON`、`token_count`、时间戳、`importance_weight`…）
- `datasets`：数据集（`id`、`name`、`owner_id`、`tenant_id`）
- `nodes` / `edges`：图的**关系镜像**（含 `data_id`、`dataset_id`、`source_node_id`、`slug` 等）
- `pipeline_runs`：流水线运行状态（cognify_status 的数据源）
- 多租户相关表（`users`/`tenants`/`roles`/`acls`/`permissions`…）：单用户场景下**只读不写**，不实现多租户逻辑

> 注意：`cognee_db.nodes/edges` 与图库里的 Node/EDGE 是两套（关系镜像 + 遍历图）。Rust 侧以 `graph.sqlite` 为遍历真源，`cognee_db.nodes/edges` 是否同步维护由实现阶段决定（见 §11 风险 R3）。

### 4.6 会话缓存：自建 SQLite

新建 `reflect_session.db`，不碰旧 `cache.db`（避免与退役前的 Python 抢锁、也避免 schema 绑定）。

```
session_records(session_id, data_id, content, created_at, ...)
```

会话记忆 = 纯文本写入，无 LLM、无 embedding、无图（与 Python 版语义一致：`remember(..., session_id=...)` 只走快路径）。

---

## 5. 写入管线（remember，永久记忆）

`remember(data | filename+content_base64, dataset_name, session_id, custom_prompt, background)`：

```
输入校验（data XOR file；file ≤ 10MB；file 不支持 session_id）
        │
        ├─ 有 session_id ──► 会话缓存路径：写 reflect_session.db，立即返回
        │
        └─ 无 session_id ──► 永久记忆路径（ETL）：
                1. 落盘源文本  data/text_<md5>.txt
                2. 写关系层   cognee_db.data / datasets / nodes / edges
                3. chunk     文本切分（语义兼容，不追求与 Python 逐字节一致）
                4. 抽取      MiniMax（OpenAI 兼容 + JSON mode）产出实体/关系
                5. 写图      graph.sqlite 的 graph_nodes / graph_edges
                6. 写向量    Ollama embedding → cognee.lancedb 对应表
                7. (可选 background=True) 后台执行，cognify_status 可查
```

抽取的 JSON schema 对齐 cognee 的 KnowledgeGraph 数据模型（实体、关系类型），但**语义兼容**即可，不逐字节复刻 Python 的 prompt/切分边界。

---

## 6. 读取管线（recall）

`recall(query, search_type, datasets, session_id, system_prompt, top_k)`：

```
输入校验（top_k 夹紧，search_type 归一化）
        │
        ├─ 有 session_id 且无 datasets/search_type ──► 会话缓存关键字检索，
        │     命中则返回；未命中回落到永久记忆
        │
        └─ 永久记忆检索，按 search_type 路由：
             ├─ SUMMARIES ──► LanceDB TextSummary_text 向量检索 ──► 返回摘要
             └─ GRAPH_COMPLETION ──►
                  1. LanceDB 向量检索命中实体种子
                  2. graph.sqlite 递归 CTE 做 1~N 跳邻居扩展
                  3. 取回子图 + 关联文本块
                  4. MiniMax 综合生成答案
```

输出前**剥离 embedding 向量字段**（对应 Python 版 `strip_vectors`），避免把原始 float 向量灌进 LLM 上下文。

---

## 7. forget

`forget(dataset, everything, data_id, dataset_id)`：

- 跨三层删除：关系层（`cognee_db`）、向量层（`cognee.lancedb`）、图（`graph.sqlite`）。
- 与 Python 版语义对齐：按数据集、按 data_id、或 everything。
- 删除是**不可逆**的，工具描述里写明，且仅由用户显式触发。

---

## 8. MCP 层设计

### 8.1 SDK 与 transports

- SDK：`rmcp`（stdio + streamable HTTP 均支持，类型化 tool schema）。
- **不做 SSE**（MCP 规范中 SSE 已 legacy，官方推 streamable HTTP）。
- 默认 stdio（CLI/IDE 宿主），HTTP 模式用于远程部署。

### 8.2 工具面

与 Python 版对齐的精简面：

| 工具 | 说明 |
|------|------|
| `remember` | 存记忆（会话快路径 / 永久 ETL / 文件上传） |
| `recall` | 查记忆（会话 / SUMMARIES / GRAPH_COMPLETION） |
| `forget` | 删记忆（数据集 / 单条 / 全部） |
| `cognify_status` | 查后台 ingestion 进度（background=True 时） |

> 不做 Python 版的 `search_tools` BM25 变换那套动态工具发现——第一版直接固定暴露 4 个工具，工具面小、无需搜索变换。

### 8.3 工具 schema（示例，rmcp 类型化）

```rust
// remember
data: Option<String>,          // 文本内容（与文件互斥）
filename: Option<String>,      // 文件名（配 content_base64）
content_base64: Option<String>,// base64 文件内容（≤10MB）
dataset_name: Option<String>,  // 默认 agent-scoped 数据集
session_id: Option<String>,    // 有则走会话缓存
custom_prompt: Option<String>, // 抽取提示（仅永久模式）
background: bool,              // 永久 ingestion 后台化

// recall
query: String,
search_type: Option<String>,   // SUMMARIES / GRAPH_COMPLETION
datasets: Option<String>,      // 逗号分隔
session_id: Option<String>,
system_prompt: Option<String>,
top_k: i32,                    // 默认 15，建议 5

// forget
dataset: Option<String>,
everything: bool,
data_id: Option<String>,
dataset_id: Option<String>,
```

---

## 9. 迁移方案

唯一迁移项是图（`LBUG+` → `graph.sqlite`）。采用 dump 方式：

1. **一次性 Python 脚本**（迁移工具，跑完即弃，不进交付物）：
   - 用 `ladybug` 包打开 `cognee_graph_ladybug`。
   - 遍历 `Node` / `EDGE` / `GraphMetadata`，dump 成 JSONL（`nodes.jsonl` / `edges.jsonl`）。
2. **Rust 导入器**（`reflect-mem migrate` 子命令）：
   - 读 JSONL，批量 INSERT 进 `graph.sqlite`（事务 + prepared statement）。
3. **校验**：节点/边计数对账，抽样比对 `type` / `relationship_name` 分布。
4. 迁移完成后：`cognee_graph_ladybug` 归档不删（保底回滚），Python venv 可卸载。

> 为什么 dump 而不是 re-cognify：re-cognify 要拿 127 个源文本在 Rust 里重跑全部 LLM 抽取，重复花 MiniMax 的钱；dump 是纯数据搬运，免费且快。用户已确认「彻底摆脱 Python」指交付物，迁移工具可用一次性 Python。

**其余数据不迁移**：源文本、`cognee_db`、`cognee.lancedb` 原地打开。

### 9.1 已实测（2026-09-20）

- dump：`migration/dump_graph.py` 在副本上跑通，导出 **6340 节点 / 18400 边 / 2 metadata**（storage version 43）到 `nodes.jsonl`（14MB）/ `edges.jsonl`（20MB）。
- import：`reflect-mem migrate` 导入并**按 `summary.json` 对账通过** → `graph.sqlite`（35MB）。
- 验证：`reflect-mem inspect` 直方图与源图一致；`reflect-mem traverse <id> --hops 2` 从「开发习惯（通用版）」到达 27 个节点 + 34 条边，多跳上下文完整。

导入端会移除旧 `graph.sqlite` 后全量重建，并对账「dumper 计数 vs 导入计数 vs 库内计数」，任一不一致即报错退出。

---

## 10. 配置

环境变量（对齐 Python 版命名，便于平滑切换）：

```bash
# LLM（实体抽取 + recall 综合）
LLM_PROVIDER=openai
LLM_MODEL=openai/MiniMax-M2.7-highspeed
LLM_ENDPOINT=https://api.minimaxi.com/v1
LLM_API_KEY=...

# 思考开关（MiniMax-M3 / M2.x）：disabled 跳过 chain-of-thought 直接回答（更快），
# 省略或 adaptive 保持默认开启
LLM_THINKING=disabled

# Embedding（必须与历史数据一致，否则向量作废）
EMBEDDING_PROVIDER=ollama
EMBEDDING_MODEL=qwen3-embedding:0.6b
EMBEDDING_ENDPOINT=http://host.docker.internal:11434/api/embed
EMBEDDING_DIMENSIONS=1024

# 数据根目录（默认复用旧数据目录）
DATA_ROOT=~/.agents/reflect-mem
```

---

## 11. 风险与待验证项

- **R1 — 迁移完整性**：✅ **已验证**（2026-09-20）。节点/边/JSON properties/`source_*` 溯源字段完整导出并导入，计数对账通过。见 §9.1。
- **R2 — LanceDB crate 与 Python 版格式兼容**：✅ **已验证**（2026-09-20）。`lancedb =0.37.1` 可打开、读向量、做最近邻检索，top-1 命中自身。见 §4.4。
- **R3 — 图的关系镜像**：`cognee_db.nodes/edges` 与图库的冗余关系需要理清。若 GRAPH_COMPLETION 不依赖镜像表，Rust 侧可只维护 `graph.sqlite`，避免双写。
- **R4 — 抽取质量**：语义兼容（非字节对齐）意味着新旧抽取结果细节不同。需用同 LLM + 相近 prompt 保证实体/关系质量不退化，必要时做小样本 A/B 对比。
- **R5 — 后台任务生命周期**：`background=True` 的 ingestion 需要 pin 住任务（对应 Python 版 `_track_background`），错误进有界环形缓冲，供 `cognify_status` 查询。

---

## 12. 实施阶段（建议顺序）

1. **Spike（先验证两个不确定性）**
   - `lancedb` crate 能否打开/检索现有 `cognee.lancedb`（R2）。
   - 迁移脚本能否完整 dump `LBUG+`（R1）。
2. **迁移器**：Python dump 脚本 + `reflect-mem migrate` 导入 + 对账。
3. **存储层**：rusqlite（关系 + 图 + 会话）、lancedb（向量）封装。
4. **读取路径**：`recall`（SUMMARIES 先行，GRAPH_COMPLETION 后上）。
5. **写入路径**：`remember`（会话快路径先行，永久 ETL 后上，含 background + cognify_status）。
6. **forget**：跨库删除。
7. **MCP 层**：rmcp 工具注册 + stdio + streamable HTTP。
8. **切换**：迁移 → Rust 独占 → Python 退役。

---

## 附录：已确认的关键事实

- 图库魔数 `LBUG+`，是 ladybug（Kuzu C++ fork，版本 0.19.0，storage-code 41→43）私有格式。
- 图 schema：`Node(id, name, type, properties JSON, source_*, created_at, updated_at)` + `EDGE(from, to, relationship_name, ...)` + `GraphMetadata(key, value)`。
- 节点类型（实测自图可视化）：`TextDocument`、`DocumentChunk`、`Entity`、`EntityType`、`NodeSet`、`TextSummary` 等。
- embedding：`qwen3-embedding:0.6b` / 1024 维 / Ollama；LLM：MiniMax M2.7（OpenAI 兼容）。

---

## 13. 实施进度

| 阶段 | 状态 | 说明 |
|------|------|------|
| 1. Spike（R1/R2） | ✅ | LanceDB crate 打开旧库；`LBUG+` 完整 dump |
| 2. 迁移器 | ✅ | `migration/dump_graph.py` + `reflect-mem migrate`（含对账） |
| 3. 存储层 | 🟡 | 图 ✅ / 向量 ✅ / 关系层待接 / 会话缓存待建 |
| 4. recall | ✅ | `SUMMARIES` + `GRAPH_COMPLETION` 均已跑通（CLI） |
| 5. remember | ✅ | 永久 ETL（会话快路径待做） |
| 6. forget | ✅ | provenance 分区删除 + doctor 修复 |
| 7. MCP 层 | 🟡 | rmcp + stdio ✅（recall/remember/forget）；streamable HTTP 待做 |
| 8. 切换 | ⬜ | 迁移 → Rust 独占 → Python 退役 |

已落地模块：`config` / `storage::graph` / `storage::vector` / `migrate` / `embed` / `llm` / `recall` / `mcp`。

**注意**：MCP 目前只暴露 `recall`（读路径）。`remember`/`forget`（写路径）**刻意未实现**——写必须逐字匹配 cognee 的图/向量 schema，仓促实现会污染真实的 651MB 记忆库。

实测记录（2026-09-20）：

- 图：6340 节点 / 18400 边迁移完成，多跳遍历正确。
- 向量：7 张表原地可读可检索，1024 维一致。
- recall：`SUMMARIES` 与 `GRAPH_COMPLETION` 端到端跑通；后者能答出前者漏掉的图内事实（如「不提交生成物」）。

### 13.1 MCP 验证（2026-09-20）

- `initialize` 握手成功：serverInfo=`reflect-mem 0.1.0`，协议协商到 `2025-11-25`。
- `tools/list` 返回 `recall` 工具。
- `tools/call recall {search_type: GRAPH_COMPLETION}` 返回正确的多跳答案。

`graph.sqlite` 已迁移到正式位置：`~/.agents/reflect-mem/system/databases/graph.sqlite`（新增文件，不触碰 cognee 原有任何文件）。

### 13.2 写路径规格（已从 cognee 源码 + 真实数据双重验证）

**确定性 ID**（`norm = lowercase + 空格→_ + 去撇号`，NAMESPACE_OID）：

- `Entity` id = uuid5("Entity:" + norm(name))
- `EntityType` id = uuid5("EntityType:" + norm(type))
- `EdgeType` id = uuid5("EdgeType:" + norm(rel))
- `edge_object_id` = uuid5(norm(source_id + rel + target_id))
- `TextSummary` id = uuid5(chunk_id, "TextSummary")
- DocumentChunk / TextDocument：uuid4（随机）

**节点 rank**：Entity=0、EntityType=0、TextDocument=1、DocumentChunk=2、TextSummary=3。

**图结构**（实测直方图）：`DocumentChunk -[contains]-> Entity`、`Entity -[is_a]-> EntityType`、`Entity -[LLM关系]-> Entity`、`TextSummary -[made_from]-> DocumentChunk`、`DocumentChunk -[is_part_of]-> TextDocument`。

**边属性**：`{source_node_id, target_node_id, relationship_name, updated_at "%Y-%m-%d %H:%M:%S", edge_object_id, feedback_weight 0.5, [relationship_type], [edge_text]}`。edge_text 兜底 = `{src} {rel去下划线} {tgt}.`（is_a 即 "X is a Y."）；contains 的 edge_text = "Document chunk mentions {name}: {desc}"。

**LLM 抽取**：system = `generate_graph_prompt.txt`（已全文拿到），响应 = `KnowledgeGraph{nodes:[{id,name,type,description}], edges:[{source_node_id,target_node_id,relationship_name,description}]}`，每 chunk 一次。

**摘要**：prompt = `summarize_content.txt`（两段式：类别 + 独立事实，≤200 tokens），每 chunk 一个 TextSummary。

**LanceDB 写入**：表名 = `{类型}_{index_field}`；嵌入文本 = index_fields 的值（Entity→name、Chunk/Summary→text、EdgeType→relationship_name）；payload 为固定 22 字段 union（id Utf8 / created_at i64 / ... / chunk_index i64 / source_chunk_id Utf8），缺失字段写 null，向量列 FixedSizeList(1024, f32)。

### 13.3 写/删路径实测（2026-09-20）

- `remember`：15.4s（LLM 为主：抽取 9.7s + 摘要 5.6s），14 datapoints 全部落库。
- 确定性 ID 实测：新写入 `Entity(reflect-mem)` = uuid5 推导值，逐字节一致。
- `forget`：provenance 分区正确——共享实体只 detach（保留），无主节点硬删（图/向量/关系层三库同步）。
- 完整 write→forget 循环后，图与向量表**精确回到基线**（6340/18399；向量经 heal 去重后 5100/577/271/271/120/122）。
- `doctor`：从迁移 dump 恢复丢失节点、清理悬挂边、按图 heals 向量表（补缺、清孤、同 id 去重）。

**修复过的 bug**（都值得记住）：
1. `ensure_dataset` 重入 Mutex 自死锁——"6 分钟没反应"的真相。
2. 写路径用 REPLACE 覆盖共享实体的 provenance → forget 误删共享实体。已改为**合并** refs。
3. forget 分区逻辑最初用「ref 出现次数」而非「去掉后是否无主」判断，同样会误删共享实体。
