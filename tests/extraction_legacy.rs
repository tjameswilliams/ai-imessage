//! Integration tests: extraction must tolerate old schemas that lack the
//! modern columns, and second-resolution timestamps.

mod common;

use ai_imessage::extract::SourceDb;
use ai_imessage::model::{CHAT_STYLE_DIRECT, TextSource};
use common::{Fixture, MessageSpec, SchemaVariant, apple_secs};

fn legacy_fixture() -> Fixture {
    let f = Fixture::new(SchemaVariant::Legacy);
    let carol = f.add_handle("carol@example.com");
    let chat = f.add_chat("iMessage;-;carol@example.com", CHAT_STYLE_DIRECT, None);
    f.link_chat_handle(chat, carol);

    let m1 = f.add_message(&MessageSpec {
        guid: "legacy-1",
        text: Some("Sent long ago"),
        handle_id: Some(carol),
        date: apple_secs("2015-03-10T18:30:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m1);

    let m2 = f.add_message(&MessageSpec {
        guid: "legacy-2",
        is_from_me: true,
        date: apple_secs("2015-03-10T18:31:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m2);
    f
}

#[test]
fn legacy_schema_caps_are_all_absent() {
    let f = legacy_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let caps = db.caps();
    assert!(!caps.has_attributed_body);
    assert!(!caps.has_date_edited);
    assert!(!caps.has_date_retracted);
    assert!(!caps.has_thread_originator_guid);
    assert!(!caps.has_associated_message_type);
    assert!(caps.has_item_type);
}

#[test]
fn legacy_messages_extract_with_second_timestamps() {
    let f = legacy_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    assert_eq!(messages.len(), 2);

    let m1 = &messages[0];
    assert_eq!(m1.text.as_deref(), Some("Sent long ago"));
    assert_eq!(m1.text_source, TextSource::TextColumn);
    assert_eq!(
        m1.sent_at.unwrap().to_rfc3339(),
        "2015-03-10T18:30:00+00:00"
    );
}

#[test]
fn missing_modern_columns_degrade_to_none() {
    let f = legacy_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    for m in &messages {
        assert_eq!(m.edited_at, None);
        assert_eq!(m.retracted_at, None);
        assert_eq!(m.reply_to_guid, None);
        assert!(!m.is_tapback);
    }
}

#[test]
fn legacy_message_without_text_has_no_source() {
    let f = legacy_fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let messages = db.collect_messages(0).unwrap();
    let m2 = &messages[1];
    assert_eq!(m2.text, None);
    assert_eq!(m2.text_source, TextSource::None);
    assert!(m2.is_from_me);
}
