//! ai-imessage: local-first Apple Messages RAG index and MCP server.
//!
//! Implemented so far: read-only extraction from the Apple Messages
//! database, diagnostics (`doctor`), a dry-run ETL report, and incremental
//! sync into a normalized local index. No search, embeddings, MCP server,
//! or LaunchAgent yet.

pub mod appledate;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod dryrun;
pub mod etl;
pub mod extract;
pub mod handles;
pub mod index;
pub mod model;
pub mod paths;
pub mod typedstream;
