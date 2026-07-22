//! Integration tests for the MCP server's tools against a populated index,
//! using the offline debug-hash embedding provider.

mod common;

use ai_imessage::chunk::ChunkParams;
use ai_imessage::config::Config;
use ai_imessage::embed::make_embedder;
use ai_imessage::etl::{embed_missing, sync};
use ai_imessage::extract::SourceDb;
use ai_imessage::index::IndexDb;
use ai_imessage::mcp::McpServer;
use ai_imessage::model::{CHAT_STYLE_DIRECT, CHAT_STYLE_GROUP};
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};
use serde_json::{Value, json};

const CHUNKING: ChunkParams = ChunkParams {
    gap_minutes: 45,
    target_tokens: 750,
    overlap_messages: 3,
};

fn debug_config() -> Config {
    let mut c = Config::default();
    c.embeddings.provider = "debug-hash".into();
    c
}

/// Fixture with two chats and three separate conversations, synced,
/// chunked, and embedded into an index served by McpServer.
fn server() -> (Fixture, McpServer) {
    let f = Fixture::new(SchemaVariant::Modern);
    let alice = f.add_handle("+15550100001");
    let direct = f.add_chat("direct-chat", CHAT_STYLE_DIRECT, Some(""));
    let group = f.add_chat("group-chat", CHAT_STYLE_GROUP, Some("Ski Trip"));

    for (i, (text, minute)) in [
        ("morning! how did the interview go", 0i64),
        ("really well, they want a second round", 2),
        ("that's fantastic news", 3),
    ]
    .iter()
    .enumerate()
    {
        let m = f.add_message(&MessageSpec {
            guid: &format!("d{i}"),
            text: Some(text),
            handle_id: (i % 2 == 0).then_some(alice),
            is_from_me: i % 2 == 1,
            date: apple_ns("2026-07-01T09:00:00Z") + minute * 60 * 1_000_000_000,
            ..Default::default()
        });
        f.link_chat_message(direct, m);
    }
    // A later conversation in the same chat (past the 45-minute gap).
    let late = f.add_message(&MessageSpec {
        guid: "d-late",
        text: Some("lunch tomorrow?"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T15:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, late);

    let g = f.add_message(&MessageSpec {
        guid: "g0",
        text: Some("cabin is booked for the ski weekend"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-02T20:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(group, g);

    let index_path = f.dir.path().join("index.sqlite");
    let source = SourceDb::open(&f.db_path).unwrap();
    let mut index = IndexDb::open(&index_path).unwrap();
    sync(&source, &mut index, 100, &CHUNKING, None).unwrap();
    let mut embedder = make_embedder(&debug_config(), f.dir.path()).unwrap();
    embed_missing(&mut index, embedder.as_mut(), |_, _| {}).unwrap();

    let server = McpServer::new(IndexDb::open(&index_path).unwrap(), debug_config());
    (f, server)
}

fn call(server: &mut McpServer, tool: &str, args: Value) -> Value {
    let resp = server
        .handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        }))
        .unwrap();
    resp["result"].clone()
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn search_messages_returns_full_chunk_text_with_chunk_ids() {
    let (_f, mut s) = server();
    let result = call(&mut s, "search_messages", json!({"query": "interview"}));
    assert_eq!(result["isError"], false);
    let text = text_of(&result);
    assert!(text.contains("[chunk "));
    assert!(text.contains("how did the interview go"));
    // Full chunk text, not just the matching line.
    assert!(text.contains("second round"));
}

#[test]
fn search_messages_modes_and_bad_mode() {
    let (_f, mut s) = server();
    for mode in ["hybrid", "keyword", "semantic"] {
        let result = call(
            &mut s,
            "search_messages",
            json!({"query": "ski cabin booked", "mode": mode}),
        );
        assert_eq!(result["isError"], false, "mode {mode}");
        assert!(text_of(&result).contains("Ski Trip"), "mode {mode}");
    }
    let result = call(
        &mut s,
        "search_messages",
        json!({"query": "x", "mode": "psychic"}),
    );
    assert_eq!(result["isError"], true);
}

#[test]
fn get_conversation_expands_a_hit_with_context() {
    let (_f, mut s) = server();
    let hit_text = text_of(&call(
        &mut s,
        "search_messages",
        json!({"query": "interview"}),
    ));
    let chunk_id: i64 = hit_text
        .split("[chunk ")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let result = call(
        &mut s,
        "get_conversation",
        json!({"chunk_id": chunk_id, "after": 5}),
    );
    assert_eq!(result["isError"], false);
    let text = text_of(&result);
    // The chunk's own messages, in order, plus the later conversation as
    // trailing context.
    assert!(text.contains("interview"));
    assert!(text.contains("fantastic"));
    assert!(text.contains("lunch tomorrow?  (context)"));
}

#[test]
fn list_chats_orders_by_recency_and_filters() {
    let (_f, mut s) = server();
    let text = text_of(&call(&mut s, "list_chats", json!({})));
    let ski = text.find("Ski Trip").unwrap();
    let direct = text.find("+15550100001").unwrap();
    assert!(ski < direct, "most recent chat first:\n{text}");
    assert!(text.contains("(group, 1 messages"));

    let text = text_of(&call(&mut s, "list_chats", json!({"filter": "ski"})));
    assert!(text.contains("Ski Trip"));
    assert!(!text.contains("+15550100001"));
}

/// A contact with an OLD direct conversation and NEWER activity elsewhere:
/// the exact shape that made relevance-ranked search misreport "when did I
/// last talk to her".
fn recency_server() -> (Fixture, McpServer) {
    let f = Fixture::new(SchemaVariant::Modern);
    let jaina = f.add_handle("+19165550142");
    let other = f.add_handle("+14155550107");
    let direct = f.add_chat("direct-jaina", CHAT_STYLE_DIRECT, Some(""));
    let group = f.add_chat("group-trip", CHAT_STYLE_GROUP, Some("Trip Crew"));
    let noise = f.add_chat("direct-other", CHAT_STYLE_DIRECT, Some(""));

    // An old, keyword-rich direct conversation (what search would find).
    let m1 = f.add_message(&MessageSpec {
        guid: "r1",
        text: Some("happy birthday!! hope the party is great"),
        handle_id: Some(jaina),
        date: apple_ns("2025-07-10T18:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m1);

    // Her latest message lives in a group chat...
    let m2 = f.add_message(&MessageSpec {
        guid: "r2",
        text: Some("flights are booked!"),
        handle_id: Some(jaina),
        date: apple_ns("2026-05-24T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(group, m2);

    // ...and the very last exchange is a from-me message in that group.
    let m3 = f.add_message(&MessageSpec {
        guid: "r3",
        text: Some("amazing, can't wait"),
        is_from_me: true,
        date: apple_ns("2026-05-25T10:30:00Z"),
        ..Default::default()
    });
    f.link_chat_message(group, m3);

    // Unrelated newer traffic that must NOT leak into Jaina's scope.
    let m4 = f.add_message(&MessageSpec {
        guid: "r4",
        text: Some("totally unrelated chatter"),
        handle_id: Some(other),
        date: apple_ns("2026-07-01T12:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(noise, m4);

    let contacts = common::build_contacts_dir(
        f.dir.path(),
        &[
            ("Jaina", "Proudmoore", "", &["(916) 555-0142"], &[]),
            ("Somebody", "Else", "", &["(415) 555-0107"], &[]),
        ],
    );
    let book = ai_imessage::contacts::ContactBook::load(&contacts).unwrap();

    let index_path = f.dir.path().join("index.sqlite");
    let source = SourceDb::open(&f.db_path).unwrap();
    let mut index = IndexDb::open(&index_path).unwrap();
    sync(&source, &mut index, 100, &CHUNKING, Some(&book)).unwrap();
    let mut embedder = make_embedder(&debug_config(), f.dir.path()).unwrap();
    ai_imessage::etl::embed_missing(&mut index, embedder.as_mut(), |_, _| {}).unwrap();

    let server = McpServer::new(IndexDb::open(&index_path).unwrap(), debug_config());
    (f, server)
}

#[test]
fn recent_messages_reports_the_true_last_contact_across_chats() {
    let (_f, mut s) = recency_server();
    let result = call(&mut s, "get_recent_messages", json!({"contact": "Jaina"}));
    assert_eq!(result["isError"], false);
    let text = text_of(&result);

    // The headline answer is her real last activity — the from-me group
    // message — not the old direct conversation.
    assert!(text.contains("Contact: Jaina Proudmoore"), "{text}");
    assert!(
        text.contains("Most recent message: [2026-05-25 10:30]"),
        "{text}"
    );
    // Both her chats appear; unrelated chats do not.
    assert!(text.contains("flights are booked!"));
    assert!(text.contains("Me: amazing, can't wait"));
    assert!(text.contains("happy birthday"));
    assert!(!text.contains("unrelated chatter"));
    // Oldest-to-newest ordering for readability.
    let birthday = text.find("happy birthday").unwrap();
    let latest = text.find("amazing, can't wait").unwrap();
    assert!(birthday < latest);
}

#[test]
fn recent_messages_scopes_by_chat_and_handles_unknowns() {
    let (_f, mut s) = recency_server();

    let text = text_of(&call(
        &mut s,
        "get_recent_messages",
        json!({"chat": "Trip Crew", "limit": 10}),
    ));
    assert!(text.contains("flights are booked!"));
    assert!(!text.contains("happy birthday"));

    let text = text_of(&call(
        &mut s,
        "get_recent_messages",
        json!({"contact": "Thrall"}),
    ));
    assert!(text.contains("No contact or handle matching"));

    // No scope at all: newest message in the whole index leads.
    let text = text_of(&call(&mut s, "get_recent_messages", json!({"limit": 2})));
    assert!(text.contains("unrelated chatter"));
}

#[test]
fn full_handshake_then_tool_call_round_trip() {
    let (_f, mut s) = server();
    let init = s
        .handle(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                        "params": {"protocolVersion": "2025-06-18"}}))
        .unwrap();
    assert_eq!(init["result"]["capabilities"]["tools"], json!({}));
    assert!(
        s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .is_none()
    );
    let result = call(&mut s, "search_messages", json!({"query": "cabin"}));
    assert!(text_of(&result).contains("cabin is booked"));
}
