//! ai-imessage: local-first Apple Messages RAG index and MCP server.
//!
//! Milestone 1 scope: read-only extraction from the Apple Messages database,
//! diagnostics (`doctor`), and a dry-run ETL report. No destination index,
//! embeddings, MCP server, or LaunchAgent yet.

pub mod appledate;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod dryrun;
pub mod extract;
pub mod handles;
pub mod model;
pub mod paths;
pub mod typedstream;
