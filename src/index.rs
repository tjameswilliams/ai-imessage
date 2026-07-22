//! The destination index database: a normalized, private copy of the
//! Messages history under `~/Library/Application Support/ai-imessage/`.
//!
//! Invariants:
//! - Created with owner-only file permissions (0600): the index contains
//!   full message bodies.
//! - Schema changes bump `user_version`; an index with a newer version than
//!   this binary understands is refused, never silently migrated down.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, params};

use crate::model::ExtractedMessage;

/// Current destination schema version, stored in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i32 = 1;

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
";

const WATERMARK_KEY: &str = "source_watermark_rowid";

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
            "DELETE FROM messages;
             DELETE FROM chats;
             DELETE FROM handles;
             DELETE FROM meta;",
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn watermark_of(conn: &Connection) -> Result<i64, IndexError> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [WATERMARK_KEY],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
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
