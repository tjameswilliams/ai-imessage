//! Synthetic Apple Messages database fixtures.
//!
//! Tests must never require a developer's live `chat.db`, so these builders
//! recreate the relevant slice of Apple's schema (modern and legacy
//! variants) with controllable rows.

#![allow(dead_code)] // each test binary compiles this module separately

use std::path::PathBuf;

use ai_imessage::typedstream;
use rusqlite::{Connection, params};
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaVariant {
    /// Ventura-era schema: attributedBody, edits, retractions, replies.
    Modern,
    /// Pre-High-Sierra-era schema: plain text column only, second-resolution
    /// timestamps, none of the newer columns.
    Legacy,
}

pub struct Fixture {
    pub dir: TempDir,
    pub db_path: PathBuf,
    pub variant: SchemaVariant,
}

pub struct MessageSpec<'a> {
    pub guid: &'a str,
    pub text: Option<&'a str>,
    /// Encoded into a real typedstream blob in `attributedBody`.
    pub attributed_text: Option<&'a str>,
    /// `handle.ROWID` of the sender; `None` for messages from me.
    pub handle_id: Option<i64>,
    pub is_from_me: bool,
    /// Raw value for `message.date` (nanoseconds on Modern, seconds on Legacy).
    pub date: i64,
    pub service: Option<&'a str>,
    pub date_edited: Option<i64>,
    pub date_retracted: Option<i64>,
    pub thread_originator_guid: Option<&'a str>,
    pub item_type: i64,
    pub associated_message_type: i64,
}

impl Default for MessageSpec<'_> {
    fn default() -> Self {
        MessageSpec {
            guid: "unnamed-guid",
            text: None,
            attributed_text: None,
            handle_id: None,
            is_from_me: false,
            date: 0,
            service: Some("iMessage"),
            date_edited: None,
            date_retracted: None,
            thread_originator_guid: None,
            item_type: 0,
            associated_message_type: 0,
        }
    }
}

impl Fixture {
    pub fn new(variant: SchemaVariant) -> Self {
        let dir = TempDir::new().expect("create fixture tempdir");
        let db_path = dir.path().join("chat.db");
        let conn = Connection::open(&db_path).expect("create fixture db");
        conn.execute_batch(schema_sql(variant))
            .expect("create schema");
        Fixture {
            dir,
            db_path,
            variant,
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).expect("open fixture db")
    }

    pub fn add_handle(&self, id: &str) -> i64 {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO handle (id, service) VALUES (?1, 'iMessage')",
            [id],
        )
        .expect("insert handle");
        conn.last_insert_rowid()
    }

    pub fn add_chat(&self, guid: &str, style: i64, display_name: Option<&str>) -> i64 {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO chat (guid, style, display_name) VALUES (?1, ?2, ?3)",
            params![guid, style, display_name],
        )
        .expect("insert chat");
        conn.last_insert_rowid()
    }

    pub fn add_message(&self, spec: &MessageSpec) -> i64 {
        let blob = spec.attributed_text.map(typedstream::encode_text);
        let conn = self.conn();
        match self.variant {
            SchemaVariant::Modern => {
                conn.execute(
                    "INSERT INTO message
                       (guid, text, attributedBody, handle_id, is_from_me, date, service,
                        date_edited, date_retracted, thread_originator_guid,
                        item_type, associated_message_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        spec.guid,
                        spec.text,
                        blob,
                        spec.handle_id.unwrap_or(0),
                        spec.is_from_me as i64,
                        spec.date,
                        spec.service,
                        spec.date_edited.unwrap_or(0),
                        spec.date_retracted.unwrap_or(0),
                        spec.thread_originator_guid,
                        spec.item_type,
                        spec.associated_message_type,
                    ],
                )
                .expect("insert modern message");
            }
            SchemaVariant::Legacy => {
                conn.execute(
                    "INSERT INTO message
                       (guid, text, handle_id, is_from_me, date, service, item_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        spec.guid,
                        spec.text,
                        spec.handle_id.unwrap_or(0),
                        spec.is_from_me as i64,
                        spec.date,
                        spec.service,
                        spec.item_type,
                    ],
                )
                .expect("insert legacy message");
            }
        }
        conn.last_insert_rowid()
    }

    /// Simulate an in-place edit (Modern schema): replace the body and set
    /// `date_edited`, exactly as Messages does when a message is edited.
    pub fn edit_message(&self, guid: &str, new_attributed_text: &str, date_edited: i64) {
        assert_eq!(self.variant, SchemaVariant::Modern, "edits are Modern-only");
        let blob = typedstream::encode_text(new_attributed_text);
        self.conn()
            .execute(
                "UPDATE message
                   SET text = NULL, attributedBody = ?2, date_edited = ?3
                 WHERE guid = ?1",
                params![guid, blob, date_edited],
            )
            .expect("edit message");
    }

    /// Simulate a retraction (unsend): the body is cleared and
    /// `date_retracted` set.
    pub fn retract_message(&self, guid: &str, date_retracted: i64) {
        assert_eq!(
            self.variant,
            SchemaVariant::Modern,
            "retractions are Modern-only"
        );
        self.conn()
            .execute(
                "UPDATE message
                   SET text = NULL, attributedBody = NULL, date_retracted = ?2
                 WHERE guid = ?1",
                params![guid, date_retracted],
            )
            .expect("retract message");
    }

    pub fn link_chat_message(&self, chat_id: i64, message_id: i64) {
        self.conn()
            .execute(
                "INSERT INTO chat_message_join (chat_id, message_id) VALUES (?1, ?2)",
                [chat_id, message_id],
            )
            .expect("link chat/message");
    }

    pub fn link_chat_handle(&self, chat_id: i64, handle_id: i64) {
        self.conn()
            .execute(
                "INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (?1, ?2)",
                [chat_id, handle_id],
            )
            .expect("link chat/handle");
    }
}

fn schema_sql(variant: SchemaVariant) -> &'static str {
    match variant {
        SchemaVariant::Modern => {
            "CREATE TABLE handle (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               id TEXT NOT NULL,
               service TEXT
             );
             CREATE TABLE chat (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               guid TEXT NOT NULL,
               style INTEGER,
               display_name TEXT
             );
             CREATE TABLE message (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               guid TEXT NOT NULL,
               text TEXT,
               attributedBody BLOB,
               handle_id INTEGER DEFAULT 0,
               is_from_me INTEGER DEFAULT 0,
               date INTEGER DEFAULT 0,
               service TEXT,
               date_edited INTEGER DEFAULT 0,
               date_retracted INTEGER DEFAULT 0,
               thread_originator_guid TEXT,
               item_type INTEGER DEFAULT 0,
               associated_message_type INTEGER DEFAULT 0
             );
             CREATE TABLE chat_message_join (
               chat_id INTEGER,
               message_id INTEGER,
               message_date INTEGER DEFAULT 0
             );
             CREATE TABLE chat_handle_join (
               chat_id INTEGER,
               handle_id INTEGER
             );"
        }
        SchemaVariant::Legacy => {
            "CREATE TABLE handle (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               id TEXT NOT NULL,
               service TEXT
             );
             CREATE TABLE chat (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               guid TEXT NOT NULL,
               style INTEGER,
               display_name TEXT
             );
             CREATE TABLE message (
               ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
               guid TEXT NOT NULL,
               text TEXT,
               handle_id INTEGER DEFAULT 0,
               is_from_me INTEGER DEFAULT 0,
               date INTEGER DEFAULT 0,
               service TEXT,
               item_type INTEGER DEFAULT 0
             );
             CREATE TABLE chat_message_join (
               chat_id INTEGER,
               message_id INTEGER
             );
             CREATE TABLE chat_handle_join (
               chat_id INTEGER,
               handle_id INTEGER
             );"
        }
    }
}

/// Convert an RFC 3339 timestamp to Apple nanoseconds (modern `message.date`).
pub fn apple_ns(rfc3339: &str) -> i64 {
    apple_secs(rfc3339) * 1_000_000_000
}

/// Convert an RFC 3339 timestamp to Apple seconds (legacy `message.date`).
pub fn apple_secs(rfc3339: &str) -> i64 {
    let dt = chrono::DateTime::parse_from_rfc3339(rfc3339).expect("valid RFC 3339 in fixture");
    dt.timestamp() - 978_307_200
}
