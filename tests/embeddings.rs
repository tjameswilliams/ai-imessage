//! Integration tests for the embedding stage and vector search, using the
//! offline debug-hash provider — no model download, no network.

mod common;

use ai_imessage::chunk::ChunkParams;
use ai_imessage::config::Config;
use ai_imessage::embed::make_embedder;
use ai_imessage::etl::{embed_missing, sync};
use ai_imessage::extract::SourceDb;
use ai_imessage::index::IndexDb;
use ai_imessage::model::CHAT_STYLE_DIRECT;
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};

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

    fn sync(&self) {
        let source = SourceDb::open(&self.fixture.db_path).unwrap();
        let mut index = IndexDb::open(&self.index_path).unwrap();
        sync(&source, &mut index, 100, &CHUNKING).unwrap();
    }

    fn embed(&self) -> ai_imessage::etl::EmbedReport {
        let mut index = IndexDb::open(&self.index_path).unwrap();
        let mut embedder = make_embedder(&debug_config(), self.fixture.dir.path()).unwrap();
        embed_missing(&mut index, embedder.as_mut(), |_, _| {}).unwrap()
    }
}

fn populate(w: &World) {
    let f = &w.fixture;
    let alice = f.add_handle("+15550100001");
    let chat = f.add_chat("direct-chat", CHAT_STYLE_DIRECT, Some(""));
    let m1 = f.add_message(&MessageSpec {
        guid: "e1",
        text: Some("we should order pizza for dinner tonight"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T18:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m1);
    // A different conversation after a lull → second chunk.
    let m2 = f.add_message(&MessageSpec {
        guid: "e2",
        text: Some("the quarterly budget review moved to Friday"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-02T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m2);
}

#[test]
fn embedding_covers_all_chunks_and_is_idempotent() {
    let w = World::new();
    populate(&w);
    w.sync();

    let r = w.embed();
    assert_eq!(r.model, "debug-hash");
    assert_eq!(r.embedded, 2);
    assert_eq!(r.total, 2);

    let again = w.embed();
    assert_eq!(again.embedded, 0);
    assert_eq!(again.total, 2);
}

#[test]
fn vector_search_ranks_the_semantically_closer_chunk_first() {
    let w = World::new();
    populate(&w);
    w.sync();
    w.embed();

    let index = IndexDb::open(&w.index_path).unwrap();
    let mut embedder = make_embedder(&debug_config(), w.fixture.dir.path()).unwrap();

    let q = embedder.embed_query("pizza dinner").unwrap();
    let hits = index.vector_search(&q, 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits[0].snippet.contains("pizza"));
    assert!(hits[0].score.unwrap() > hits[1].score.unwrap());

    let q = embedder.embed_query("budget review").unwrap();
    let hits = index.vector_search(&q, 10).unwrap();
    assert!(hits[0].snippet.contains("budget"));
}

#[test]
fn appended_messages_only_embed_new_chunks() {
    let w = World::new();
    populate(&w);
    w.sync();
    w.embed();

    // A new conversation after a lull adds one chunk; the two existing
    // chunk embeddings are reused, not recomputed.
    let f = &w.fixture;
    let m3 = f.add_message(&MessageSpec {
        guid: "e3",
        text: Some("running late, be there in ten"),
        is_from_me: true,
        date: apple_ns("2026-07-03T12:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(1, m3);
    w.sync();

    let r = w.embed();
    assert_eq!(r.embedded, 1);
    assert_eq!(r.total, 3);
    assert_eq!(r.pruned, 0);
}

#[test]
fn edits_reembed_only_the_affected_chunk_and_prune_the_stale_vector() {
    let w = World::new();
    populate(&w);
    w.sync();
    w.embed();

    w.fixture.edit_message(
        "e1",
        "we should order sushi for dinner tonight",
        apple_ns("2026-07-01T19:00:00Z"),
    );
    w.sync();

    let r = w.embed();
    // The edited chunk got a new hash: its old vector is pruned and one
    // new embedding is computed. The untouched chunk keeps its vector.
    assert_eq!(r.pruned, 1);
    assert_eq!(r.embedded, 1);
    assert_eq!(r.total, 2);
}

#[test]
fn switching_models_wipes_and_rebuilds_embeddings() {
    let w = World::new();
    populate(&w);
    w.sync();
    w.embed();

    let mut index = IndexDb::open(&w.index_path).unwrap();
    assert!(index.ensure_embedding_model("other-model").unwrap());
    assert_eq!(index.embedding_count().unwrap(), 0);

    // Back on the original model, everything re-embeds.
    let r = w.embed();
    assert_eq!(r.embedded, 2);
}
