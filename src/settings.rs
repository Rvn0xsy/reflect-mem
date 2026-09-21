//! Runtime settings.
//!
//! Everything the binary needs is resolved once, at startup, from four layers
//! (highest priority first):
//!
//!   1. CLI flags (`--config`, and `mcp --bind` / `--token`)
//!   2. environment variables
//!   3. a TOML config file
//!   4. built-in defaults
//!
//! Keeping env vars ahead of the file means containers and CI can override a
//! checked-in config without editing it.
//!
//! The config file defaults to `<data_root>/config.toml`; the data root itself
//! defaults to `~/.agents/reflect-mem`. Point elsewhere with `--config` or
//! `REFLECT_MEM_CONFIG`.
//!
//! ```toml
//! data_root = "/Users/me/.agents/reflect-mem"
//!
//! [llm]
//! endpoint = "https://api.minimaxi.com/v1"
//! model = "MiniMax-M3"
//! api_key = "sk-..."
//! thinking = "disabled"          # or "adaptive"
//! # args = { reasoning_split = true }
//!
//! [embedding]
//! endpoint = "http://localhost:11434/api/embed"
//! model = "qwen3-embedding:0.6b"
//! dimensions = 1024
//!
//! [mcp]
//! transport = "stdio"            # or "streamable-http"
//! bind = "127.0.0.1:8080"
//! # token = "a-long-random-secret"
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_DATA_ROOT_DIR: &str = ".agents/reflect-mem";
const DEFAULT_LLM_ENDPOINT: &str = "https://api.minimaxi.com/v1";
const DEFAULT_LLM_MODEL: &str = "MiniMax-M2.7-highspeed";
const DEFAULT_EMBEDDING_ENDPOINT: &str = "http://localhost:11434/api/embed";
const DEFAULT_EMBEDDING_MODEL: &str = "qwen3-embedding:0.6b";
const DEFAULT_EMBEDDING_DIMENSIONS: usize = 1024;
const DEFAULT_MCP_TRANSPORT: &str = "stdio";
const DEFAULT_MCP_BIND: &str = "127.0.0.1:8080";

/// Fully resolved settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub data_root: PathBuf,
    pub llm: LlmSettings,
    pub embedding: EmbeddingSettings,
    pub mcp: McpSettings,
}

#[derive(Debug, Clone)]
pub struct LlmSettings {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub args: serde_json::Value,
    /// Skip MiniMax chain-of-thought (`thinking: {type: disabled}`).
    pub disable_thinking: bool,
}

#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    pub endpoint: String,
    pub model: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone)]
pub struct McpSettings {
    pub transport: String,
    pub bind: String,
    /// When set, streamable-HTTP requests must carry
    /// `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

// ---------------------------------------------------------------------------
// File schema — every field optional, so a partial config is valid.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    data_root: Option<PathBuf>,
    #[serde(default)]
    llm: FileLlm,
    #[serde(default)]
    embedding: FileEmbedding,
    #[serde(default)]
    mcp: FileMcp,
}

#[derive(Debug, Default, Deserialize)]
struct FileLlm {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct FileEmbedding {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    dimensions: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct FileMcp {
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    bind: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn default_data_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(DEFAULT_DATA_ROOT_DIR)
}

/// Map a `thinking` value to a boolean. `disabled` / `off` / `false` / `none`
/// turn chain-of-thought off; anything else (empty, `adaptive`, `on`) keeps the
/// provider default.
fn thinking_disabled(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "disabled" | "off" | "false" | "0" | "none"
    )
}

/// Where the config file lives: `--config` > `REFLECT_MEM_CONFIG` >
/// `<data_root>/config.toml`.
fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = env_opt("REFLECT_MEM_CONFIG") {
        return PathBuf::from(p);
    }
    let root = env_opt("DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_root);
    root.join("config.toml")
}

impl Settings {
    /// Resolve from file + environment. A missing config file is not an error;
    /// a present-but-unreadable one is.
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let path = resolve_config_path(config_path);
        let file = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str::<FileConfig>(&text)
                .with_context(|| format!("parsing config {}", path.display()))?
        } else {
            FileConfig::default()
        };
        Self::merge(file)
    }

    /// Pure compile-time defaults, ignoring env and files (test fallback and
    /// the `OnceLock` emergency path).
    pub fn builtin() -> Self {
        Self {
            data_root: default_data_root(),
            llm: LlmSettings {
                endpoint: DEFAULT_LLM_ENDPOINT.into(),
                model: DEFAULT_LLM_MODEL.into(),
                api_key: String::new(),
                args: serde_json::json!({}),
                disable_thinking: false,
            },
            embedding: EmbeddingSettings {
                endpoint: DEFAULT_EMBEDDING_ENDPOINT.into(),
                model: DEFAULT_EMBEDDING_MODEL.into(),
                dimensions: DEFAULT_EMBEDDING_DIMENSIONS,
            },
            mcp: McpSettings {
                transport: DEFAULT_MCP_TRANSPORT.into(),
                bind: DEFAULT_MCP_BIND.into(),
                token: None,
            },
        }
    }

    /// Overlay environment on top of the file config, then fill defaults.
    fn merge(f: FileConfig) -> Result<Self> {
        let data_root = env_opt("DATA_ROOT")
            .map(PathBuf::from)
            .or(f.data_root)
            .unwrap_or_else(default_data_root);

        let llm_endpoint = env_opt("LLM_ENDPOINT")
            .or(f.llm.endpoint)
            .unwrap_or_else(|| DEFAULT_LLM_ENDPOINT.into());
        let llm_model = env_opt("LLM_MODEL")
            .or(f.llm.model)
            .unwrap_or_else(|| DEFAULT_LLM_MODEL.into());
        let llm_api_key = env_opt("LLM_API_KEY").or(f.llm.api_key).unwrap_or_default();
        let thinking = env_opt("LLM_THINKING").or(f.llm.thinking);
        let llm_args = match env_opt("LLM_ARGS") {
            Some(raw) => serde_json::from_str(&raw).context("LLM_ARGS must be a JSON object")?,
            None => f.llm.args.unwrap_or_else(|| serde_json::json!({})),
        };

        let embed_endpoint = env_opt("EMBEDDING_ENDPOINT")
            .or(f.embedding.endpoint)
            .unwrap_or_else(|| DEFAULT_EMBEDDING_ENDPOINT.into());
        let embed_model = env_opt("EMBEDDING_MODEL")
            .or(f.embedding.model)
            .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.into());
        let embed_dims = match env_opt("EMBEDDING_DIMENSIONS") {
            Some(raw) => raw
                .parse::<usize>()
                .context("EMBEDDING_DIMENSIONS must be a number")?,
            None => f
                .embedding
                .dimensions
                .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS),
        };

        let mcp_transport = env_opt("MCP_TRANSPORT")
            .or(f.mcp.transport)
            .unwrap_or_else(|| DEFAULT_MCP_TRANSPORT.into());
        let mcp_bind = env_opt("MCP_BIND")
            .or(f.mcp.bind)
            .unwrap_or_else(|| DEFAULT_MCP_BIND.into());
        let mcp_token = env_opt("MCP_TOKEN").or(f.mcp.token);

        Ok(Self {
            data_root,
            llm: LlmSettings {
                endpoint: llm_endpoint,
                model: llm_model,
                api_key: llm_api_key,
                args: llm_args,
                disable_thinking: thinking.as_deref().is_some_and(thinking_disabled),
            },
            embedding: EmbeddingSettings {
                endpoint: embed_endpoint,
                model: embed_model,
                dimensions: embed_dims,
            },
            mcp: McpSettings {
                transport: mcp_transport,
                bind: mcp_bind,
                token: mcp_token,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Process-wide handle
// ---------------------------------------------------------------------------

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Resolve and install settings. Call once, at the top of `main`.
pub fn init(config_path: Option<&Path>) -> Result<()> {
    let settings = Settings::load(config_path)?;
    let _ = SETTINGS.set(settings);
    Ok(())
}

/// The active settings. Falls back to reloading (then to built-in defaults) if
/// [`init`] was never called — which only happens in tests and small tools.
pub fn get() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings::load(None).unwrap_or_else(|_| Settings::builtin()))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parses_a_full_toml_config() {
        let text = r#"
            data_root = "/tmp/rm-test"
            [llm]
            endpoint = "http://llm.test/v1"
            model = "MiniMax-M3"
            api_key = "sk-test"
            thinking = "disabled"
            [embedding]
            dimensions = 768
            [mcp]
            transport = "streamable-http"
            bind = "0.0.0.0:9999"
            token = "s3cret"
        "#;
        let f: FileConfig = toml::from_str(text).unwrap();
        let s = Settings::merge(f).unwrap();
        assert_eq!(s.data_root, PathBuf::from("/tmp/rm-test"));
        assert_eq!(s.llm.endpoint, "http://llm.test/v1");
        assert_eq!(s.llm.model, "MiniMax-M3");
        assert!(s.llm.disable_thinking);
        assert_eq!(s.embedding.dimensions, 768);
        assert_eq!(s.mcp.transport, "streamable-http");
        assert_eq!(s.mcp.bind, "0.0.0.0:9999");
        assert_eq!(s.mcp.token.as_deref(), Some("s3cret"));
    }

    #[test]
    fn partial_config_falls_back_to_defaults() {
        let f: FileConfig = toml::from_str("[llm]\nmodel = \"MiniMax-M3\"\n").unwrap();
        let s = Settings::merge(f).unwrap();
        assert_eq!(s.llm.model, "MiniMax-M3");
        assert_eq!(s.llm.endpoint, DEFAULT_LLM_ENDPOINT);
        assert_eq!(s.embedding.dimensions, DEFAULT_EMBEDDING_DIMENSIONS);
        assert_eq!(s.mcp.transport, "stdio");
    }
}
