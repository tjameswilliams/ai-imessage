//! Integration tests for incremental sync into the destination index.

mod common;

use ai_imessage::etl::sync;
use ai_imessage::extract::SourceDb;
use ai_imessage::index::IndexDb;
use ai_imessage::model::{CHAT_STYLE_DIRECT, CHAT_STYLE_GROUP};
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};
use rusqlite::Connection;

const OVERLAP: u32 = 100;

struct World {
    fixture: Fixture,
    index_path: std::path::PathBuf,
}

impl World {
    fn new() -> Self {
        let fixture = Fixture::new(SchemaVariant::Modern);
        let index_path = fixture.dir.path().join("index.sqlite");
        World {
            fixture,
            index_path,
        }
    }

    fn sync(&self) -> ai_imessage::etl::SyncReport {
        self.sync_with_overlap(OVERLAP)
    }

    fn sync_with_overlap(&self, overlap: u32) -> ai_imessage::etl::SyncReport {
        let source = SourceDb::open(&self.fixture.db_path).unwrap();
        let mut index = IndexDb::open(&self.index_path).unwrap();
        sync(&source, &mut index, overlap).unwrap()
    }

    /// Read-only peek into the produced index for assertions.
    fn inspect<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&Connection) -> T,
    {
        let conn = Connection::open(&self.index_path).unwrap();
        f(&conn)
    }
}

fn populate(w: &World) {
    let f = &w.fixture;
    let alice = f.add_handle("+15550100001");
    let bob = f.add_handle("bob@example.com");
    let direct = f.add_chat("direct-chat", CHAT_STYLE_DIRECT, None);
    let group = f.add_chat("group-chat", CHAT_STYLE_GROUP, Some("The Group"));

    let m1 = f.add_message(&MessageSpec {
        guid: "m1",
        text: Some("hello from alice"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m1);

    let m2 = f.add_message(&MessageSpec {
        guid: "m2",
        attributed_text: Some("reply from me"),
        is_from_me: true,
        date: apple_ns("2026-07-01T09:05:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m2);

    let m3 = f.add_message(&MessageSpec {
        guid: "m3",
        attributed_text: Some("Loved “reply from me”"),
        associated_message_type: 2000,
        handle_id: Some(bob),
        date: apple_ns("2026-07-01T09:06:00Z"),
        ..Default::default()
    });
    f.link_chat_message(group, m3);
}

#[test]
fn first_run_ingests_everything() {
    let w = World::new();
    populate(&w);
    let r = w.sync();

    assert_eq!(r.scanned, 3);
    assert_eq!(r.inserted, 3);
    assert_eq!(r.updated, 0);
    assert_eq!(r.unchanged, 0);
    assert_eq!(r.watermark_before, 0);
    assert_eq!(r.watermark_after, 3);
    assert_eq!(r.total_messages, 3);
    assert_eq!(r.total_chats, 2);
    assert_eq!(r.total_handles, 2);
}

#[test]
fn normalized_rows_join_correctly() {
    let w = World::new();
    populate(&w);
    w.sync();

    w.inspect(|conn| {
        // Sender resolution through the handles table.
        let sender: String = conn
            .query_row(
                "SELECT h.handle FROM messages m
                 JOIN handles h ON h.id = m.sender_id
                 WHERE m.guid = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sender, "+15550100001");

        // From-me messages have no sender handle.
        let (from_me, sender_id): (bool, Option<i64>) = conn
            .query_row(
                "SELECT is_from_me, sender_id FROM messages WHERE guid = 'm2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(from_me);
        assert_eq!(sender_id, None);

        // Chat resolution, group flag, and display name.
        let (chat_guid, is_group, name): (String, bool, String) = conn
            .query_row(
                "SELECT c.guid, c.is_group, c.display_name FROM messages m
                 JOIN chats c ON c.id = m.chat_id
                 WHERE m.guid = 'm3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(chat_guid, "group-chat");
        assert!(is_group);
        assert_eq!(name, "The Group");

        // Tapback classification carries through with its raw kind.
        let (is_tapback, kind): (bool, i64) = conn
            .query_row(
                "SELECT is_tapback, associated_type FROM messages WHERE guid = 'm3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(is_tapback);
        assert_eq!(kind, 2000);

        // Timestamps land as unix millis.
        let sent: i64 = conn
            .query_row(
                "SELECT sent_at_ms FROM messages WHERE guid = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sent,
            chrono::DateTime::parse_from_rfc3339("2026-07-01T09:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
    });
}

#[test]
fn second_run_is_idempotent() {
    let w = World::new();
    populate(&w);
    w.sync();
    let r = w.sync();

    assert_eq!(r.inserted, 0);
    assert_eq!(r.updated, 0);
    // Overlap rescans the tail; everything in it is unchanged.
    assert_eq!(r.unchanged, 3);
    assert_eq!(r.total_messages, 3);
    assert_eq!(r.watermark_after, 3);
}

#[test]
fn new_messages_are_picked_up_incrementally() {
    let w = World::new();
    populate(&w);
    w.sync();

    let f = &w.fixture;
    let m4 = f.add_message(&MessageSpec {
        guid: "m4",
        text: Some("a new message"),
        date: apple_ns("2026-07-02T08:00:00Z"),
        is_from_me: true,
        ..Default::default()
    });
    f.link_chat_message(1, m4);

    let r = w.sync();
    assert_eq!(r.inserted, 1);
    assert_eq!(r.unchanged, 3);
    assert_eq!(r.total_messages, 4);
    assert_eq!(r.watermark_after, 4);
}

#[test]
fn edits_inside_the_overlap_window_are_applied() {
    let w = World::new();
    populate(&w);
    w.sync();

    w.fixture.edit_message(
        "m1",
        "hello from alice (edited)",
        apple_ns("2026-07-01T10:00:00Z"),
    );

    let r = w.sync();
    assert_eq!(r.inserted, 0);
    assert_eq!(r.updated, 1);
    assert_eq!(r.unchanged, 2);

    w.inspect(|conn| {
        let (text, edited): (String, Option<i64>) = conn
            .query_row(
                "SELECT text, edited_at_ms FROM messages WHERE guid = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(text, "hello from alice (edited)");
        assert!(edited.is_some());
    });
}

#[test]
fn retractions_clear_the_stored_body() {
    let w = World::new();
    populate(&w);
    w.sync();

    w.fixture
        .retract_message("m2", apple_ns("2026-07-01T11:00:00Z"));

    let r = w.sync();
    assert_eq!(r.updated, 1);

    w.inspect(|conn| {
        let (text, retracted): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT text, retracted_at_ms FROM messages WHERE guid = 'm2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(text, None);
        assert!(retracted.is_some());
    });
}

#[test]
fn edits_outside_the_overlap_window_are_missed_by_design() {
    let w = World::new();
    populate(&w);
    w.sync_with_overlap(0);

    w.fixture
        .edit_message("m1", "sneaky late edit", apple_ns("2026-07-01T10:00:00Z"));

    // Overlap 0 means only rows past the watermark are scanned; the edit
    // mutated an already-ingested row, so this run cannot see it.
    let r = w.sync_with_overlap(0);
    assert_eq!(r.scanned, 0);
    assert_eq!(r.updated, 0);

    // A later run WITH overlap catches it.
    let r = w.sync();
    assert_eq!(r.updated, 1);
}

#[test]
fn reset_then_sync_reingests_from_scratch() {
    let w = World::new();
    populate(&w);
    w.sync();

    let source = SourceDb::open(&w.fixture.db_path).unwrap();
    let mut index = IndexDb::open(&w.index_path).unwrap();
    index.reset().unwrap();
    let r = sync(&source, &mut index, OVERLAP).unwrap();

    assert_eq!(r.watermark_before, 0);
    assert_eq!(r.inserted, 3);
    assert_eq!(r.total_messages, 3);
}

#[test]
fn legacy_schema_syncs_without_modern_columns() {
    let fixture = Fixture::new(SchemaVariant::Legacy);
    let alice = fixture.add_handle("+15550100001");
    let chat = fixture.add_chat("legacy-chat", CHAT_STYLE_DIRECT, None);
    let m1 = fixture.add_message(&MessageSpec {
        guid: "l1",
        text: Some("old message"),
        handle_id: Some(alice),
        date: common::apple_secs("2015-03-01T12:00:00Z"),
        ..Default::default()
    });
    fixture.link_chat_message(chat, m1);

    let source = SourceDb::open(&fixture.db_path).unwrap();
    let index_path = fixture.dir.path().join("index.sqlite");
    let mut index = IndexDb::open(&index_path).unwrap();
    let r = sync(&source, &mut index, OVERLAP).unwrap();

    assert_eq!(r.inserted, 1);
    assert_eq!(r.total_messages, 1);
}

#[test]
fn report_display_contains_no_message_bodies() {
    let w = World::new();
    populate(&w);
    let rendered = w.sync().to_string();
    assert!(!rendered.contains("hello from alice"));
    assert!(!rendered.contains("reply from me"));
}
