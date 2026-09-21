//! OpenAI-compatible chat-completions client.
//!
//! Defaults match the Python `.env`: MiniMax M2.7 behind an OpenAI-compatible
//! endpoint. Used for `recall` synthesis (SUMMARIES / GRAPH_COMPLETION) and,
//! later, entity extraction in the write path.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_ENDPOINT: &str = "https://api.minimaxi.com/v1";
const DEFAULT_MODEL: &str = "MiniMax-M2.7-highspeed";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Map the `LLM_THINKING` value to a boolean. `disabled` / `off` / `false` /
/// `none` turn thinking off; everything else (empty, `adaptive`, `on`, …)
/// keeps the provider default.
fn thinking_disabled(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "disabled" | "off" | "false" | "0" | "none"
    )
}

/// litellm-style configs prefix the provider (`openai/MiniMax-M2.7-highspeed`);
/// a raw OpenAI-compatible endpoint wants the bare model id.
fn bare_model(model: &str) -> &str {
    model.rsplit_once('/').map(|(_, m)| m).unwrap_or(model)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
    /// MiniMax returns reasoning separately when `reasoning_split` is on; we
    /// fall back to it only if `content` is empty.
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl Message {
    fn text(&self) -> Option<String> {
        let raw = self
            .content
            .as_ref()
            .filter(|c| !c.trim().is_empty())
            .or(self.reasoning_content.as_ref())?;
        Some(strip_reasoning(raw))
    }
}

/// MiniMax M2.x is a reasoning model: unless `reasoning_split` is requested it
/// emits its chain of thought inline, terminated by `</think>`, ahead of the
/// real answer. Keep only what follows that marker.
fn strip_reasoning(text: &str) -> String {
    if let Some(idx) = text.rfind("</think>") {
        let tail = text[idx + "</think>".len()..].trim();
        if !tail.is_empty() {
            return tail.to_string();
        }
    }
    text.trim().to_string()
}

/// Chat-completions client.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
    /// Extra request fields, merged from `LLM_ARGS` (e.g. MiniMax's
    /// `{"reasoning_split": true}`).
    extra_args: serde_json::Value,
    /// When true, send `thinking: {"type": "disabled"}` so the model skips
    /// chain-of-thought and answers directly.
    disable_thinking: bool,
}

impl LlmClient {
    /// Build from `LLM_ENDPOINT` / `LLM_MODEL` / `LLM_API_KEY` / `LLM_ARGS` /
    /// `LLM_THINKING`. `LLM_THINKING=disabled` (also `off` / `false` / `none`)
    /// turns chain-of-thought off; anything else keeps the provider default
    /// (thinking on).
    pub fn from_env() -> Result<Self> {
        let base_url = env_or("LLM_ENDPOINT", DEFAULT_ENDPOINT);
        let model = env_or("LLM_MODEL", DEFAULT_MODEL);
        let api_key = std::env::var("LLM_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .context("LLM_API_KEY is not set")?;
        let extra_args = std::env::var("LLM_ARGS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| serde_json::from_str(&v).context("LLM_ARGS must be a JSON object"))
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({}));
        let disable_thinking = thinking_disabled(&std::env::var("LLM_THINKING").unwrap_or_default());
        Self::with_extra_args(base_url, model, api_key, extra_args, disable_thinking)
    }

    pub fn new(base_url: String, model: String, api_key: String) -> Result<Self> {
        Self::with_extra_args(base_url, model, api_key, serde_json::json!({}), false)
    }

    fn with_extra_args(
        base_url: String,
        model: String,
        api_key: String,
        extra_args: serde_json::Value,
        disable_thinking: bool,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            extra_args,
            disable_thinking,
        })
    }

    pub fn model(&self) -> &str {
        bare_model(&self.model)
    }

    fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Single-turn completion. Returns the assistant text.
    pub async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut body = serde_json::json!({
            "model": self.model(),
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": 0.0,
        });
        if let Some(extra) = self.extra_args.as_object() {
            for (k, v) in extra {
                body[k] = v.clone();
            }
        }
        if self.disable_thinking {
            body["thinking"] = serde_json::json!({ "type": "disabled" });
        }
        let resp = self
            .http
            .post(self.completions_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("calling chat completions")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("LLM request failed ({status}): {body}");
        }

        let parsed: ChatResponse = resp.json().await.context("parsing LLM response")?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.text())
            .context("LLM response contained no message content")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_inline_chain_of_thought() {
        let raw = "先想一下...\n</think>\n\n最终答案是这样。";
        assert_eq!(strip_reasoning(raw), "最终答案是这样。");
    }

    #[test]
    fn keeps_plain_answers_untouched() {
        assert_eq!(strip_reasoning("  直接答案  "), "直接答案");
    }

    #[test]
    fn falls_back_when_only_reasoning_is_present() {
        assert_eq!(strip_reasoning("思考中</think>"), "思考中</think>");
    }

    #[test]
    fn strips_provider_prefix() {
        assert_eq!(
            bare_model("openai/MiniMax-M2.7-highspeed"),
            "MiniMax-M2.7-highspeed"
        );
        assert_eq!(
            bare_model("MiniMax-M2.7-highspeed"),
            "MiniMax-M2.7-highspeed"
        );
        assert_eq!(bare_model("openai/qwen3:14b"), "qwen3:14b");
    }

    #[test]
    fn completions_url_has_no_double_slash() {
        let c = LlmClient::new(
            "https://api.minimaxi.com/v1/".into(),
            "m".into(),
            "k".into(),
        )
        .unwrap();
        assert_eq!(
            c.completions_url(),
            "https://api.minimaxi.com/v1/chat/completions"
        );
    }

    #[test]
    fn thinking_flag_parses_off_values() {
        for v in ["disabled", "off", "false", "0", "none", "  DISABLED "] {
            assert!(thinking_disabled(v), "{v} should disable thinking");
        }
    }

    #[test]
    fn thinking_flag_keeps_default_on() {
        for v in ["", "adaptive", "on", "true", "enabled", "garbage"] {
            assert!(!thinking_disabled(v), "{v:?} should keep thinking on");
        }
    }
}
