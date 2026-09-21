//! reflect-mem — a long-term memory-management MCP server in Rust.
//!
//! See `docs/design.md` for the full design.

pub mod config;
pub mod doctor;
pub mod embed;
pub mod forget;
pub mod ingest;
pub mod llm;
pub mod mcp;
pub mod migrate;
pub mod recall;
pub mod remember;
pub mod settings;
pub mod storage;
