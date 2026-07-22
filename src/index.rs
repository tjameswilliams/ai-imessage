//! The destination index database: a normalized, private copy of the
//! Messages history under `~/Library/Application Support/ai-imessage/`.
//!
//! Invariants:
//! - Created with owner-only file permissions (0600): the index contains
//!   full message bodies.
//! - Schema changes bump `user_version`; an index with a newer version than
//!   this binary understands is refused, never silently migrated down.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, params};

use crate::chunk::{ChunkInput, ChunkParams, chunk_messages};
use crate::model::ExtractedMessage;

/// Current destination schema version, stored in `PRAGMA user_version`.
/// Additive changes only so far: opening an older index under a newer
/// binary creates the missing tables in place.
pub const SCHEMA_VERSION: i32 = 3;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chats (
  id           INTEGER PRIMARY KEY,
  guid         TEXT NOT NULL UNIQUE,
  is_group     INTEGER,             -- 1 group, 0 direct, NULL unknown
  display_name TEXT
);
CREATE TABLE IF NOT EXISTS handles (
  id     INTEGER PRIMARY KEY,
  handle TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS messages (
  id              INTEGER PRIMARY KEY,
  guid            TEXT NOT NULL UNIQUE,
  source_rowid    INTEGER NOT NULL,
  chat_id         INTEGER REFERENCES chats(id),
  sender_id       INTEGER REFERENCES handles(id),  -- NULL for from-me
  is_from_me      INTEGER NOT NULL DEFAULT 0,
  sent_at_ms      INTEGER,
  text            TEXT,
  service         TEXT,
  reply_to_guid   TEXT,
  edited_at_ms    INTEGER,
  retracted_at_ms INTEGER,
  is_tapback      INTEGER NOT NULL DEFAULT 0,
  associated_type INTEGER,
  is_system_event INTEGER NOT NULL DEFAULT 0,
  content_hash    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_chat_sent ON messages(chat_id, sent_at_ms);
CREATE INDEX IF NOT EXISTS messages_source_rowid ON messages(source_rowid);
CREATE TABLE IF NOT EXISTS chunks (
  id            INTEGER PRIMARY KEY,
  chat_id       INTEGER NOT NULL REFERENCES chats(id),
  seq           INTEGER NOT NULL,   -- position within the chat
  started_at_ms INTEGER,
  ended_at_ms   INTEGER,
  message_count INTEGER NOT NULL,
  text          TEXT NOT NULL,
  content_hash  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_chat ON chunks(chat_id, seq);
CREATE INDEX IF NOT EXISTS chunks_hash ON chunks(content_hash);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text);
CREATE TABLE IF NOT EXISTS embeddings (
  chunk_hash TEXT PRIMARY KEY,   -- chunks.content_hash
  model      TEXT NOT NULL,
  vector     BLOB NOT NULL       -- f32 little-endian
);
";

const WATERMARK_KEY: &str = "source_watermark_rowid";
const EMBEDDING_MODEL_KEY: &str = "embedding_model";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("could not create index directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not open index database {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "index database {path} has schema version {found}, newer than this \
         binary supports ({supported}) — upgrade ai-imessage"
    )]
    NewerSchema {
        path: PathBuf,
        found: i32,
        supported: i32,
    },
    #[error("index database query failed: {0}")]
    Query(#[from] rusqlite::Error),
    #[error("could not restrict permissions on {path}: {source}")]
    Permissions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Outcome of upserting one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    Updated,
    Unchanged,
}

/// One keyword-search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub chunk_id: i64,
    /// Chat display name, or a participant handle, or the chat GUID.
    pub chat_label: String,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub message_count: i64,
    /// Keyword search: FTS5 snippet with matches wrapped in «guillemets».
    /// Vector search: the truncated start of the chunk.
    pub snippet: String,
    /// Cosine similarity, vector search only.
    pub score: Option<f32>,
}

fn truncate_snippet(text: &str) -> String {
    const MAX_CHARS: usize = 200;
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let mut s: String = text.chars().take(MAX_CHARS).collect();
    s.push('…');
    s
}

/// Quote every whitespace-separated term so user input is always a valid
/// FTS5 query (terms AND together; operators lose their meaning).
fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A read-write connection to the destination index.
#[derive(Debug)]
pub struct IndexDb {
    conn: Connection,
    path: PathBuf,
}

impl IndexDb {
    /// Open (creating if needed) the index at `path`. The parent directory
    /// is created with owner-only permissions, as is the database file.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir).map_err(|e| IndexError::CreateDir {
                path: dir.to_path_buf(),
                source: e,
            })?;
            restrict_permissions(dir, 0o700)?;
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| IndexError::Open {
            path: path.to_path_buf(),
            source: e,
        })?;
        restrict_permissions(path, 0o600)?;

        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(IndexError::NewerSchema {
                path: path.to_path_buf(),
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        conn.execute_batch(SCHEMA_SQL)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        Ok(IndexDb {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>, IndexError> {
        Ok(self.conn.transaction()?)
    }

    /// Highest source ROWID already ingested; 0 before the first sync.
    pub fn watermark(&self) -> Result<i64, IndexError> {
        watermark_of(&self.conn)
    }

    pub fn message_count(&self) -> Result<u64, IndexError> {
        self.count("messages")
    }

    pub fn chat_count(&self) -> Result<u64, IndexError> {
        self.count("chats")
    }

    pub fn handle_count(&self) -> Result<u64, IndexError> {
        self.count("handles")
    }

    pub fn chunk_count(&self) -> Result<u64, IndexError> {
        self.count("chunks")
    }

    pub fn embedding_count(&self) -> Result<u64, IndexError> {
        self.count("embeddings")
    }

    /// Chunks that have no stored embedding yet, as (content_hash, text).
    /// Hashes are distinct even if several chunk rows share content.
    pub fn missing_embeddings(&self) -> Result<Vec<(String, String)>, IndexError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT content_hash, MIN(text) FROM chunks
             WHERE content_hash NOT IN (SELECT chunk_hash FROM embeddings)
             GROUP BY content_hash",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Store one batch of embeddings in its own transaction, so a long
    /// embedding run that is interrupted keeps everything finished so far.
    pub fn store_embeddings(
        &mut self,
        model: &str,
        items: &[(String, Vec<f32>)],
    ) -> Result<(), IndexError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO embeddings (chunk_hash, model, vector) VALUES (?1, ?2, ?3)
                 ON CONFLICT(chunk_hash) DO UPDATE SET
                   model = excluded.model, vector = excluded.vector",
            )?;
            for (hash, vector) in items {
                stmt.execute(params![hash, model, crate::embed::vector_to_blob(vector)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record which embedding model the index was built with. A model
    /// change wipes all stored vectors (they are not comparable across
    /// models); returns true when that happened.
    pub fn ensure_embedding_model(&mut self, model: &str) -> Result<bool, IndexError> {
        let stored = meta_get(&self.conn, EMBEDDING_MODEL_KEY)?;
        match stored.as_deref() {
            Some(s) if s == model => Ok(false),
            Some(_) => {
                let tx = self.conn.transaction()?;
                tx.execute("DELETE FROM embeddings", [])?;
                meta_set(&tx, EMBEDDING_MODEL_KEY, model)?;
                tx.commit()?;
                Ok(true)
            }
            None => {
                meta_set(&self.conn, EMBEDDING_MODEL_KEY, model)?;
                Ok(false)
            }
        }
    }

    /// Drop embeddings whose chunk no longer exists (superseded by
    /// re-chunking). Returns how many were removed.
    pub fn prune_orphan_embeddings(&mut self) -> Result<u64, IndexError> {
        let n = self.conn.execute(
            "DELETE FROM embeddings
             WHERE chunk_hash NOT IN (SELECT content_hash FROM chunks)",
            [],
        )?;
        Ok(n as u64)
    }

    /// Brute-force cosine similarity over every embedded chunk, best first.
    /// At hundreds of thousands of chunks this is still tens of
    /// milliseconds; no vector index needed at this scale.
    pub fn vector_search(&self, query: &[f32], limit: u32) -> Result<Vec<SearchHit>, IndexError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT
               c.id,
               COALESCE(
                 NULLIF(ch.display_name, ''),
                 (SELECT h.handle FROM messages m JOIN handles h ON h.id = m.sender_id
                   WHERE m.chat_id = ch.id LIMIT 1),
                 ch.guid
               ),
               c.started_at_ms,
               c.ended_at_ms,
               c.message_count,
               c.text,
               e.vector
             FROM chunks c
             JOIN embeddings e ON e.chunk_hash = c.content_hash
             JOIN chats ch ON ch.id = c.chat_id",
        )?;
        let mut hits: Vec<(f32, SearchHit)> = stmt
            .query_map([], |r| {
                let text: String = r.get(5)?;
                let blob: Vec<u8> = r.get(6)?;
                Ok((
                    crate::embed::cosine(query, &crate::embed::blob_to_vector(&blob)),
                    SearchHit {
                        chunk_id: r.get(0)?,
                        chat_label: r.get(1)?,
                        started_at_ms: r.get(2)?,
                        ended_at_ms: r.get(3)?,
                        message_count: r.get(4)?,
                        snippet: truncate_snippet(&text),
                        score: None,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Orthogonal or opposed vectors share no signal with the query;
        // "nearest" among those is noise, not a result.
        hits.retain(|(score, _)| *score > 0.0);
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.truncate(limit as usize);
        Ok(hits
            .into_iter()
            .map(|(score, mut h)| {
                h.score = Some(score);
                h
            })
            .collect())
    }

    /// FTS5 keyword search over conversation chunks, best match first.
    /// The user query is sanitized into quoted terms (implicit AND), so
    /// FTS5 operator syntax can never cause an error.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchHit>, IndexError> {
        let fts_query = sanitize_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT
               c.id,
               COALESCE(
                 NULLIF(ch.display_name, ''),  -- Messages stores '' not NULL
                 (SELECT h.handle FROM messages m JOIN handles h ON h.id = m.sender_id
                   WHERE m.chat_id = ch.id LIMIT 1),
                 ch.guid
               ),
               c.started_at_ms,
               c.ended_at_ms,
               c.message_count,
               snippet(chunks_fts, 0, '«', '»', ' … ', 24)
             FROM chunks_fts
             JOIN chunks c ON c.id = chunks_fts.rowid
             JOIN chats ch ON ch.id = c.chat_id
             WHERE chunks_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![fts_query, limit], |r| {
                Ok(SearchHit {
                    chunk_id: r.get(0)?,
                    chat_label: r.get(1)?,
                    started_at_ms: r.get(2)?,
                    ended_at_ms: r.get(3)?,
                    message_count: r.get(4)?,
                    snippet: r.get(5)?,
                    score: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    fn count(&self, table: &str) -> Result<u64, IndexError> {
        // Table names are compile-time constants, never user input.
        let n: i64 = self
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// Drop all ingested data and reset the watermark, keeping the schema.
    /// The next sync re-ingests from scratch.
    pub fn reset(&mut self) -> Result<(), IndexError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM embeddings;
             DELETE FROM chunks_fts;
             DELETE FROM chunks;
             DELETE FROM messages;
             DELETE FROM chats;
             DELETE FROM handles;
             DELETE FROM meta;",
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn watermark_of(conn: &Connection) -> Result<i64, IndexError> {
    let v = meta_get(conn, WATERMARK_KEY)?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, IndexError> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(IndexError::from(other)),
        })
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), IndexError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<(), IndexError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| {
        IndexError::Permissions {
            path: path.to_path_buf(),
            source: e,
        }
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<(), IndexError> {
    Ok(())
}

/// Write-side of one sync run, scoped to a transaction. All statements go
/// through the connection's prepared-statement cache.
pub struct Writer<'tx> {
    tx: &'tx Transaction<'tx>,
}

impl<'tx> Writer<'tx> {
    pub fn new(tx: &'tx Transaction<'tx>) -> Self {
        Writer { tx }
    }

    /// Insert or refresh a chat by GUID, returning its index-side id.
    /// Display name and group flag follow the source on every run.
    pub fn upsert_chat(
        &self,
        guid: &str,
        is_group: Option<bool>,
        display_name: Option<&str>,
    ) -> Result<i64, IndexError> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO chats (guid, is_group, display_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(guid) DO UPDATE SET
               is_group = excluded.is_group,
               display_name = excluded.display_name
             RETURNING id",
        )?;
        Ok(stmt.query_row(params![guid, is_group, display_name], |r| r.get(0))?)
    }

    pub fn upsert_handle(&self, handle: &str) -> Result<i64, IndexError> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO handles (handle) VALUES (?1)
             ON CONFLICT(handle) DO UPDATE SET handle = excluded.handle
             RETURNING id",
        )?;
        Ok(stmt.query_row([handle], |r| r.get(0))?)
    }

    /// Insert a new message or, when its content hash changed (edit,
    /// retraction), update it in place. Identity is the source GUID.
    pub fn upsert_message(
        &self,
        m: &ExtractedMessage,
        chat_id: Option<i64>,
        sender_id: Option<i64>,
    ) -> Result<Upsert, IndexError> {
        let hash = m.content_hash();

        let mut find = self
            .tx
            .prepare_cached("SELECT id, content_hash FROM messages WHERE guid = ?1")?;
        let existing: Option<(i64, String)> = find
            .query_row([&m.guid], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;

        let sent_at_ms = m.sent_at.map(|t| t.timestamp_millis());
        let edited_at_ms = m.edited_at.map(|t| t.timestamp_millis());
        let retracted_at_ms = m.retracted_at.map(|t| t.timestamp_millis());

        match existing {
            None => {
                let mut insert = self.tx.prepare_cached(
                    "INSERT INTO messages
                       (guid, source_rowid, chat_id, sender_id, is_from_me,
                        sent_at_ms, text, service, reply_to_guid,
                        edited_at_ms, retracted_at_ms,
                        is_tapback, associated_type, is_system_event, content_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                )?;
                insert.execute(params![
                    m.guid,
                    m.rowid,
                    chat_id,
                    sender_id,
                    m.is_from_me,
                    sent_at_ms,
                    m.text,
                    m.service,
                    m.reply_to_guid,
                    edited_at_ms,
                    retracted_at_ms,
                    m.is_tapback,
                    m.associated_type,
                    m.is_system_event,
                    hash,
                ])?;
                Ok(Upsert::Inserted)
            }
            Some((_, old_hash)) if old_hash == hash => Ok(Upsert::Unchanged),
            Some((id, _)) => {
                // The source row mutated in place: an edit or retraction.
                // Text and timestamps follow the source; a retracted
                // message's cleared body overwrites the stored one.
                let mut update = self.tx.prepare_cached(
                    "UPDATE messages SET
                       source_rowid = ?2, chat_id = ?3, sender_id = ?4,
                       sent_at_ms = ?5, text = ?6,
                       edited_at_ms = ?7, retracted_at_ms = ?8,
                       content_hash = ?9
                     WHERE id = ?1",
                )?;
                update.execute(params![
                    id,
                    m.rowid,
                    chat_id,
                    sender_id,
                    sent_at_ms,
                    m.text,
                    edited_at_ms,
                    retracted_at_ms,
                    hash,
                ])?;
                Ok(Upsert::Updated)
            }
        }
    }

    pub fn all_chat_ids(&self) -> Result<Vec<i64>, IndexError> {
        let mut stmt = self.tx.prepare_cached("SELECT id FROM chats")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Rebuild the chunks of one chat from its indexed messages, reusing
    /// rows whose content hash is unchanged so downstream layers keyed on
    /// chunk identity (embeddings) are not invalidated by appends.
    ///
    /// Returns (kept, inserted, deleted) chunk counts.
    pub fn rechunk_chat(
        &self,
        chat_id: i64,
        params_: &ChunkParams,
    ) -> Result<(u64, u64, u64), IndexError> {
        let mut load = self.tx.prepare_cached(
            "SELECT
               CASE WHEN m.is_from_me THEN 'Me' ELSE COALESCE(h.handle, 'unknown') END,
               m.text,
               m.sent_at_ms
             FROM messages m
             LEFT JOIN handles h ON h.id = m.sender_id
             WHERE m.chat_id = ?1
               AND m.text IS NOT NULL
               AND m.is_system_event = 0
             ORDER BY COALESCE(m.sent_at_ms, 0), m.id",
        )?;
        let msgs: Vec<ChunkInput> = load
            .query_map([chat_id], |r| {
                Ok(ChunkInput {
                    sender: r.get(0)?,
                    text: r.get(1)?,
                    sent_at_ms: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let new_chunks = chunk_messages(&chat_id.to_string(), &msgs, params_);

        // Existing chunks by content hash; matches keep their row (and id).
        let mut find = self
            .tx
            .prepare_cached("SELECT id, content_hash FROM chunks WHERE chat_id = ?1")?;
        let mut existing: HashMap<String, Vec<i64>> = HashMap::new();
        for row in find.query_map([chat_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (id, hash) = row?;
            existing.entry(hash).or_default().push(id);
        }

        let (mut kept, mut inserted) = (0u64, 0u64);
        for (seq, c) in new_chunks.iter().enumerate() {
            let seq = seq as i64;
            match existing.get_mut(&c.content_hash).and_then(Vec::pop) {
                Some(id) => {
                    self.tx
                        .prepare_cached("UPDATE chunks SET seq = ?2 WHERE id = ?1")?
                        .execute(params![id, seq])?;
                    kept += 1;
                }
                None => {
                    self.tx
                        .prepare_cached(
                            "INSERT INTO chunks
                               (chat_id, seq, started_at_ms, ended_at_ms,
                                message_count, text, content_hash)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        )?
                        .execute(params![
                            chat_id,
                            seq,
                            c.started_at_ms,
                            c.ended_at_ms,
                            c.message_count,
                            c.text,
                            c.content_hash,
                        ])?;
                    let id = self.tx.last_insert_rowid();
                    self.tx
                        .prepare_cached("INSERT INTO chunks_fts (rowid, text) VALUES (?1, ?2)")?
                        .execute(params![id, c.text])?;
                    inserted += 1;
                }
            }
        }

        let mut deleted = 0u64;
        for id in existing.into_values().flatten() {
            self.tx
                .prepare_cached("DELETE FROM chunks WHERE id = ?1")?
                .execute([id])?;
            self.tx
                .prepare_cached("DELETE FROM chunks_fts WHERE rowid = ?1")?
                .execute([id])?;
            deleted += 1;
        }
        Ok((kept, inserted, deleted))
    }

    pub fn set_watermark(&self, rowid: i64) -> Result<(), IndexError> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?;
        stmt.execute(params![WATERMARK_KEY, rowid.to_string()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp() -> (TempDir, IndexDb) {
        let dir = TempDir::new().unwrap();
        let db = IndexDb::open(&dir.path().join("nested/index.sqlite")).unwrap();
        (dir, db)
    }

    #[test]
    fn open_creates_schema_and_parent_directory() {
        let (_dir, db) = open_temp();
        assert_eq!(db.message_count().unwrap(), 0);
        assert_eq!(db.chat_count().unwrap(), 0);
        assert_eq!(db.handle_count().unwrap(), 0);
    }

    #[test]
    fn watermark_defaults_to_zero_and_roundtrips() {
        let (_dir, mut db) = open_temp();
        assert_eq!(db.watermark().unwrap(), 0);
        let tx = db.transaction().unwrap();
        Writer::new(&tx).set_watermark(42).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.watermark().unwrap(), 42);
    }

    #[test]
    fn watermark_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.sqlite");
        {
            let mut db = IndexDb::open(&path).unwrap();
            let tx = db.transaction().unwrap();
            Writer::new(&tx).set_watermark(7).unwrap();
            tx.commit().unwrap();
        }
        let db = IndexDb::open(&path).unwrap();
        assert_eq!(db.watermark().unwrap(), 7);
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index.sqlite");
        drop(IndexDb::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        match IndexDb::open(&path) {
            Err(IndexError::NewerSchema { found, .. }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
            }
            other => panic!("expected NewerSchema, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_data_and_watermark() {
        let (_dir, mut db) = open_temp();
        let tx = db.transaction().unwrap();
        let w = Writer::new(&tx);
        w.upsert_handle("+15550100001").unwrap();
        w.set_watermark(9).unwrap();
        tx.commit().unwrap();

        db.reset().unwrap();
        assert_eq!(db.handle_count().unwrap(), 0);
        assert_eq!(db.watermark().unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn index_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/index.sqlite");
        drop(IndexDb::open(&path).unwrap());
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn upsert_chat_is_idempotent_and_refreshes_name() {
        let (_dir, mut db) = open_temp();
        let tx = db.transaction().unwrap();
        let w = Writer::new(&tx);
        let a = w.upsert_chat("c1", Some(false), None).unwrap();
        let b = w.upsert_chat("c1", Some(true), Some("Renamed")).unwrap();
        assert_eq!(a, b);
        tx.commit().unwrap();
        assert_eq!(db.chat_count().unwrap(), 1);
        let (is_group, name): (Option<bool>, Option<String>) = db
            .conn
            .query_row(
                "SELECT is_group, display_name FROM chats WHERE guid = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(is_group, Some(true));
        assert_eq!(name.as_deref(), Some("Renamed"));
    }

    #[test]
    fn upsert_handle_is_idempotent() {
        let (_dir, mut db) = open_temp();
        let tx = db.transaction().unwrap();
        let w = Writer::new(&tx);
        let a = w.upsert_handle("+15550100001").unwrap();
        let b = w.upsert_handle("+15550100001").unwrap();
        assert_eq!(a, b);
        tx.commit().unwrap();
        assert_eq!(db.handle_count().unwrap(), 1);
    }
}
