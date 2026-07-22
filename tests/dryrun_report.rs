//! Integration tests for the `etl --dry-run` report.

mod common;

use ai_imessage::dryrun::{build_report, text_samples};
use ai_imessage::extract::SourceDb;
use ai_imessage::model::{CHAT_STYLE_DIRECT, CHAT_STYLE_GROUP};
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};

fn fixture() -> Fixture {
    let f = Fixture::new(SchemaVariant::Modern);
    let alice = f.add_handle("+15550100001");
    let direct = f.add_chat("direct-chat", CHAT_STYLE_DIRECT, None);
    let group = f.add_chat("group-chat", CHAT_STYLE_GROUP, Some("Group"));
    // A chat with an unknown style must land in "other".
    f.add_chat("weird-chat", 99, None);

    let m1 = f.add_message(&MessageSpec {
        guid: "r1",
        text: Some("plain text body"),
        handle_id: Some(alice),
        date: apple_ns("2020-01-05T00:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m1);

    let m2 = f.add_message(&MessageSpec {
        guid: "r2",
        attributed_text: Some("typedstream body"),
        is_from_me: true,
        date: apple_ns("2026-07-10T12:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m2);

    let m3 = f.add_message(&MessageSpec {
        guid: "r3",
        associated_message_type: 2001,
        handle_id: Some(alice),
        date: apple_ns("2023-06-15T08:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(direct, m3);

    let m4 = f.add_message(&MessageSpec {
        guid: "r4",
        item_type: 1,
        date: apple_ns("2024-02-20T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(group, m4);

    f
}

#[test]
fn report_counts_are_correct() {
    let f = fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let r = build_report(&db).unwrap();

    assert_eq!(r.total_messages, 4);
    assert_eq!(r.with_text_column, 1);
    assert_eq!(r.recovered_from_typedstream, 1);
    assert_eq!(r.without_text, 2);
    assert_eq!(r.tapbacks_without_text, 1);
    assert_eq!(r.system_events, 1);
    assert_eq!(r.direct_chats, 1);
    assert_eq!(r.group_chats, 1);
    assert_eq!(r.other_chats, 1);
}

#[test]
fn report_time_range_spans_earliest_to_latest() {
    let f = fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let r = build_report(&db).unwrap();
    assert_eq!(
        r.earliest.unwrap().to_rfc3339(),
        "2020-01-05T00:00:00+00:00"
    );
    assert_eq!(r.latest.unwrap().to_rfc3339(), "2026-07-10T12:00:00+00:00");
}

#[test]
fn report_display_never_contains_message_bodies() {
    let f = fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let r = build_report(&db).unwrap();
    let rendered = r.to_string();
    assert!(!rendered.contains("plain text body"));
    assert!(!rendered.contains("typedstream body"));
}

#[test]
fn empty_database_produces_zeroed_report() {
    let f = Fixture::new(SchemaVariant::Modern);
    let db = SourceDb::open(&f.db_path).unwrap();
    let r = build_report(&db).unwrap();
    assert_eq!(r.total_messages, 0);
    assert_eq!(r.earliest, None);
    assert_eq!(r.latest, None);
}

#[test]
fn text_samples_respect_the_limit() {
    let f = fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let samples = text_samples(&db, 1).unwrap();
    assert_eq!(samples.len(), 1);
    assert!(samples[0].contains("plain text body"));
}

#[test]
fn text_samples_skip_textless_messages() {
    let f = fixture();
    let db = SourceDb::open(&f.db_path).unwrap();
    let samples = text_samples(&db, 10).unwrap();
    // Only two messages have any text.
    assert_eq!(samples.len(), 2);
}

#[test]
fn text_samples_truncate_long_bodies() {
    let f = Fixture::new(SchemaVariant::Modern);
    let long_text = "x".repeat(500);
    f.add_message(&MessageSpec {
        guid: "long",
        text: Some(&long_text),
        date: apple_ns("2026-01-01T00:00:00Z"),
        ..Default::default()
    });
    let db = SourceDb::open(&f.db_path).unwrap();
    let samples = text_samples(&db, 1).unwrap();
    assert!(samples[0].chars().count() < 200);
    assert!(samples[0].ends_with('…'));
}
