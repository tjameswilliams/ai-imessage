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
                index. Use search_messages to find conversations by topic, \
                get_recent_messages for recency questions (when someone last \
                talked, what was said most recently, catching up on a chat), \
                get_conversation to expand a search hit with surrounding \
                context, and list_chats to browse chats. search_messages \
                ranks by relevance, NOT date — never use it to answer \
                'when' or 'most recent' questions.",
        })
    }

    fn tools_call(&mut self, id: Value, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = match name {
            "search_messages" => self.tool_search(&args),
            "get_recent_messages" => self.tool_recent_messages(&args),
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

    fn tool_recent_messages(&mut self, args: &Value) -> Result<String> {
        let contact = args
            .get("contact")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let chat = args
            .get("chat")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 200) as u32)
            .unwrap_or(25);

        let mut header = String::new();
        let chat_ids: Option<Vec<i64>> = match (contact, chat) {
            (Some(who), _) => {
                let matches = self.index.find_handles(who)?;
                if matches.is_empty() {
                    return Ok(format!(
                        "No contact or handle matching \"{who}\". Try list_chats \
                         to see who exists in the index."
                    ));
                }
                let mut names: Vec<String> = matches
                    .iter()
                    .map(|m| m.display_name.clone().unwrap_or_else(|| m.handle.clone()))
                    .collect();
                names.sort();
                names.dedup();
                if names.len() > 4 {
                    return Ok(format!(
                        "\"{who}\" is ambiguous — it matches {}: {}. Call again \
                         with a more specific contact.",
                        names.len(),
                        names.join(", ")
                    ));
                }
                let handles: Vec<String> = matches.iter().map(|m| m.handle.clone()).collect();
                header = format!("Contact: {} ({})\n", names.join(", "), handles.join(", "));
                let ids: Vec<i64> = matches.iter().map(|m| m.id).collect();
                Some(self.index.chats_for_handles(&ids)?)
            }
            (None, Some(label)) => {
                let ids = self.index.chats_matching_label(label)?;
                if ids.is_empty() {
                    return Ok(format!("No chat matching \"{label}\"."));
                }
                Some(ids)
            }
            (None, None) => None,
        };

        let mut messages = self.index.recent_messages(chat_ids.as_deref(), limit)?;
        let Some(latest) = messages.first() else {
            return Ok("No messages found in that scope.".into());
        };
        header.push_str(&format!(
            "Most recent message: [{}] in \"{}\"\n\nLast {} message(s), oldest to newest:\n",
            format_ms(Some(latest.sent_at_ms)),
            latest.chat_label,
            messages.len(),
        ));
        messages.reverse();
        let mut out = header;
        for m in &messages {
            out.push_str(&format!(
                "[{}] {} • {}: {}\n",
                format_ms(Some(m.sent_at_ms)),
                m.chat_label,
                m.sender,
                m.text
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

/// Serve MCP over streamable HTTP: a single `/mcp` endpoint accepting
/// POSTed JSON-RPC messages, answering with `application/json`. Stateless
/// (no session ids) and without server-initiated streams, both of which
/// the spec permits; GET therefore answers 405.
///
/// Every request must carry `Authorization: Bearer <token>` — this serves
/// a complete message history, so there is no unauthenticated mode.
pub fn serve_http(server: &mut McpServer, addr: &str, token: &str) -> Result<()> {
    let http =
        tiny_http::Server::http(addr).map_err(|e| anyhow::anyhow!("could not bind {addr}: {e}"))?;
    // The OS picks the port when the caller asked for :0; always report
    // the resolved address so clients (and tests) know where to connect.
    eprintln!("MCP listening on http://{}/mcp", http.server_addr());
    if !addr.starts_with("127.0.0.1")
        && !addr.starts_with("localhost")
        && !addr.starts_with("[::1]")
    {
        eprintln!(
            "warning: binding a non-loopback address exposes your entire \
             message history to anyone on that network who has the token. \
             Prefer 127.0.0.1 or a tailnet address."
        );
    }

    const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
    for mut request in http.incoming_requests() {
        let response = handle_http_request(server, &mut request, token, MAX_BODY_BYTES);
        let _ = request.respond(response);
    }
    Ok(())
}

fn handle_http_request(
    server: &mut McpServer,
    request: &mut tiny_http::Request,
    token: &str,
    max_body: u64,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    use tiny_http::{Header, Method, Response};

    let json_response = |status: u16, body: &Value| {
        Response::from_data(body.to_string().into_bytes())
            .with_status_code(status)
            .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
    };
    let empty = |status: u16| Response::from_data(Vec::new()).with_status_code(status);

    if request.url() != "/mcp" {
        return empty(404);
    }
    match request.method() {
        Method::Post => {}
        // No server-initiated streams and no sessions to delete.
        Method::Get | Method::Delete => return empty(405),
        _ => return empty(405),
    }

    let authorized = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), token.as_bytes()));
    if !authorized {
        return Response::from_data(Vec::new())
            .with_status_code(401)
            .with_header(Header::from_bytes("WWW-Authenticate", "Bearer").unwrap());
    }

    let mut body = String::new();
    use std::io::Read;
    if request
        .as_reader()
        .take(max_body)
        .read_to_string(&mut body)
        .is_err()
    {
        return empty(400);
    }
    let msg: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                400,
                &rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
            );
        }
    };

    match server.handle(&msg) {
        Some(resp) => json_response(200, &resp),
        // Notifications and responses get 202 Accepted with no body.
        None => empty(202),
    }
}

/// Compare secrets without early exit; a timing oracle on a token guarding
/// a full message history is cheap to close.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
            "description": "Search the local Apple Messages history BY TOPIC. \
                Returns matching conversation chunks with their chunk ids, \
                chat, date range, and full text, ranked by relevance — NOT by \
                date. For 'when did…' / 'most recent…' / 'last time…' \
                questions use get_recent_messages instead. Default mode fuses \
                keyword and semantic ranking.",
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
            "name": "get_recent_messages",
            "description": "The chronological tail of conversation, newest \
                messages across all relevant chats (direct AND groups), \
                including messages sent by the user. THE tool for: when \
                someone was last talked to, what was said most recently, \
                catching up on a person or chat. Scope by contact name, by \
                chat, or neither (all chats).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "contact": { "type": "string",
                                 "description": "Contact name or phone/email fragment; covers every chat they are in" },
                    "chat": { "type": "string",
                              "description": "Chat name filter (used when contact is not given)" },
                    "limit": { "type": "integer", "description": "Max messages (default 25, max 200)" }
                }
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
    fn tools_list_names_all_four_tools() {
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
            vec![
                "search_messages",
                "get_recent_messages",
                "get_conversation",
                "list_chats"
            ]
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
