//! Incremental sync from the Apple Messages database into the local index.
//!
//! Strategy: messages are appended to `chat.db` with monotonically
//! increasing ROWIDs, but recent rows also mutate in place (edits,
//! retractions, delivery updates). Each run therefore rescans from
//! `watermark - overlap` and upserts by GUID; the content hash decides
//! whether an already-known message actually changed.

use std::collections::HashMap;
use std::fmt;

use serde::Serialize;

use crate::extract::{SourceDb, SourceError};
use crate::index::{IndexDb, IndexError, Upsert, Writer};

#[derive(Debug, thiserror::Error)]
pub enum EtlError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Index(#[from] IndexError),
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
    pub total_messages: u64,
    pub total_chats: u64,
    pub total_handles: u64,
}

/// Run one sync pass. All writes happen in a single transaction: a failed
/// run leaves the index exactly as the previous run did.
pub fn sync(source: &SourceDb, index: &mut IndexDb, overlap: u32) -> Result<SyncReport, EtlError> {
    let mut r = SyncReport {
        index_path: index.path().display().to_string(),
        watermark_before: index.watermark()?,
        ..SyncReport::default()
    };
    let start = (r.watermark_before - i64::from(overlap)).max(0);
    r.watermark_after = r.watermark_before;

    let tx = index.transaction()?;
    {
        let writer = Writer::new(&tx);
        let mut chat_ids: HashMap<String, i64> = HashMap::new();
        let mut handle_ids: HashMap<String, i64> = HashMap::new();
        // scan_messages takes an infallible callback; the first write error
        // is parked here and turns the rest of the scan into a no-op.
        let mut failure: Option<EtlError> = None;

        source.scan_messages(start, |m| {
            if failure.is_some() {
                return;
            }
            let outcome = (|| -> Result<Upsert, EtlError> {
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
                Ok(writer.upsert_message(&m, chat_id, sender_id)?)
            })();

            match outcome {
                Ok(Upsert::Inserted) => r.inserted += 1,
                Ok(Upsert::Updated) => r.updated += 1,
                Ok(Upsert::Unchanged) => r.unchanged += 1,
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
        writer.set_watermark(r.watermark_after)?;
    }
    tx.commit().map_err(IndexError::from)?;

    r.total_messages = index.message_count()?;
    r.total_chats = index.chat_count()?;
    r.total_handles = index.handle_count()?;
    Ok(r)
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
        writeln!(f)?;
        writeln!(f, "Index totals")?;
        writeln!(f, "  Messages:  {}", self.total_messages)?;
        writeln!(f, "  Chats:     {}", self.total_chats)?;
        writeln!(f, "  Handles:   {}", self.total_handles)?;
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
