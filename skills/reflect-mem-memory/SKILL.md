---
name: reflect-mem-memory
description: >-
  How to use the reflect-mem long-term memory MCP tools (`mcp_reflect_mem_remember` / `mcp_reflect_mem_recall` /
  `mcp_reflect_mem_forget`) — when to recall, remember, and forget, the two retrieval modes, the gotchas, and worked
  examples. Load this skill whenever the conversation seems to be missing context, the user references something from
  earlier or asks you to remember/forget information, you are about to make a remember/recall/forget call, or the
  memory policy in your system prompt tells you to consult it. The memory backend is the Rust `reflect-mem`
  server. Treat the memory tool as active context:
  check it before answering rather than treating it as an optional side feature.
---

# reflect-mem memory tools

`mcp_reflect_mem_*` gives you long-term memory that survives across conversations. Treat it as active context, not a
background feature: checking it is cheap, and the payoff is reusing what the user already told you instead of asking
again or losing it.

> **Migration note:** all memory calls now go to `reflect-mem`. Use `mcp_reflect_mem_*` only.

## The three tools

| Tool | Purpose |
|------|---------|
| `mcp_reflect_mem_recall` | Search memory and answer a question from it |
| `mcp_reflect_mem_remember` | Store text as permanent memory (entity extraction → knowledge graph) |
| `mcp_reflect_mem_forget` | Delete one item, one dataset, or everything (irreversible) |

## When to act

- **Answering a question about the user, their projects, preferences, or prior decisions** → recall first, before
  anything else. If memory has nothing relevant, say so honestly and fall back to your own knowledge or the web.
- **Learning something reusable** → remember it, so a future conversation can pick it up. Add a timestamp and a topic
  tag when they help.
- **The user asks you to forget** → forget, and confirm the exact scope before removing.
- **Never** delete memory on your own initiative; `forget` runs only on an explicit request.

## recall — read from memory

Arguments:

| Arg | Required | Notes |
|-----|----------|-------|
| `query` | yes | Natural-language question |
| `search_type` | no | `SUMMARIES` (default, aliases `SUMMARY`) or `GRAPH_COMPLETION` (alias `GRAPH`). Case-insensitive |
| `top_k` | no | Default 5, clamped to 1–50 |
| `hops` | no | Graph expansion depth, only for `GRAPH_COMPLETION`. Default 2, capped at 6 |

Modes:

- **SUMMARIES** — vector search over pre-computed summaries. Fast, good for "what did I say about X".
- **GRAPH_COMPLETION** — seeds entities from vector search, then walks the graph (`hops`) and synthesises. Use when
  the answer needs facts stitched together across several memories (person → org → project → dates).

**Hard context-budget rules:**

- Always pass `search_type = "SUMMARIES"` (preferred) or `"GRAPH_COMPLETION"`.
- **There is no `CHUNKS`, no RAG mode, no temporal mode.** `CHUNKS` is a parse error, not a fallback — do not try it.
- Keep `top_k` small (5 is the default and is usually right).
- Only fall back to your own knowledge or a web search when memory really has nothing.

Empty result: the tool returns the literal string `未找到相关记忆。`. Relay that honestly ("未找到相关记忆") and ask
for the missing detail — never invent an answer to fill the gap.

## remember — write to memory

Arguments:

| Arg | Required | Notes |
|-----|----------|-------|
| `data` | yes | The text to store. Plain text only — one blob per call |
| `dataset_name` | no | Defaults to `main_dataset`. Use a separate dataset for unrelated domains |
| `custom_prompt` | no | Hint that steers entity extraction (use it to name the entities you care about) |

Behaviour:

- Text is chunked (~1500 tokens/chunk), then each chunk is LLM-extracted into a graph and LLM-summarized. It is
  **slow** (seconds to tens of seconds per chunk) and writes real data to disk.
- **Only `data` exists.** There is no file ingestion, no session-cache staging, and no background async ingest mode
  in these tools — write the facts as text yourself.
- **Idempotent by content hash** — storing the same text into the same dataset is a no-op and returns
  `Already stored (dataset=…, data_id=…)`.
- Success returns a report: `Stored in dataset 'main_dataset': 1 chunk(s), +7 graph nodes, +39 edges, 21 vectors.
  data_id=e4988f9a-…`. **Note that `data_id`** — it is what you pass to `forget`.

**Gotcha — `extraction returned no nodes`:** the extractor can fail on prose-y, prompt-like, or loosely structured
text. It is not fatal and not a memory corruption — just rewrite the content as short, explicit, declarative sentences
(one fact per sentence, name the entities directly) and call `remember` again. Example of what works:

```
示例用户的博客主站是 blog.example.com。
blog.example.com 托管在 GitHub Pages 上，是通过自定义域名绑定的。
旧地址 alice.github.io 和 blog.example.com 是同一个博客站点，不是两个独立的博客。
```

Do not store a correction as a vague one-liner like `更新一下博客信息` — the extractor needs the fact spelled out.

## forget — delete from memory

Provide exactly one target:

| Form | Effect |
|------|--------|
| `data_id` + `dataset` | Remove one item (the `data_id` from `remember`) |
| `dataset` | Forget everything in that dataset |
| `dataset_id` | Same as above, by dataset UUID |
| `everything: true` | Wipe all memory |

Shared entities still referenced by surviving memories are kept; only the reference is detached. **Irreversible** —
confirm scope with the user before running, and report what was removed.

## Rules

- **Privacy** — never store sensitive info (passwords, ID numbers, tokens) unless the user forces it.
- **Miss handling** — when recall returns `未找到相关记忆。`, say so plainly (
  「未找到相关记忆」) and ask for the missing detail. Never paper over the gap.
- **Batching** — `remember` takes one `data` blob per call; several calls are fine. `recall` takes one mode per call.
- **Corrections** — when the user corrects a fact, `remember` the corrected fact explicitly (state that the old
  version was wrong, e.g. "A 和 B 是同一个站，不是两个独立的博客"), then verify with a `recall`.

## Worked examples

- User: 「我之前提到的项目截止日是什么时候？」(no context)
  → `recall(query="项目截止日", search_type="SUMMARIES", top_k=5)` → 「您曾在 2026-08-20 提及项目截止日为 2026-09-15。」
- User: 「请记住我的偏好：我喜欢简洁的回答。」
  → `remember(data="用户偏好简洁回答。")` → 已记录。
- User: 「我的博客信息有么？」
  → `recall(query="用户的博客信息：博客地址、域名、名称、技术栈", search_type="GRAPH_COMPLETION", top_k=10)`
- User corrects you: 「blog.example.com 是主站，就是跑在 GitHub Pages 上的」
  → `remember` the corrected facts as declarative sentences, then `recall` to confirm. (This exact correction is
  stored and known to work.)
