//! Incremental sync from the Apple Messages database into the local index.
//!
//! Strategy: messages are appended to `chat.db` with monotonically
//! increasing ROWIDs, but recent rows also mutate in place (edits,
//! retractions, delivery updates). Each run therefore rescans from
//! `watermark - overlap` and upserts by GUID; the content hash decides
//! whether an already-known message actually changed.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::Serialize;

use crate::chunk::ChunkParams;
use crate::embed::Embedder;
use crate::extract::{SourceDb, SourceError};
use crate::index::{IndexDb, IndexError, Upsert, Writer};

#[derive(Debug, thiserror::Error)]
pub enum EtlError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("embedding failed: {0}")]
    Embedding(#[source] anyhow::Error),
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct SyncReport {
    pub index_path: String,
    pub scanned: u64,
    pub inserted: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub watermark_before: i64,
    pub watermark_after: i64,
    pub rechunked_chats: u64,
    pub total_messages: u64,
    pub total_chats: u64,
    pub total_handles: u64,
    pub total_chunks: u64,
}

/// Run one sync pass: upsert messages, then re-chunk every chat this run
/// touched. All writes happen in a single transaction: a failed run leaves
/// the index exactly as the previous run did.
pub fn sync(
    source: &SourceDb,
    index: &mut IndexDb,
    overlap: u32,
    chunking: &ChunkParams,
) -> Result<SyncReport, EtlError> {
    let mut r = SyncReport {
        index_path: index.path().display().to_string(),
        watermark_before: index.watermark()?,
        ..SyncReport::default()
    };
    let start = (r.watermark_before - i64::from(overlap)).max(0);
    r.watermark_after = r.watermark_before;

    // An index synced before chunking existed (or whose chunks were wiped)
    // has messages but no chunks: give every chat a first chunking pass.
    let bootstrap_chunking = index.chunk_count()? == 0 && index.message_count()? > 0;

    let tx = index.transaction()?;
    {
        let writer = Writer::new(&tx);
        let mut chat_ids: HashMap<String, i64> = HashMap::new();
        let mut handle_ids: HashMap<String, i64> = HashMap::new();
        let mut dirty_chats: HashSet<i64> = HashSet::new();
        if bootstrap_chunking {
            dirty_chats.extend(writer.all_chat_ids()?);
        }
        // scan_messages takes an infallible callback; the first write error
        // is parked here and turns the rest of the scan into a no-op.
        let mut failure: Option<EtlError> = None;

        source.scan_messages(start, |m| {
            if failure.is_some() {
                return;
            }
            let outcome = (|| -> Result<(Upsert, Option<i64>), EtlError> {
                let chat_id = match &m.chat_guid {
                    Some(guid) => Some(match chat_ids.get(guid) {
                        Some(&id) => id,
                        None => {
                            let id = writer.upsert_chat(
                                guid,
                                m.is_group_chat(),
                                m.chat_display_name.as_deref(),
                            )?;
                            chat_ids.insert(guid.clone(), id);
                            id
                        }
                    }),
                    None => None,
                };
                let sender_id = match (m.is_from_me, &m.sender_handle) {
                    (false, Some(h)) => Some(match handle_ids.get(h) {
                        Some(&id) => id,
                        None => {
                            let id = writer.upsert_handle(h)?;
                            handle_ids.insert(h.clone(), id);
                            id
                        }
                    }),
                    _ => None,
                };
                Ok((writer.upsert_message(&m, chat_id, sender_id)?, chat_id))
            })();

            match outcome {
                Ok((changed @ (Upsert::Inserted | Upsert::Updated), chat_id)) => {
                    match changed {
                        Upsert::Inserted => r.inserted += 1,
                        _ => r.updated += 1,
                    }
                    if let Some(id) = chat_id {
                        dirty_chats.insert(id);
                    }
                }
                Ok((Upsert::Unchanged, _)) => r.unchanged += 1,
                Err(e) => {
                    failure = Some(e);
                    return;
                }
            }
            r.scanned += 1;
            r.watermark_after = r.watermark_after.max(m.rowid);
        })?;

        if let Some(e) = failure {
            return Err(e); // tx drops without commit: full rollback
        }
        for chat_id in dirty_chats {
            writer.rechunk_chat(chat_id, chunking)?;
            r.rechunked_chats += 1;
        }
        writer.set_watermark(r.watermark_after)?;
    }
    tx.commit().map_err(IndexError::from)?;

    r.total_messages = index.message_count()?;
    r.total_chats = index.chat_count()?;
    r.total_handles = index.handle_count()?;
    r.total_chunks = index.chunk_count()?;
    Ok(r)
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct EmbedReport {
    pub model: String,
    pub embedded: u64,
    pub pruned: u64,
    pub total: u64,
}

/// Embed every chunk that has no stored vector yet. Each batch commits on
/// its own, so an interrupted run resumes where it left off. Progress goes
/// to stderr via `progress` (chunks done, chunks to do).
pub fn embed_missing(
    index: &mut IndexDb,
    embedder: &mut dyn Embedder,
    mut progress: impl FnMut(u64, u64),
) -> Result<EmbedReport, EtlError> {
    // Vectors from different models are not comparable; a model switch
    // starts over.
    index.ensure_embedding_model(&embedder.id())?;
    let mut r = EmbedReport {
        model: embedder.id(),
        pruned: index.prune_orphan_embeddings()?,
        ..EmbedReport::default()
    };

    // Batch sizing: large enough to amortize per-call overhead, small
    // enough that each commit is prompt and interruption loses little.
    const STORE_BATCH: usize = 256;
    let missing = index.missing_embeddings()?;
    let todo = missing.len() as u64;
    for batch in missing.chunks(STORE_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = embedder.embed_docs(&texts).map_err(EtlError::Embedding)?;
        let items: Vec<(String, Vec<f32>)> =
            batch.iter().map(|(h, _)| h.clone()).zip(vectors).collect();
        index.store_embeddings(&r.model, &items)?;
        r.embedded += batch.len() as u64;
        progress(r.embedded, todo);
    }

    r.total = index.embedding_count()?;
    Ok(r)
}

impl fmt::Display for EmbedReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Embeddings ({})", self.model)?;
        writeln!(f, "  Added:  {}", self.embedded)?;
        if self.pruned > 0 {
            writeln!(f, "  Pruned: {}", self.pruned)?;
        }
        write!(f, "  Total:  {}", self.total)
    }
}

impl fmt::Display for SyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Index database: {}", self.index_path)?;
        writeln!(f)?;
        writeln!(f, "This run")?;
        if self.watermark_before > 0 {
            writeln!(f, "  Scanned:   {} (incremental)", self.scanned)?;
        } else {
            writeln!(f, "  Scanned:   {} (initial full sync)", self.scanned)?;
        }
        writeln!(f, "  Inserted:  {}", self.inserted)?;
        writeln!(f, "  Updated:   {}", self.updated)?;
        writeln!(f, "  Unchanged: {}", self.unchanged)?;
        writeln!(f, "  Rechunked: {} chats", self.rechunked_chats)?;
        writeln!(f)?;
        writeln!(f, "Index totals")?;
        writeln!(f, "  Messages:  {}", self.total_messages)?;
        writeln!(f, "  Chats:     {}", self.total_chats)?;
        writeln!(f, "  Handles:   {}", self.total_handles)?;
        writeln!(f, "  Chunks:    {}", self.total_chunks)?;
        write!(f, "  Watermark: source ROWID {}", self.watermark_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_display_mentions_initial_sync_when_no_watermark() {
        let r = SyncReport::default();
        assert!(r.to_string().contains("initial full sync"));
    }

    #[test]
    fn report_stays_serializable_for_future_json_flag() {
        let serialized = toml::to_string(&SyncReport::default()).unwrap();
        assert!(serialized.contains("inserted"));
    }
}
