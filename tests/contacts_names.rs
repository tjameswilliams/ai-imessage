//! Integration tests for contact-name enrichment: names from a synthetic
//! AddressBook flow into handles, chunk text, and every label surface.

mod common;

use ai_imessage::chunk::ChunkParams;
use ai_imessage::contacts::ContactBook;
use ai_imessage::etl::sync;
use ai_imessage::extract::SourceDb;
use ai_imessage::index::IndexDb;
use ai_imessage::model::CHAT_STYLE_DIRECT;
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns, build_contacts_dir};
use rusqlite::Connection;

const CHUNKING: ChunkParams = ChunkParams {
    gap_minutes: 45,
    target_tokens: 750,
    overlap_messages: 3,
};

fn populate(f: &Fixture) {
    let alice = f.add_handle("+19165550100");
    let bob = f.add_handle("bob@example.com");
    let chat_a = f.add_chat("chat-alice", CHAT_STYLE_DIRECT, Some(""));
    let chat_b = f.add_chat("chat-bob", CHAT_STYLE_DIRECT, Some(""));

    let m1 = f.add_message(&MessageSpec {
        guid: "c1",
        text: Some("lunch on thursday?"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat_a, m1);

    let m2 = f.add_message(&MessageSpec {
        guid: "c2",
        text: Some("sending over the contract now"),
        handle_id: Some(bob),
        date: apple_ns("2026-07-01T10:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat_b, m2);
}

fn run_sync(f: &Fixture, index_path: &std::path::Path, book: Option<&ContactBook>) {
    let source = SourceDb::open(&f.db_path).unwrap();
    let mut index = IndexDb::open(index_path).unwrap();
    sync(&source, &mut index, 100, &CHUNKING, book).unwrap();
}

#[test]
fn names_land_in_handles_chunks_and_labels() {
    let f = Fixture::new(SchemaVariant::Modern);
    populate(&f);
    let contacts = build_contacts_dir(
        f.dir.path(),
        &[
            ("Alice", "Smith", "", &["(916) 555-0100"], &[]),
            ("Bob", "Jones", "", &[], &["Bob@Example.com"]),
        ],
    );
    let book = ContactBook::load(&contacts).unwrap();
    let index_path = f.dir.path().join("index.sqlite");
    run_sync(&f, &index_path, Some(&book));

    // Chunk text bakes the name in — retrieval by name works.
    let conn = Connection::open(&index_path).unwrap();
    let chunk: String = conn
        .query_row(
            "SELECT k.text FROM chunks k JOIN chats c ON c.id = k.chat_id
             WHERE c.guid = 'chat-alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(chunk, "Alice Smith: lunch on thursday?");
    drop(conn);

    // Keyword search finds people by name, and the hit label is the name.
    let index = IndexDb::open(&index_path).unwrap();
    let hits = index.search("Alice thursday", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chat_label, "Alice Smith");

    // Chat listing and conversation windows use names too.
    let chats = index.list_chats(None, 10).unwrap();
    let labels: Vec<&str> = chats.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"Alice Smith"));
    assert!(labels.contains(&"Bob Jones"));

    let window = index
        .conversation_window(hits[0].chunk_id, 5, 5)
        .unwrap()
        .unwrap();
    assert_eq!(window.messages[0].sender, "Alice Smith");
}

#[test]
fn syncing_without_contacts_keeps_raw_handles() {
    let f = Fixture::new(SchemaVariant::Modern);
    populate(&f);
    let index_path = f.dir.path().join("index.sqlite");
    run_sync(&f, &index_path, None);

    let index = IndexDb::open(&index_path).unwrap();
    let hits = index.search("thursday", 10).unwrap();
    assert_eq!(hits[0].chat_label, "+19165550100");
}

#[test]
fn a_rename_rechunks_only_that_contacts_chats() {
    let f = Fixture::new(SchemaVariant::Modern);
    populate(&f);
    let index_path = f.dir.path().join("index.sqlite");

    let contacts_v1 = build_contacts_dir(
        &f.dir.path().join("v1"),
        &[
            ("Alice", "Smith", "", &["(916) 555-0100"], &[]),
            ("Bob", "Jones", "", &[], &["bob@example.com"]),
        ],
    );
    run_sync(
        &f,
        &index_path,
        Some(&ContactBook::load(&contacts_v1).unwrap()),
    );

    // Alice marries and changes her name; Bob is untouched.
    let contacts_v2 = build_contacts_dir(
        &f.dir.path().join("v2"),
        &[
            ("Alice", "Nguyen", "", &["(916) 555-0100"], &[]),
            ("Bob", "Jones", "", &[], &["bob@example.com"]),
        ],
    );
    let source = SourceDb::open(&f.db_path).unwrap();
    let mut index = IndexDb::open(&index_path).unwrap();
    let book = ContactBook::load(&contacts_v2).unwrap();
    let report = sync(&source, &mut index, 100, &CHUNKING, Some(&book)).unwrap();

    // Only Alice's chat re-chunked; nothing was inserted or updated.
    assert_eq!(report.inserted, 0);
    assert_eq!(report.updated, 0);
    assert_eq!(report.rechunked_chats, 1);
    assert_eq!(report.named_handles, 2);

    let hits = index.search("thursday", 10).unwrap();
    assert_eq!(hits[0].chat_label, "Alice Nguyen");
}

#[test]
fn names_arriving_later_backfill_an_existing_index() {
    // First sync with no contacts (e.g. before the feature existed or a
    // denied AddressBook read), second sync with them: chunks upgrade.
    let f = Fixture::new(SchemaVariant::Modern);
    populate(&f);
    let index_path = f.dir.path().join("index.sqlite");
    run_sync(&f, &index_path, None);

    let contacts = build_contacts_dir(
        f.dir.path(),
        &[("Alice", "Smith", "", &["+1 916 555 0100"], &[])],
    );
    run_sync(
        &f,
        &index_path,
        Some(&ContactBook::load(&contacts).unwrap()),
    );

    let index = IndexDb::open(&index_path).unwrap();
    let hits = index.search("Alice", 10).unwrap();
    assert_eq!(hits.len(), 1, "name is searchable after backfill");
    assert_eq!(hits[0].chat_label, "Alice Smith");
}
