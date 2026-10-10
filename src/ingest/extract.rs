//! LLM extraction and summarization, using the original prompts and response
//! schemas.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::llm::LlmClient;

/// The extraction system prompt
/// (the original `generate_graph_prompt.txt`).
const EXTRACT_SYSTEM: &str = r#"You are a top-tier algorithm designed for extracting information in structured formats to build a knowledge graph.
**Nodes** represent entities and concepts. They're akin to Wikipedia nodes.
**Edges** represent relationships between concepts. They're akin to Wikipedia links.
Every edge should include a description when the text supports relevant
information about the endpoints. The description must use the endpoint names,
stay dry and efficient, and may include useful qualifiers from the source text.
Do not add outside knowledge.
  - Good: Alice works at Acme as a platform engineer on the search team.
  - Bad: This edge describes an employment relationship.

The aim is to achieve simplicity and clarity in the knowledge graph.
# 1. Labeling Nodes
**Consistency**: Ensure you use basic or elementary types for node labels.
  - For example, when you identify an entity representing a person, always label it as **"Person"**.
  - Avoid using more specific terms like "Mathematician" or "Scientist", keep those as "profession" property.
  - Don't use too generic terms like "Entity".
**Node IDs**: Never utilize integers as node IDs.
  - Node IDs should be names or human-readable identifiers found in the text.
**Node Names**: Every node MUST include a "name" field.
  - Use the most complete human-readable name for the entity (e.g., "Albert Einstein", "Python").
# 2. Handling Numerical Data and Dates
  - For example, when you identify an entity representing a date, make sure it has type **"Date"**.
  - Extract the date in the format "YYYY-MM-DD"
  - If not possible to extract the whole date, extract month or year, or both if available.
  - **Property Format**: Properties must be in a key-value format.
  - **Quotation Marks**: Never use escaped single or double quotes within property values.
  - **Naming Convention**: Use snake_case for relationship names, e.g., `acted_in`.
# 3. Coreference Resolution
  - **Maintain Entity Consistency**: When extracting entities, it's vital to ensure consistency.
  If an entity, is mentioned multiple times in the text but is referred to by different names or pronouns,
  always use the most complete identifier for that entity throughout the knowledge graph.
Remember, the knowledge graph should be coherent and easily understandable, so maintaining consistency in entity references is crucial.
# 4. Strict Compliance
Adhere to the rules strictly. Non-compliance will result in termination.

Respond with ONLY a JSON object of this exact shape, no markdown fences:
{"nodes": [{"id": "entity name", "name": "entity name", "type": "EntityType", "description": "..."}], "edges": [{"source_node_id": "entity name", "target_node_id": "entity name", "relationship_name": "snake_case_name", "description": "one-sentence fact"}]}"#;

/// the original `summarize_content.txt`.
const SUMMARIZE_SYSTEM: &str = r#"Summarize the chunk for retrieval.

Output two sections only.

First section:
This chunk is about:
- <Category>: <names or topics>
- <Category>: <names or topics>

First-section rules:
1. List entity/topic categories. Do not list facts here.
2. Use only clear, useful categories.
3. Good categories include People, Companies, Organizations, Places, Roles, Projects, Products, Systems, Concepts, Events, and Topics.
4. Keep category lines short.

Second section:
Facts:
- <self-contained fact>
- <self-contained fact>

Second-section rules:
1. Write complete sentences with clear subjects from the first section.
2. Each fact must stand alone without the chunk or the other facts.
3. Order facts by: time first, category second, entity/topic third.
4. Do not group all facts about one entity if that makes the facts jump backward or forward in time.
5. Make sure the facts cover the full content of the chunk.
6. Do not invent.

Max 200 tokens.

Respond with ONLY a JSON object of this exact shape, no markdown fences:
{"summary": "the summary text"}"#;

/// The `KnowledgeGraph` response schema (the original data models).
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractedNode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type", default)]
    pub type_: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractedEdge {
    #[serde(rename = "source_node_id")]
    pub source_node_id: String,
    #[serde(rename = "target_node_id")]
    pub target_node_id: String,
    #[serde(rename = "relationship_name")]
    pub relationship_name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExtractedGraph {
    #[serde(default)]
    pub nodes: Vec<ExtractedNode>,
    #[serde(default)]
    pub edges: Vec<ExtractedEdge>,
}

/// Parse a model response into JSON, tolerating ``` fences, prose around the
/// object, reasoning tails, and **extra content after the object**.
///
/// The last part matters: the previous implementation took the
/// `first '{' ..= last '}'` span, so a repeated/second object or a stray
/// trailing brace turned a perfectly good first object into
/// `trailing characters` / `expected \`:\`` errors. Those are exactly the
/// failures observed on dense Notion exports, and they cost a whole item.
fn parse_loose<T: for<'de> Deserialize<'de>>(raw: &str) -> Result<T> {
    let mut text = raw.trim();
    if let Some(idx) = text.rfind("</think>") {
        text = text[idx + "</think>".len()..].trim();
    }
    if let Ok(parsed) = serde_json::from_str(text) {
        return Ok(parsed);
    }
    let candidate = first_json_object(text)
        .context("the LLM response holds no balanced JSON object (truncated?)")?;
    serde_json::from_str(candidate).with_context(|| format!("parsing LLM JSON: {candidate:.200}"))
}

/// The first balanced `{…}` object in `raw`, ignoring prose before it, code
/// fences around it, and anything after it (a second object, a trailing brace, an
/// explanation, a reasoning tail).
///
/// `None` means there is no complete object — the response was cut off mid-JSON,
/// which the caller can retry or work around by extracting smaller pieces.
pub fn first_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in raw[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract a `KnowledgeGraph` from one chunk of text.
///
/// An extraction with **no nodes is not an error**: a table of numbers, an index
/// page or a block of URLs genuinely has no entities to link, and the chunk's
/// summary is still worth storing (SUMMARIES recall reads summaries, not the
/// graph). This used to `bail!`, which failed the whole item — the reason the
/// 2026-08-26 Notion batches never landed.
pub async fn extract_graph(
    llm: &LlmClient,
    text: &str,
    custom_prompt: Option<&str>,
) -> Result<ExtractedGraph> {
    let system = custom_prompt
        .map(|p| {
            format!("{p}\n\nRespond with ONLY a JSON object of the exact schema described above.")
        })
        .unwrap_or_else(|| EXTRACT_SYSTEM.to_string());
    let raw = llm.complete(&system, text).await?;
    parse_loose(&raw)
}

/// How many times one chunk is extracted before falling back to splitting it.
const EXTRACT_ATTEMPTS: usize = 2;
/// How many times a chunk may be halved. Bounds the work on a pathological chunk.
const EXTRACT_MAX_SPLIT_DEPTH: usize = 2;

/// Extract a chunk's graph, surviving the extractor's occasional malformed or
/// truncated JSON.
///
/// Dense chunks — Notion metadata/CSV exports, tables — are the ones that break:
/// the model's response is long enough to be cut off mid-object, or it emits a
/// second object afterwards. Both used to fail the whole item. Here the same call
/// is tried twice, and if that still fails the chunk is halved at a paragraph
/// boundary and each half extracted and merged: smaller inputs, shorter responses,
/// same facts. A chunk too small to split fails as before.
pub async fn extract_graph_resilient(
    llm: &LlmClient,
    text: &str,
    custom_prompt: Option<&str>,
) -> Result<ChunkGraph> {
    extract_with_fallback(llm, text, custom_prompt, 0).await
}

/// One chunk's extraction plus whether the provider refused the text.
#[derive(Debug)]
pub struct ChunkGraph {
    pub graph: ExtractedGraph,
    /// The provider refused this text: no entities and no summary exist for the
    /// chunk, but its text is still stored (a `DocumentChunk` carries it).
    pub refused: bool,
}

fn extract_with_fallback<'a>(
    llm: &'a LlmClient,
    text: &'a str,
    custom_prompt: Option<&'a str>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ChunkGraph>> + Send + 'a>> {
    Box::pin(async move {
        let mut last_err = None;
        let mut empty = None;
        for _ in 0..EXTRACT_ATTEMPTS {
            match extract_graph(llm, text, custom_prompt).await {
                Ok(graph) if !graph.nodes.is_empty() => {
                    return Ok(ChunkGraph {
                        graph,
                        refused: false,
                    });
                }
                // Keep it, but let a second attempt look for entities before we
                // settle for "nothing here".
                Ok(graph) => empty = Some(graph),
                Err(err) if crate::llm::is_input_refused(&err) => {
                    // No retry and no split: the same text is refused every time
                    // (verified down to ~400 chars), so this chunk simply cannot
                    // be extracted. Keep it for its text, lose its entities.
                    eprintln!(
                        "[extract] provider refused {} chars ({err}) — keeping the chunk without \
                         entities; its text is still stored in the graph",
                        text.chars().count()
                    );
                    return Ok(ChunkGraph {
                        graph: ExtractedGraph::default(),
                        refused: true,
                    });
                }
                Err(err) => last_err = Some(err),
            }
        }
        if let Some(graph) = empty {
            eprintln!(
                "[extract] {} chars yielded no entities — keeping the chunk for its summary",
                text.chars().count()
            );
            return Ok(ChunkGraph {
                graph,
                refused: false,
            });
        }
        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("extraction failed"));

        if depth >= EXTRACT_MAX_SPLIT_DEPTH {
            return Err(err);
        }
        let Some((left, right)) = split_in_half(text) else {
            return Err(err);
        };
        eprintln!(
            "[extract] {} chars failed twice ({err}); splitting into {} + {} chars",
            text.chars().count(),
            left.chars().count(),
            right.chars().count(),
        );
        let left_graph = extract_with_fallback(llm, left, custom_prompt, depth + 1).await?;
        let right_graph = extract_with_fallback(llm, right, custom_prompt, depth + 1).await?;
        Ok(ChunkGraph {
            refused: left_graph.refused && right_graph.refused,
            graph: merge_graphs(left_graph.graph, right_graph.graph),
        })
    })
}

/// Split text into two useful halves at the paragraph break nearest the middle
/// (falling back to a character boundary).
///
/// `None` when the text is too short for both halves to be worth a call — which
/// is also what bounds the split recursion.
pub fn split_in_half(text: &str) -> Option<(&str, &str)> {
    const MIN_HALF_CHARS: usize = 400;
    let n = text.chars().count();
    if n < MIN_HALF_CHARS * 2 {
        return None;
    }
    let offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let mid_byte = offsets[n / 2];
    let cut = text
        .match_indices("\n\n")
        .map(|(i, _)| i)
        .min_by_key(|i| i.abs_diff(mid_byte))
        .unwrap_or(mid_byte);
    let (left, right) = text.split_at(cut);
    let (left, right) = (left.trim(), right.trim());
    if left.chars().count() < MIN_HALF_CHARS || right.chars().count() < MIN_HALF_CHARS {
        return None;
    }
    Some((left, right))
}

/// Concatenate two extractions from the same chunk, dropping the duplicates the
/// halves produce for entities that straddle the split.
pub fn merge_graphs(mut left: ExtractedGraph, right: ExtractedGraph) -> ExtractedGraph {
    let key = |n: &ExtractedNode| {
        if n.id.is_empty() {
            n.name.clone()
        } else {
            n.id.clone()
        }
    };
    let mut seen: std::collections::HashSet<String> = left.nodes.iter().map(key).collect();
    for node in right.nodes {
        if seen.insert(key(&node)) {
            left.nodes.push(node);
        }
    }
    let edge_key = |e: &ExtractedEdge| {
        format!(
            "{}|{}|{}",
            e.source_node_id, e.target_node_id, e.relationship_name
        )
    };
    let mut seen_edges: std::collections::HashSet<String> =
        left.edges.iter().map(edge_key).collect();
    for edge in right.edges {
        if seen_edges.insert(edge_key(&edge)) {
            left.edges.push(edge);
        }
    }
    left
}

/// Summarize one chunk (`summarize_content.txt` prompt → `{"summary": …}`).
#[derive(Debug, Deserialize)]
struct SummaryResponse {
    summary: String,
}

pub async fn summarize(llm: &LlmClient, text: &str) -> Result<String> {
    let raw = llm.complete(SUMMARIZE_SYSTEM, text).await?;
    let parsed: SummaryResponse = parse_loose(&raw)?;
    Ok(parsed.summary.trim().to_string())
}

/// Summarize a chunk, surviving the same truncated-JSON failure the extraction
/// path has.
///
/// The summary call is the quieter half of the same problem: it also asks for a
/// JSON object, and one truncated response used to fail the whole item — the
/// Notion batches died here after their extraction had already succeeded (no
/// `chunk N/M: summarized` line, just the parse error). Retry, then summarize
/// the two halves and join them.
pub async fn summarize_resilient(llm: &LlmClient, text: &str) -> Result<String> {
    summarize_with_fallback(llm, text, 0).await
}

fn summarize_with_fallback<'a>(
    llm: &'a LlmClient,
    text: &'a str,
    depth: usize,
) -> SummaryFuture<'a> {
    Box::pin(async move {
        let mut last_err = None;
        for _ in 0..EXTRACT_ATTEMPTS {
            match summarize(llm, text).await {
                Ok(summary) => return Ok(summary),
                Err(err) if crate::llm::is_input_refused(&err) => {
                    eprintln!(
                        "[summarize] provider refused {} chars ({err}) — no summary for this chunk",
                        text.chars().count()
                    );
                    return Ok(String::new());
                }
                Err(err) => last_err = Some(err),
            }
        }
        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("summarization failed"));
        if depth >= EXTRACT_MAX_SPLIT_DEPTH {
            return Err(err);
        }
        let Some((left, right)) = split_in_half(text) else {
            return Err(err);
        };
        eprintln!(
            "[summarize] {} chars failed twice ({err}); splitting into {} + {} chars",
            text.chars().count(),
            left.chars().count(),
            right.chars().count(),
        );
        let left_summary = summarize_with_fallback(llm, left, depth + 1).await?;
        let right_summary = summarize_with_fallback(llm, right, depth + 1).await?;
        Ok(format!("{left_summary}\n{right_summary}"))
    })
}

/// What the retry/split recursion for one summary returns.
type SummaryFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let g: ExtractedGraph = serde_json::from_str(
            r#"{"nodes":[{"id":"a","name":"A","type":"Person","description":"d"}],"edges":[]}"#,
        )
        .unwrap();
        assert_eq!(g.nodes[0].name, "A");
    }

    #[test]
    fn parse_loose_survives_fences_and_prose() {
        #[derive(Deserialize)]
        struct S {
            summary: String,
        }
        let s: S = parse_loose("```json\n{\"summary\": \"hi\"}\n```").unwrap();
        assert_eq!(s.summary, "hi");
        let s: S = parse_loose("好的，结果如下：{\"summary\": \"结果\"} 请查收。").unwrap();
        assert_eq!(s.summary, "结果");
    }

    #[test]
    fn parse_loose_strips_reasoning_tail() {
        #[derive(Deserialize)]
        struct S {
            summary: String,
        }
        let s: S = parse_loose("思考过程...</think>{\"summary\": \"答案\"}").unwrap();
        assert_eq!(s.summary, "答案");
    }

    /// The observed failure: the model appended a second object, and the old
    /// first-brace..last-brace span made the whole response unparseable.
    #[test]
    fn parse_loose_ignores_a_second_object() {
        #[derive(Deserialize)]
        struct S {
            summary: String,
        }
        let s: S = parse_loose(r#"{"summary": "first"}{"summary": "second"}"#).unwrap();
        assert_eq!(s.summary, "first");
        let s: S = parse_loose("{\"summary\": \"ok\"} } trailing brace").unwrap();
        assert_eq!(s.summary, "ok");
    }

    #[test]
    fn first_json_object_handles_nesting_and_strings() {
        // a `}` inside a string must not close the object, and `\"` must not
        // toggle the in-string state
        let raw = r#"pre {"a": {"b": "}"}, "c": "x}y"} post"#;
        assert_eq!(
            first_json_object(raw).unwrap(),
            r#"{"a": {"b": "}"}, "c": "x}y"}"#
        );
        let raw = r#"{"a": "he said \"hi\""} tail"#;
        assert_eq!(
            first_json_object(raw).unwrap(),
            r#"{"a": "he said \"hi\""}"#
        );
        assert_eq!(first_json_object("no object here"), None);
    }

    /// A response cut off mid-object has no balanced object; the caller retries
    /// or splits instead of failing the item outright.
    #[test]
    fn first_json_object_returns_none_when_truncated() {
        let raw = r#"{"nodes": [{"id": "a", "name": "A"}, {"id": "b""#;
        assert_eq!(first_json_object(raw), None);
        let err = parse_loose::<ExtractedGraph>(raw).unwrap_err();
        assert!(
            err.to_string().contains("no balanced JSON object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn split_in_half_prefers_paragraph_boundaries() {
        let text = format!("{}\n\n{}", "甲".repeat(600), "乙".repeat(600));
        let (left, right) = split_in_half(&text).unwrap();
        // the split lands on the paragraph break, and the break itself is trimmed
        assert_eq!(left.chars().count(), 600);
        assert_eq!(right.chars().count(), 600);
        assert!(left.ends_with('甲'));
        assert!(right.starts_with('乙'));
    }

    #[test]
    fn split_in_half_refuses_text_too_short_to_split() {
        assert!(split_in_half(&"短".repeat(300)).is_none());
        // one huge half and one tiny one is not a useful split either
        let lopsided = format!("甲\n\n{}", "乙".repeat(2000));
        assert!(split_in_half(&lopsided).is_none());
    }

    #[test]
    fn merge_graphs_drops_duplicate_nodes_and_edges() {
        let g = |names: &[&str]| ExtractedGraph {
            nodes: names
                .iter()
                .map(|n| ExtractedNode {
                    id: n.to_string(),
                    name: n.to_string(),
                    type_: "Entity".into(),
                    description: String::new(),
                })
                .collect(),
            edges: vec![ExtractedEdge {
                source_node_id: names[0].to_string(),
                target_node_id: names[names.len() - 1].to_string(),
                relationship_name: "related_to".into(),
                description: None,
            }],
        };
        let merged = merge_graphs(g(&["a", "shared"]), g(&["shared", "b"]));
        let ids: Vec<&str> = merged.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "shared", "b"]);
        assert_eq!(merged.edges.len(), 2, "the two halves' edges differ");
    }
}
