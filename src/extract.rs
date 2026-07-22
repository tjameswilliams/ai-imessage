//! Read-only extraction from the Apple Messages database.
//!
//! Invariants:
//! - The source database is ONLY ever opened with `SQLITE_OPEN_READ_ONLY`,
//!   plus `PRAGMA query_only = ON` as a second line of defense.
//! - The schema is probed, not assumed: columns added in newer macOS
//!   releases (`date_edited`, `thread_originator_guid`, …) degrade to NULL
//!   when absent instead of failing the query.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Row};

use crate::appledate::apple_time_to_utc;
use crate::model::{CHAT_STYLE_DIRECT, CHAT_STYLE_GROUP, ExtractedMessage, TextSource};
use crate::typedstream;

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("no Messages database found at {0}")]
    NotFound(PathBuf),
    #[error("permission denied reading {0} — this process does not have Full Disk Access")]
    PermissionDenied(PathBuf),
    #[error("could not inspect {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not open {path} read-only: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("{path} does not look like a Messages database (missing tables: {missing})")]
    UnexpectedSchema { path: PathBuf, missing: String },
    #[error("query against the source database failed: {0}")]
    Query(#[from] rusqlite::Error),
}

/// Optional columns detected in this database's `message` table.
#[derive(Debug, Clone, Copy, Default)]
pub struct SchemaCaps {
    pub has_attributed_body: bool,
    pub has_date_edited: bool,
    pub has_date_retracted: bool,
    pub has_thread_originator_guid: bool,
    pub has_item_type: bool,
    pub has_associated_message_type: bool,
}

impl SchemaCaps {
    pub fn summary(&self) -> String {
        fn yn(v: bool) -> &'static str {
            if v { "yes" } else { "no" }
        }
        format!(
            "attributedBody={}, edits={}, retractions={}, replies={}",
            yn(self.has_attributed_body),
            yn(self.has_date_edited),
            yn(self.has_date_retracted),
            yn(self.has_thread_originator_guid),
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatStats {
    pub direct: u64,
    pub group: u64,
    pub total: u64,
}

const REQUIRED_TABLES: &[&str] = &["message", "chat", "handle", "chat_message_join"];

/// A read-only connection to an Apple Messages database.
#[derive(Debug)]
pub struct SourceDb {
    conn: Connection,
    path: PathBuf,
    caps: SchemaCaps,
}

impl SourceDb {
    pub fn open(path: &Path) -> Result<Self, SourceError> {
        match fs::metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SourceError::NotFound(path.to_path_buf()));
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(SourceError::PermissionDenied(path.to_path_buf()));
            }
            Err(e) => {
                return Err(SourceError::Io {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| classify_open_error(path, e))?;

        // Messages.app may be writing; never block it for long.
        conn.busy_timeout(Duration::from_secs(3))?;
        // Belt and suspenders on top of the read-only open flag.
        conn.pragma_update(None, "query_only", true)?;

        let tables = existing_tables(&conn)?;
        let missing: Vec<&str> = REQUIRED_TABLES
            .iter()
            .copied()
            .filter(|t| !tables.contains(*t))
            .collect();
        if !missing.is_empty() {
            return Err(SourceError::UnexpectedSchema {
                path: path.to_path_buf(),
                missing: missing.join(", "),
            });
        }

        let caps = probe_message_columns(&conn)?;
        Ok(SourceDb {
            conn,
            path: path.to_path_buf(),
            caps,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn caps(&self) -> &SchemaCaps {
        &self.caps
    }

    pub fn message_count(&self) -> Result<u64, SourceError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    pub fn chat_stats(&self) -> Result<ChatStats, SourceError> {
        let sql = format!(
            "SELECT
               COALESCE(SUM(CASE WHEN style = {CHAT_STYLE_DIRECT} THEN 1 ELSE 0 END), 0),
               COALESCE(SUM(CASE WHEN style = {CHAT_STYLE_GROUP} THEN 1 ELSE 0 END), 0),
               COUNT(*)
             FROM chat"
        );
        let (direct, group, total): (i64, i64, i64) = self
            .conn
            .query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(ChatStats {
            direct: direct.max(0) as u64,
            group: group.max(0) as u64,
            total: total.max(0) as u64,
        })
    }

    /// Stream every message with `ROWID > after_rowid` in ROWID order.
    ///
    /// Returns the number of rows visited. Streaming (rather than collecting)
    /// keeps memory flat on multi-hundred-thousand-message databases.
    pub fn scan_messages<F>(&self, after_rowid: i64, mut f: F) -> Result<u64, SourceError>
    where
        F: FnMut(ExtractedMessage),
    {
        let sql = select_sql(&self.caps);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([after_rowid], row_to_message)?;
        let mut count = 0u64;
        for row in rows {
            f(row?);
            count += 1;
        }
        Ok(count)
    }

    /// Collect messages after a watermark. Convenience for tests and small
    /// scans; prefer [`SourceDb::scan_messages`] for full passes.
    pub fn collect_messages(&self, after_rowid: i64) -> Result<Vec<ExtractedMessage>, SourceError> {
        let mut out = Vec::new();
        self.scan_messages(after_rowid, |m| out.push(m))?;
        Ok(out)
    }
}

/// Turn a SQLite open failure into the most actionable error possible.
///
/// macOS TCC has a trap: without Full Disk Access, `stat()` on chat.db can
/// SUCCEED while `open()` is denied, so SQLite reports only a generic
/// "unable to open database file" (SQLITE_CANTOPEN). Probing with a plain
/// `File::open` recovers the underlying EPERM/EACCES so the user gets the
/// Full Disk Access guidance instead of a dead-end message.
fn classify_open_error(path: &Path, e: rusqlite::Error) -> SourceError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &e
        && failure.code == rusqlite::ErrorCode::CannotOpen
        && matches!(
            fs::File::open(path),
            Err(io_err) if io_err.kind() == io::ErrorKind::PermissionDenied
        )
    {
        return SourceError::PermissionDenied(path.to_path_buf());
    }
    SourceError::Open {
        path: path.to_path_buf(),
        source: e,
    }
}

fn existing_tables(conn: &Connection) -> Result<HashSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
    names.collect()
}

fn probe_message_columns(conn: &Connection) -> Result<SchemaCaps, rusqlite::Error> {
    let mut stmt = conn.prepare("PRAGMA table_info(message)")?;
    let cols: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?
        .into_iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    Ok(SchemaCaps {
        has_attributed_body: cols.contains("attributedbody"),
        has_date_edited: cols.contains("date_edited"),
        has_date_retracted: cols.contains("date_retracted"),
        has_thread_originator_guid: cols.contains("thread_originator_guid"),
        has_item_type: cols.contains("item_type"),
        has_associated_message_type: cols.contains("associated_message_type"),
    })
}

/// Build the extraction query, substituting NULL for columns this database
/// version does not have. A message linked to several chats (rare, but real
/// after chat merges) is attributed to one chat via MIN(chat_id) so it never
/// appears twice.
fn select_sql(caps: &SchemaCaps) -> String {
    let opt = |present: bool, expr: &str| {
        if present {
            expr.to_string()
        } else {
            "NULL".to_string()
        }
    };
    format!(
        "SELECT
           m.ROWID,
           m.guid,
           m.text,
           {attributed_body},
           m.is_from_me,
           m.date,
           m.service,
           {date_edited},
           {date_retracted},
           {reply_to},
           {item_type},
           {associated_type},
           h.id,
           c.guid,
           c.style,
           c.display_name
         FROM message m
         LEFT JOIN handle h ON h.ROWID = m.handle_id
         LEFT JOIN (
           SELECT message_id, MIN(chat_id) AS chat_id
           FROM chat_message_join
           GROUP BY message_id
         ) cmj ON cmj.message_id = m.ROWID
         LEFT JOIN chat c ON c.ROWID = cmj.chat_id
         WHERE m.ROWID > ?1
         ORDER BY m.ROWID ASC",
        attributed_body = opt(caps.has_attributed_body, "m.attributedBody"),
        date_edited = opt(caps.has_date_edited, "m.date_edited"),
        date_retracted = opt(caps.has_date_retracted, "m.date_retracted"),
        reply_to = opt(caps.has_thread_originator_guid, "m.thread_originator_guid"),
        item_type = opt(caps.has_item_type, "m.item_type"),
        associated_type = opt(
            caps.has_associated_message_type,
            "m.associated_message_type"
        ),
    )
}

fn row_to_message(row: &Row) -> rusqlite::Result<ExtractedMessage> {
    let rowid: i64 = row.get(0)?;
    let guid: Option<String> = row.get(1)?;
    let text_col: Option<String> = row.get(2)?;
    let attributed_body: Option<Vec<u8>> = row.get(3)?;
    let is_from_me: Option<i64> = row.get(4)?;
    let raw_date: Option<i64> = row.get(5)?;
    let service: Option<String> = row.get(6)?;
    let raw_edited: Option<i64> = row.get(7)?;
    let raw_retracted: Option<i64> = row.get(8)?;
    let reply_to_guid: Option<String> = row.get(9)?;
    let item_type: Option<i64> = row.get(10)?;
    let associated_type: Option<i64> = row.get(11)?;
    let sender_handle: Option<String> = row.get(12)?;
    let chat_guid: Option<String> = row.get(13)?;
    let chat_style: Option<i64> = row.get(14)?;
    let chat_display_name: Option<String> = row.get(15)?;

    let (text, text_source) = resolve_text(text_col, attributed_body.as_deref());

    Ok(ExtractedMessage {
        rowid,
        guid: guid.unwrap_or_else(|| format!("missing-guid:{rowid}")),
        chat_guid,
        chat_style,
        chat_display_name,
        sender_handle,
        is_from_me: is_from_me.unwrap_or(0) != 0,
        sent_at: raw_date.and_then(apple_time_to_utc),
        text,
        text_source,
        service,
        reply_to_guid,
        edited_at: raw_edited.and_then(apple_time_to_utc),
        retracted_at: raw_retracted.and_then(apple_time_to_utc),
        is_tapback: associated_type.is_some_and(|t| (2000..4000).contains(&t)),
        is_system_event: item_type.is_some_and(|t| t != 0),
    })
}

/// Pick the message text: the plain `text` column wins; otherwise recover
/// from the typedstream blob. Whitespace-only text counts as absent.
fn resolve_text(text_col: Option<String>, blob: Option<&[u8]>) -> (Option<String>, TextSource) {
    if let Some(t) = text_col
        && !t.trim().is_empty()
    {
        return (Some(t), TextSource::TextColumn);
    }
    if let Some(b) = blob
        && let Some(t) = typedstream::extract_text(b)
        && !t.trim().is_empty()
    {
        return (Some(t), TextSource::AttributedBody);
    }
    (None, TextSource::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_db(dir: &std::path::Path) -> PathBuf {
        let db = dir.join("chat.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT,
               handle_id INTEGER, is_from_me INTEGER, date INTEGER, service TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, style INTEGER,
               display_name TEXT);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);",
        )
        .unwrap();
        db
    }

    #[test]
    fn source_connection_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = SourceDb::open(&minimal_db(dir.path())).unwrap();
        let err = db
            .conn
            .execute("CREATE TABLE should_never_exist (x INTEGER)", [])
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("readonly") || msg.contains("read-only"),
            "expected a read-only error, got: {msg}"
        );
    }

    /// The macOS TCC trap: without Full Disk Access, chat.db can be stat-ed
    /// but not opened. A mode-000 file reproduces the same stat-ok/open-denied
    /// split, and it must classify as PermissionDenied, not a generic error.
    #[cfg(unix)]
    #[test]
    fn stat_ok_but_open_denied_classifies_as_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db = minimal_db(dir.path());
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();

        let err = SourceDb::open(&db).unwrap_err();

        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            matches!(err, SourceError::PermissionDenied(_)),
            "expected PermissionDenied, got {err:?}"
        );
        assert!(err.to_string().contains("Full Disk Access"));
    }

    #[test]
    fn missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = SourceDb::open(&dir.path().join("nope.db")).unwrap_err();
        assert!(matches!(err, SourceError::NotFound(_)));
    }

    #[test]
    fn non_messages_database_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("other.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (x INTEGER);")
            .unwrap();
        let err = SourceDb::open(&db).unwrap_err();
        match err {
            SourceError::UnexpectedSchema { missing, .. } => {
                assert!(missing.contains("message"));
                assert!(missing.contains("chat"));
            }
            other => panic!("expected UnexpectedSchema, got {other:?}"),
        }
    }

    #[test]
    fn non_sqlite_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"definitely not a sqlite file at all........").unwrap();
        assert!(SourceDb::open(&junk).is_err());
    }

    #[test]
    fn caps_are_all_false_on_minimal_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = SourceDb::open(&minimal_db(dir.path())).unwrap();
        let caps = db.caps();
        assert!(!caps.has_attributed_body);
        assert!(!caps.has_date_edited);
        assert!(!caps.has_date_retracted);
        assert!(!caps.has_thread_originator_guid);
    }

    #[test]
    fn caps_summary_mentions_each_capability() {
        let s = SchemaCaps::default().summary();
        for key in ["attributedBody", "edits", "retractions", "replies"] {
            assert!(s.contains(key), "summary missing {key}: {s}");
        }
    }

    #[test]
    fn select_sql_substitutes_null_for_missing_columns() {
        let none = select_sql(&SchemaCaps::default());
        assert!(!none.contains("attributedBody"));
        assert!(!none.contains("date_edited"));
        let all = select_sql(&SchemaCaps {
            has_attributed_body: true,
            has_date_edited: true,
            has_date_retracted: true,
            has_thread_originator_guid: true,
            has_item_type: true,
            has_associated_message_type: true,
        });
        assert!(all.contains("m.attributedBody"));
        assert!(all.contains("m.date_edited"));
        assert!(all.contains("m.thread_originator_guid"));
    }

    #[test]
    fn resolve_text_prefers_text_column() {
        let blob = typedstream::encode_text("from blob");
        let (text, src) = resolve_text(Some("from column".into()), Some(&blob));
        assert_eq!(text.as_deref(), Some("from column"));
        assert_eq!(src, TextSource::TextColumn);
    }

    #[test]
    fn resolve_text_falls_back_to_blob_on_empty_column() {
        let blob = typedstream::encode_text("from blob");
        for col in [None, Some(String::new()), Some("   ".to_string())] {
            let (text, src) = resolve_text(col, Some(&blob));
            assert_eq!(text.as_deref(), Some("from blob"));
            assert_eq!(src, TextSource::AttributedBody);
        }
    }

    #[test]
    fn resolve_text_handles_total_absence() {
        let (text, src) = resolve_text(None, None);
        assert_eq!(text, None);
        assert_eq!(src, TextSource::None);
        let (text, src) = resolve_text(None, Some(b"garbage"));
        assert_eq!(text, None);
        assert_eq!(src, TextSource::None);
    }
}
