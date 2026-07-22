//! Integration tests: extraction against a synthetic modern-schema chat.db.

mod common;

use ai_imessage::extract::SourceDb;
use ai_imessage::model::{CHAT_STYLE_DIRECT, CHAT_STYLE_GROUP, TextSource};
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};

/// A fixture exercising direct/group chats, sent/received messages, plain
/// text, typedstream-only bodies, tapbacks, system events, edits, and
/// replies.
fn populated_fixture() -> Fixture {
    let f = Fixture::new(SchemaVariant::Modern);
    let alice = f.add_handle("+15550100001");
    let bob = f.add_handle("bob@example.com");

    let direct = f.add_chat("iMessage;-;+15550100001", CHAT_STYLE_DIRECT, None);
    let group = f.add_chat("chat831264;+;group", CHAT_STYLE_GROUP, Some("The Crew"));
    f.link_chat_handle(direct, alice);
    f.link_chat_handle(group, alice);
    f.link_chat_handle(group, bob);

    // m1: received, plain text column.
    let m1 = f.add_message(&MessageSpec {
        guid: "m1",
        text: Some("Hello Tim"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:14:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m1);

    // m2: sent by me, body only in attributedBody (modern macOS pattern).
    let m2 = f.add_message(&MessageSpec {
        guid: "m2",
        attributed_text: Some("Recovered from typedstream 🎉"),
        is_from_me: true,
        date: apple_ns("2026-07-01T09:16:30Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m2);

    // m3: a tapback with no text.
    let m3 = f.add_message(&MessageSpec {
        guid: "m3",
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:17:00Z"),
        associated_message_type: 2000,
        ..Default::default()
    });
    f.link_chat_message(direct, m3);

    // m4: a group-membership system event.
    let m4 = f.add_message(&MessageSpec {
        guid: "m4",
        date: apple_ns("2026-07-02T10:00:00Z"),
        item_type: 2,
        ..Default::default()
    });
    f.link_chat_message(group, m4);

    // m5: group reply from bob, later edited.
    let m5 = f.add_message(&MessageSpec {
        guid: "m5",
        text: Some("Replying in the group"),
        handle_id: Some(bob),
        date: apple_ns("2026-07-02T10:05:00Z"),
        date_edited: Some(apple_ns("2026-07-02T10:06:00Z")),
        thread_originator_guid: Some("m1"),
        service: Some("SMS"),
        ..Default::default()
    });
    f.link_chat_message(group, m5);

    // m6: both text column and blob — the column must win.
    let m6 = f.add_message(&MessageSpec {
        guid: "m6",
        text: Some("column text"),
        attributed_text: Some("blob text"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-03T08:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m6);

    // m7: empty text column, valid blob — blob must be used.
    let m7 = f.add_message(&MessageSpec {
        guid: "m7",
        text: Some(""),
        attributed_text: Some("only in the blob"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-03T08:01:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m7);

    // m8: orphan — no chat link, no timestamp. Must still extract.
    f.add_message(&MessageSpec {
        guid: "m8",
        text: Some("orphaned message"),
        ..Default::default()
    });

    f
}

#[test]
fn extracts_all_messages_in_rowid_order() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    assert_eq!(messages.len(), 8);
    let guids: Vec<&str> = messages.iter().map(|m| m.guid.as_str()).collect();
    assert_eq!(guids, ["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8"]);
    let rowids: Vec<i64> = messages.iter().map(|m| m.rowid).collect();
    let mut sorted = rowids.clone();
    sorted.sort_unstable();
    assert_eq!(rowids, sorted);
}

#[test]
fn plain_text_message_fields_are_correct() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    let m1 = &messages[0];

    assert_eq!(m1.text.as_deref(), Some("Hello Tim"));
    assert_eq!(m1.text_source, TextSource::TextColumn);
    assert_eq!(m1.sender_handle.as_deref(), Some("+15550100001"));
    assert!(!m1.is_from_me);
    assert_eq!(m1.chat_guid.as_deref(), Some("iMessage;-;+15550100001"));
    assert_eq!(m1.is_group_chat(), Some(false));
    assert_eq!(m1.service.as_deref(), Some("iMessage"));
    assert_eq!(
        m1.sent_at.unwrap().to_rfc3339(),
        "2026-07-01T09:14:00+00:00"
    );
    assert!(!m1.is_tapback);
    assert!(!m1.is_system_event);
    assert_eq!(m1.edited_at, None);
    assert_eq!(m1.retracted_at, None);
}

#[test]
fn typedstream_only_message_is_recovered() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    let m2 = &messages[1];

    assert_eq!(m2.text.as_deref(), Some("Recovered from typedstream 🎉"));
    assert_eq!(m2.text_source, TextSource::AttributedBody);
    assert!(m2.is_from_me);
    assert_eq!(m2.sender_handle, None);
}

#[test]
fn tapbacks_and_system_events_are_flagged() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();

    let m3 = &messages[2];
    assert!(m3.is_tapback);
    assert_eq!(m3.text, None);
    assert_eq!(m3.text_source, TextSource::None);

    let m4 = &messages[3];
    assert!(m4.is_system_event);
    assert!(!m4.is_tapback);
}

#[test]
fn group_reply_with_edit_carries_metadata() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    let m5 = &messages[4];

    assert_eq!(m5.chat_display_name.as_deref(), Some("The Crew"));
    assert_eq!(m5.is_group_chat(), Some(true));
    assert_eq!(m5.sender_handle.as_deref(), Some("bob@example.com"));
    assert_eq!(m5.reply_to_guid.as_deref(), Some("m1"));
    assert_eq!(m5.service.as_deref(), Some("SMS"));
    assert_eq!(
        m5.edited_at.unwrap().to_rfc3339(),
        "2026-07-02T10:06:00+00:00"
    );
}

#[test]
fn text_column_wins_over_blob_and_empty_column_falls_back() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();

    let m6 = &messages[5];
    assert_eq!(m6.text.as_deref(), Some("column text"));
    assert_eq!(m6.text_source, TextSource::TextColumn);

    let m7 = &messages[6];
    assert_eq!(m7.text.as_deref(), Some("only in the blob"));
    assert_eq!(m7.text_source, TextSource::AttributedBody);
}

#[test]
fn orphan_message_without_chat_or_date_still_extracts() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    let m8 = &messages[7];

    assert_eq!(m8.chat_guid, None);
    assert_eq!(m8.is_group_chat(), None);
    assert_eq!(m8.sent_at, None);
    assert_eq!(m8.text.as_deref(), Some("orphaned message"));
}

#[test]
fn rowid_watermark_skips_already_seen_messages() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let all = db.collect_messages(0).unwrap();
    let after = db.collect_messages(all[2].rowid).unwrap();
    assert_eq!(after.len(), 5);
    assert_eq!(after[0].guid, "m4");
}

#[test]
fn message_in_multiple_chats_is_extracted_once() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    // Link m1 into the group chat as well (simulates a chat merge).
    f.link_chat_message(2, 1);
    let messages = db.collect_messages(0).unwrap();
    assert_eq!(
        messages.len(),
        8,
        "duplicate join rows must not duplicate messages"
    );
    assert_eq!(messages.iter().filter(|m| m.guid == "m1").count(), 1);
}

#[test]
fn chat_stats_count_by_style() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let stats = db.chat_stats().unwrap();
    assert_eq!(stats.direct, 1);
    assert_eq!(stats.group, 1);
    assert_eq!(stats.total, 2);
}

#[test]
fn message_count_matches_scan() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    assert_eq!(db.message_count().unwrap(), 8);
}

#[test]
fn empty_database_extracts_nothing() {
    let f = Fixture::new(SchemaVariant::Modern);
    let db = SourceDb::open(&f.db_path).unwrap();
    assert_eq!(db.collect_messages(0).unwrap().len(), 0);
    assert_eq!(db.message_count().unwrap(), 0);
    assert_eq!(db.chat_stats().unwrap().total, 0);
}

#[test]
fn modern_schema_caps_are_detected() {
    let f = Fixture::new(SchemaVariant::Modern);
    let db = SourceDb::open(&f.db_path).unwrap();
    let caps = db.caps();
    assert!(caps.has_attributed_body);
    assert!(caps.has_date_edited);
    assert!(caps.has_date_retracted);
    assert!(caps.has_thread_originator_guid);
    assert!(caps.has_item_type);
    assert!(caps.has_associated_message_type);
}

#[test]
fn content_hash_reflects_edits_across_scans() {
    let f = populated_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let before = db.collect_messages(0).unwrap();
    let m1_before = before
        .iter()
        .find(|m| m.guid == "m1")
        .unwrap()
        .content_hash();

    // Simulate an edit landing in the source database.
    rusqlite::Connection::open(&f.db_path)
        .unwrap()
        .execute(
            "UPDATE message SET text = 'Hello Tim (edited)', date_edited = ?1 WHERE guid = 'm1'",
            [apple_ns("2026-07-01T09:30:00Z")],
        )
        .unwrap();

    let after = db.collect_messages(0).unwrap();
    let m1_after = after
        .iter()
        .find(|m| m.guid == "m1")
        .unwrap()
        .content_hash();
    assert_ne!(m1_before, m1_after, "edits must change the content hash");
}
