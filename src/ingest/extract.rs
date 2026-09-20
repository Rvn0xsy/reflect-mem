//! LLM extraction and summarization, using cognee's own prompts and response
//! schemas (see `docs/design.md` §13.2).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::llm::LlmClient;

/// cognee's default extraction system prompt
/// (`cognee/infrastructure/llm/prompts/generate_graph_prompt.txt`).
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

/// cognee's `summarize_content.txt`.
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

/// The `KnowledgeGraph` response schema (`cognee.shared.data_models`).
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

/// Parse a model response into JSON, tolerating ``` fences and reasoning tails.
fn parse_loose<T: for<'de> Deserialize<'de>>(raw: &str) -> Result<T> {
    let mut text = raw.trim();
    if let Some(idx) = text.rfind("</think>") {
        text = text[idx + "</think>".len()..].trim();
    }
    let stripped = text
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // If the model wrapped the JSON in prose, take the outermost braces.
    let candidate = if stripped.starts_with('{') {
        stripped
    } else {
        match (stripped.find('{'), stripped.rfind('}')) {
            (Some(a), Some(b)) if b > a => &stripped[a..=b],
            _ => stripped,
        }
    };
    serde_json::from_str(candidate).with_context(|| format!("parsing LLM JSON: {candidate:.200}"))
}

/// Extract a `KnowledgeGraph` from one chunk of text.
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
    let graph: ExtractedGraph = parse_loose(&raw)?;
    if graph.nodes.is_empty() {
        bail!("extraction returned no nodes");
    }
    Ok(graph)
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
}
