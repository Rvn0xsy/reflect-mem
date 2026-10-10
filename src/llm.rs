//! OpenAI-compatible chat-completions client.
//!
//! Used for `recall` synthesis (SUMMARIES / GRAPH_COMPLETION) and entity
//! extraction in the write path. Configuration comes from [`crate::settings`].

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// litellm-style configs prefix the provider (`openai/MiniMax-M2.7-highspeed`);
/// a raw OpenAI-compatible endpoint wants the bare model id.
fn bare_model(model: &str) -> &str {
    model.rsplit_once('/').map(|(_, m)| m).unwrap_or(model)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

/// The provider refused this input — MiniMax answers `422 input new_sensitive`,
/// DeepSeek answers `200` with an empty message, and both do it for the *same*
/// region of a red-team notes file. It is not a transport failure and not a JSON
/// problem: retrying or splitting the text produces the same refusal (verified
/// down to ~400 chars), so callers must stop and treat the chunk as
/// un-extractable rather than failing the whole item.
#[derive(Debug)]
pub struct InputRefused {
    pub reason: String,
}

impl std::fmt::Display for InputRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the provider refused this input: {}", self.reason)
    }
}

impl std::error::Error for InputRefused {}

/// Whether an error means "the provider will never accept this text"
/// (as opposed to a transport hiccup or an unparseable answer, which are worth
/// retrying and splitting).
pub fn is_input_refused(err: &anyhow::Error) -> bool {
    err.downcast_ref::<InputRefused>().is_some()
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
    /// Chain-of-thought, when the provider returns it as its own field (MiniMax
    /// with `reasoning_split`). Kept for diagnostics only — it is never the
    /// answer, so it is never surfaced as one.
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl Message {
    /// The assistant's answer, with any inline chain-of-thought removed.
    ///
    /// `None` when the model produced no answer at all, so callers report that
    /// instead of showing the user the model's private reasoning.
    fn answer(&self) -> Option<String> {
        let answer = strip_reasoning(self.content.as_deref().unwrap_or_default());
        (!answer.is_empty()).then_some(answer)
    }

    /// True when the message carried reasoning but no answer — a retry, or
    /// `LLM_THINKING=disabled`, is what fixes it.
    fn reasoning_only(&self) -> bool {
        let had_reasoning = self
            .reasoning_content
            .as_ref()
            .is_some_and(|r| !r.trim().is_empty())
            || self.content.as_deref().is_some_and(has_reasoning);
        had_reasoning && self.answer().is_none()
    }
}

/// Extract the answer from a reply that may carry inline chain-of-thought.
///
/// MiniMax M2/M3 emit their reasoning inline, terminated by `</think>`, ahead
/// of the real answer. A reply that is *only* reasoning yields an empty string,
/// so a truncated or reasoning-only turn is reported as missing rather than
/// being handed to the user as the answer.
fn strip_reasoning(text: &str) -> String {
    if let Some(idx) = text.rfind("</think>") {
        return text[idx + "</think>".len()..].trim().to_string();
    }
    // Opening marker with no terminator: reasoning cut off mid-stream.
    if text.trim_start().starts_with("<think>") {
        return String::new();
    }
    text.trim().to_string()
}

/// Does this text carry chain-of-thought markers?
fn has_reasoning(text: &str) -> bool {
    text.contains("<think>") || text.contains("</think>")
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
    /// Build from the resolved settings (config file overlaid by env).
    pub fn from_settings() -> Result<Self> {
        let s = crate::settings::get();
        let api_key = s.llm.api_key.trim();
        if api_key.is_empty() {
            bail!("LLM API key is not set — configure `llm.api_key` or LLM_API_KEY");
        }
        Self::with_extra_args(
            s.llm.endpoint.clone(),
            s.llm.model.clone(),
            api_key.to_string(),
            s.llm.args.clone(),
            s.llm.disable_thinking,
        )
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
            // 4xx on the input is the provider's content filter, not a bad
            // request of ours; 5xx/429 are worth retrying.
            if status.as_u16() == 422
                || (status.is_client_error() && body.contains("new_sensitive"))
            {
                return Err(anyhow::Error::new(InputRefused {
                    reason: format!("{status}: {}", body.chars().take(200).collect::<String>()),
                }));
            }
            bail!("LLM request failed ({status}): {body}");
        }

        let parsed: ChatResponse = resp.json().await.context("parsing LLM response")?;
        let message = parsed
            .choices
            .into_iter()
            .next()
            .context("LLM returned no choices")?
            .message;
        if let Some(answer) = message.answer() {
            return Ok(answer);
        }
        if message.reasoning_only() {
            bail!(
                "the LLM returned reasoning but no answer — retry, or set LLM_THINKING=disabled \
                 to stop it emitting a chain-of-thought"
            );
        }
        // Same shape as MiniMax's 422: the provider dropped the answer instead of
        // saying no (DeepSeek does this for filtered content).
        Err(anyhow::Error::new(InputRefused {
            reason: "the provider returned an empty message".to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal signal must be distinguishable from a transport error: one is
    /// permanent for that text (skip the chunk), the other is worth retrying.
    #[test]
    fn refusal_is_distinguishable_from_other_errors() {
        let refused = anyhow::Error::new(InputRefused {
            reason: "422: input new_sensitive".into(),
        });
        assert!(is_input_refused(&refused));
        assert!(refused.to_string().contains("provider refused this input"));
        assert!(!is_input_refused(&anyhow::anyhow!("connection reset")));
    }

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
    fn reasoning_only_yields_no_answer() {
        // Regression: these used to be returned verbatim, leaking the model's
        // private chain-of-thought to the user as if it were the answer.
        assert_eq!(strip_reasoning("思考中</think>"), "");
        assert_eq!(strip_reasoning("<think>思考到一半被截断"), "");

        let m = Message {
            content: Some("<think>thinking</think>".into()),
            reasoning_content: None,
        };
        assert_eq!(m.answer(), None);
        assert!(m.reasoning_only());
    }

    #[test]
    fn separate_reasoning_field_is_never_the_answer() {
        let m = Message {
            content: Some(String::new()),
            reasoning_content: Some("only thinking here".into()),
        };
        assert_eq!(m.answer(), None);
        assert!(m.reasoning_only());
    }

    #[test]
    fn an_answer_is_preferred_over_separate_reasoning() {
        let m = Message {
            content: Some("the answer".into()),
            reasoning_content: Some("the thinking".into()),
        };
        assert_eq!(m.answer().as_deref(), Some("the answer"));
        assert!(!m.reasoning_only());
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
}
