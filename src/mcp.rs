//! MCP (Model Context Protocol) server over stdio: newline-delimited
//! JSON-RPC 2.0, tools capability only, strictly read-only against the
//! local index.
//!
//! The handler is pure — one JSON message in, at most one JSON message
//! out — so the protocol logic is testable without spawning a process.
//! stdout carries protocol frames only; all logging goes to stderr.

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::index::IndexDb;
use crate::retrieve::{RetrievalParams, hybrid_search};

/// Protocol revisions this server knows; the client's choice is echoed
/// when we support it, otherwise we answer with our newest.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct McpServer {
    index: IndexDb,
    config: Config,
    /// Lazily constructed on the first search so `initialize` stays fast.
    embedder: Option<Box<dyn Embedder>>,
}

impl McpServer {
    pub fn new(index: IndexDb, config: Config) -> Self {
        McpServer {
            index,
            config,
            embedder: None,
        }
    }

    /// Handle one incoming JSON-RPC message. `None` means nothing is sent
    /// back (notifications, and requests that malformed their own id).
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Notifications never get a response, whatever the method.
        let id = id?;

        let outcome = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => return Some(self.tools_call(id, &params)),
            "" => Err((-32600, "missing method".to_string())),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match outcome {
            Ok(result) => rpc_result(id, result),
            Err((code, message)) => rpc_error(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = if SUPPORTED_PROTOCOLS.contains(&requested) {
            requested
        } else {
            SUPPORTED_PROTOCOLS[0]
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "ai-imessage",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Read-only search over the local Apple Messages \
                index. Use search_messages to find conversations, \
                get_conversation to expand a chunk with surrounding context, \
                and list_chats to browse chats.",
        })
    }

    fn tools_call(&mut self, id: Value, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = match name {
            "search_messages" => self.tool_search(&args),
            "get_conversation" => self.tool_conversation(&args),
            "list_chats" => self.tool_list_chats(&args),
            other => return rpc_error(id, -32602, &format!("unknown tool: {other}")),
        };
        // Tool execution failures are results with isError, not protocol
        // errors — the model is meant to read them.
        match outcome {
            Ok(text) => rpc_result(
                id,
                json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
            ),
            Err(e) => rpc_result(
                id,
                json!({ "content": [{ "type": "text", "text": format!("error: {e:#}") }], "isError": true }),
            ),
        }
    }

    fn tool_search(&mut self, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("'query' (non-empty string) is required"))?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 50) as u32)
            .unwrap_or(self.config.retrieval.result_limit);
        let mode = args.get("mode").and_then(Value::as_str).unwrap_or("hybrid");

        let hits = match mode {
            "keyword" => self.index.search(query, limit)?,
            "semantic" => {
                let vec = self
                    .embedder()?
                    .ok_or_else(|| {
                        anyhow::anyhow!("no embeddings in the index; run `ai-imessage etl`")
                    })?
                    .embed_query(query)?;
                self.index.vector_search(&vec, limit)?
            }
            "hybrid" => {
                let query_vec = match self.embedder()? {
                    Some(e) => Some(e.embed_query(query)?),
                    None => None,
                };
                let params = RetrievalParams {
                    fts_candidates: self.config.retrieval.fts_candidates,
                    vector_candidates: self.config.retrieval.vector_candidates,
                    limit,
                };
                hybrid_search(&self.index, query, query_vec.as_deref(), &params)?
            }
            other => anyhow::bail!("unknown mode \"{other}\" (hybrid, keyword, or semantic)"),
        };

        if hits.is_empty() {
            return Ok(format!("No matches for \"{query}\"."));
        }
        let mut out = format!("{} result(s) for \"{query}\"\n", hits.len());
        for h in &hits {
            let text = self
                .index
                .chunk_text(h.chunk_id)?
                .unwrap_or_else(|| h.snippet.clone());
            out.push_str(&format!(
                "\n[chunk {}] {} — {} ({} messages)\n{}\n",
                h.chunk_id,
                h.chat_label,
                format_range(h.started_at_ms, h.ended_at_ms),
                h.message_count,
                text,
            ));
        }
        Ok(out)
    }

    fn tool_conversation(&mut self, args: &Value) -> Result<String> {
        let chunk_id = args
            .get("chunk_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("'chunk_id' (integer) is required"))?;
        let before = args
            .get("before")
            .and_then(Value::as_u64)
            .map(|n| n.min(100) as u32)
            .unwrap_or(self.config.retrieval.context_before);
        let after = args
            .get("after")
            .and_then(Value::as_u64)
            .map(|n| n.min(100) as u32)
            .unwrap_or(self.config.retrieval.context_after);

        let window = self
            .index
            .conversation_window(chunk_id, before, after)?
            .ok_or_else(|| anyhow::anyhow!("no chunk with id {chunk_id}"))?;

        let mut out = format!(
            "{} — conversation around chunk {chunk_id}\n",
            window.chat_label
        );
        for m in &window.messages {
            let marker = if m.in_span { "" } else { "  (context)" };
            out.push_str(&format!(
                "[{}] {}: {}{marker}\n",
                format_ms(m.sent_at_ms),
                m.sender,
                m.text
            ));
        }
        Ok(out)
    }

    fn tool_list_chats(&mut self, args: &Value) -> Result<String> {
        let filter = args
            .get("filter")
            .and_then(Value::as_str)
            .filter(|f| !f.trim().is_empty());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 200) as u32)
            .unwrap_or(25);

        let chats = self.index.list_chats(filter, limit)?;
        if chats.is_empty() {
            return Ok("No chats match.".into());
        }
        let mut out = format!("{} chat(s), most recent first\n", chats.len());
        for c in &chats {
            let kind = match c.is_group {
                Some(true) => "group",
                Some(false) => "direct",
                None => "unknown",
            };
            out.push_str(&format!(
                "- {} ({kind}, {} messages, last {})\n",
                c.label,
                c.message_count,
                format_ms(c.last_message_ms)
            ));
        }
        Ok(out)
    }

    fn embedder(&mut self) -> Result<Option<&mut Box<dyn Embedder>>> {
        if self.embedder.is_none() {
            if self.index.embedding_count()? == 0 {
                return Ok(None);
            }
            let cache = self.config.index_dir()?.join("models");
            self.embedder = Some(embed::make_embedder(&self.config, &cache)?);
        }
        Ok(self.embedder.as_mut())
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search_messages",
            "description": "Search the local Apple Messages history. Returns \
                matching conversation chunks with their chunk ids, chat, date \
                range, and full text. Default mode fuses keyword and semantic \
                ranking.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for" },
                    "limit": { "type": "integer", "description": "Max results (default from config, max 50)" },
                    "mode": { "type": "string", "enum": ["hybrid", "keyword", "semantic"],
                              "description": "Retrieval mode (default hybrid)" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_conversation",
            "description": "Expand a search hit: the messages of one chunk \
                plus surrounding messages from the same chat, in order.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": { "type": "integer", "description": "Chunk id from search_messages" },
                    "before": { "type": "integer", "description": "Context messages before (default from config, max 100)" },
                    "after": { "type": "integer", "description": "Context messages after (default from config, max 100)" }
                },
                "required": ["chunk_id"]
            }
        },
        {
            "name": "list_chats",
            "description": "List chats in the index, most recently active \
                first, with message counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "description": "Case-insensitive substring of the chat name/handle" },
                    "limit": { "type": "integer", "description": "Max chats (default 25, max 200)" }
                }
            }
        }
    ])
}

fn format_ms(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".into())
}

fn format_range(start: Option<i64>, end: Option<i64>) -> String {
    let day = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into())
    };
    match (start, end) {
        (Some(s), Some(e)) if day(s) == day(e) => day(s),
        (Some(s), Some(e)) => format!("{} → {}", day(s), day(e)),
        (Some(s), None) | (None, Some(s)) => day(s),
        (None, None) => "unknown date".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn server() -> (TempDir, McpServer) {
        let dir = TempDir::new().unwrap();
        let index = IndexDb::open(&dir.path().join("index.sqlite")).unwrap();
        (dir, McpServer::new(index, Config::default()))
    }

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn initialize_echoes_a_supported_protocol_version() {
        let (_d, mut s) = server();
        let resp = s
            .handle(&req(
                1,
                "initialize",
                json!({"protocolVersion": "2025-03-26"}),
            ))
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "ai-imessage");
    }

    #[test]
    fn initialize_falls_back_to_newest_supported_version() {
        let (_d, mut s) = server();
        let resp = s
            .handle(&req(
                1,
                "initialize",
                json!({"protocolVersion": "1999-01-01"}),
            ))
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn notifications_get_no_response() {
        let (_d, mut s) = server();
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(s.handle(&note).is_none());
    }

    #[test]
    fn ping_answers_with_empty_result() {
        let (_d, mut s) = server();
        let resp = s.handle(&req(7, "ping", Value::Null)).unwrap();
        assert_eq!(resp["result"], json!({}));
        assert_eq!(resp["id"], 7);
    }

    #[test]
    fn tools_list_names_all_three_tools() {
        let (_d, mut s) = server();
        let resp = s.handle(&req(2, "tools/list", Value::Null)).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["search_messages", "get_conversation", "list_chats"]
        );
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let (_d, mut s) = server();
        let resp = s.handle(&req(3, "resources/list", Value::Null)).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn unknown_tool_is_an_invalid_params_error() {
        let (_d, mut s) = server();
        let resp = s
            .handle(&req(4, "tools/call", json!({"name": "drop_tables"})))
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn search_without_query_is_a_tool_error_result() {
        let (_d, mut s) = server();
        let resp = s
            .handle(&req(
                5,
                "tools/call",
                json!({"name": "search_messages", "arguments": {}}),
            ))
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("query")
        );
    }

    #[test]
    fn conversation_for_missing_chunk_is_a_tool_error_result() {
        let (_d, mut s) = server();
        let resp = s
            .handle(&req(
                6,
                "tools/call",
                json!({"name": "get_conversation", "arguments": {"chunk_id": 999}}),
            ))
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
