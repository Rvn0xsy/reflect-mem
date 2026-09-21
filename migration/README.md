# migration — 一次性图迁移

把旧记忆服务的 `LBUG+` 图导出成 JSONL，供 `reflect-mem migrate` 导入 `reflect-mem.graph.sqlite`。

**这是一次性工具**，跑完即可连同 Python 环境一起删除，不进交付物。

## 为什么需要它

图库 `cognee_graph_ladybug` 的魔数是 `LBUG+`，是 `ladybug`（Kuzu 的 C++ fork）的私有格式，Rust 无法读取。其余数据层（源文本 / `reflect-mem.sqlite` / `reflect-mem.lancedb`）都能原地复用，只有这一层要搬。

## 用法

```bash
cd migration
python -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt          # ladybug==0.19.0，与原实现钉的版本一致
python dump_graph.py --out ./out
```

默认从 `~/.agents/reflect-mem/system/databases/cognee_graph_ladybug` 读，可用 `--graph` 覆盖。

## 安全

**打开图的 ladybug 版本必须与写它的版本一致**，否则会在原地迁移磁盘格式。因此脚本：

- 默认先**快照**图文件到临时目录（`--no-copy` 可关闭），只从快照 dump，原始文件不动；
- `requirements.txt` 钉死 `ladybug==0.19.0`。

实测结果（2026-09-20）：6340 节点 / 18400 边 / 2 条 metadata，storage version 43。

## 产物

| 文件 | 内容 |
|------|------|
| `nodes.jsonl` | 每行一个节点：`id/name/type/created_at/updated_at/properties/source_*`  |
| `edges.jsonl` | 每行一条边：`from_id/to_id/relationship_name/created_at/updated_at/properties/source_*` |
| `metadata.jsonl` | `GraphMetadata` 键值对 |
| `summary.json` | 计数与来源，供导入端对账 |

导入：

```bash
reflect-mem migrate --input migration/out --graph ~/.agents/reflect-mem/system/databases/reflect-mem.graph.sqlite
```

导入按 `summary.json` 对账节点/边计数，不一致即报错。
