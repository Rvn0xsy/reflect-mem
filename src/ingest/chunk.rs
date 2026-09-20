//! Text chunking.
//!
//! cognee's default is `TextChunker` with `chunk_size ≈ 1500` tokens on
//! paragraph/sentence boundaries. Without the Qwen tokenizer we estimate
//! tokens (CJK ≈ 1/char, other ≈ 1/4 chars) — semantically compatible per
//! design decision D12, not byte-identical.

use uuid::Uuid;

/// A chunk of the source document.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub id: Uuid,
    pub text: String,
    pub chunk_size: usize,
    pub chunk_index: usize,
    /// cognee's cut types: `paragraph_end` / `sentence_end` / `sentence_cut`.
    pub cut_type: String,
}

/// Rough token estimate without a tokenizer.
pub fn estimate_tokens(text: &str) -> usize {
    let cjk = text.chars().filter(|c| is_cjk(*c)).count();
    let other = text.chars().count() - cjk;
    cjk + other.div_ceil(4)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK unified
        | 0x3400..=0x4DBF // ext A
        | 0x3000..=0x303F // punctuation
        | 0xFF00..=0xFFEF // fullwidth
        | 0x3040..=0x30FF // kana
        | 0xAC00..=0xD7AF // hangul
    )
}

/// Split into sentence-ish pieces, keeping the terminator.
fn sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in paragraph.chars() {
        current.push(c);
        if matches!(c, '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；' | '\n') {
            if current.trim().is_empty() {
                continue;
            }
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Chunk `text` into pieces of at most `max_tokens` estimated tokens.
pub fn chunk_text(text: &str, max_tokens: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_tokens = 0usize;
    let mut ended_at_paragraph = false;

    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();
    for paragraph in paragraphs.iter() {
        for sentence in sentences(paragraph) {
            let st = estimate_tokens(&sentence);
            if current_tokens + st > max_tokens && !current.trim().is_empty() {
                let cut = if ended_at_paragraph {
                    "paragraph_end"
                } else {
                    "sentence_end"
                };
                chunks.push(finish(&current, cut, chunks.len()));
                current.clear();
                current_tokens = 0;
            }
            current.push_str(&sentence);
            current_tokens += st;
            ended_at_paragraph = false;
        }
        current.push('\n');
        ended_at_paragraph = true;
        // A single paragraph overflowing the budget: flush it hard.
        if current_tokens > max_tokens {
            chunks.push(finish(&current, "sentence_cut", chunks.len()));
            current.clear();
            current_tokens = 0;
            ended_at_paragraph = false;
        }
    }
    if !current.trim().is_empty() {
        let cut = "paragraph_end";
        chunks.push(finish(&current, cut, chunks.len()));
    }
    chunks
}

fn finish(text: &str, cut_type: &str, index: usize) -> Chunk {
    Chunk {
        id: Uuid::new_v4(),
        text: text.trim().to_string(),
        chunk_size: estimate_tokens(text),
        chunk_index: index,
        cut_type: cut_type.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_token_budget() {
        let para = "这是一个测试段落。".repeat(400); // ~3600 CJK tokens
        let chunks = chunk_text(&para, 1500);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(
                estimate_tokens(&c.text) <= 1500 + 200,
                "chunk {} too large: {}",
                c.chunk_index,
                estimate_tokens(&c.text)
            );
        }
    }

    #[test]
    fn keeps_short_text_in_one_chunk() {
        let chunks = chunk_text("短文本，一段话。", 1500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[0].cut_type, "paragraph_end");
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("  \n\n  ", 1500).is_empty());
    }
}
