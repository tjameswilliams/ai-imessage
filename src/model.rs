//! Normalized domain models, independent of Apple's database schema.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// Where a message's text came from during extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSource {
    /// The plain `message.text` column.
    TextColumn,
    /// Recovered from the `message.attributedBody` typedstream blob.
    AttributedBody,
    /// No text was found in either location.
    None,
}

/// Chat style values used by Messages.
pub const CHAT_STYLE_GROUP: i64 = 43;
pub const CHAT_STYLE_DIRECT: i64 = 45;

/// A message extracted from the Apple Messages database, in normalized form.
#[derive(Debug, Clone)]
pub struct ExtractedMessage {
    pub rowid: i64,
    pub guid: String,
    pub chat_guid: Option<String>,
    pub chat_style: Option<i64>,
    pub chat_display_name: Option<String>,
    pub sender_handle: Option<String>,
    pub is_from_me: bool,
    pub sent_at: Option<DateTime<Utc>>,
    pub text: Option<String>,
    pub text_source: TextSource,
    pub service: Option<String>,
    pub reply_to_guid: Option<String>,
    pub edited_at: Option<DateTime<Utc>>,
    pub retracted_at: Option<DateTime<Utc>>,
    /// True for tapbacks/reactions (associated_message_type 2000–3999).
    pub is_tapback: bool,
    /// Raw `associated_message_type` from the source, when present and
    /// non-zero. Distinguishes reaction kinds (2000 loved, 2001 liked, …)
    /// and removals (3000s).
    pub associated_type: Option<i64>,
    /// True for non-message items such as group membership changes.
    pub is_system_event: bool,
}

impl ExtractedMessage {
    /// `Some(true)` for group chats, `Some(false)` for direct chats, `None`
    /// when the message is not linked to a chat or the style is unknown.
    pub fn is_group_chat(&self) -> Option<bool> {
        match self.chat_style {
            Some(CHAT_STYLE_GROUP) => Some(true),
            Some(CHAT_STYLE_DIRECT) => Some(false),
            _ => None,
        }
    }

    /// Stable hash of the message content, used to detect edits and
    /// retractions across ETL runs without comparing full bodies.
    pub fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [
            self.guid.as_str(),
            self.text.as_deref().unwrap_or(""),
            &self.edited_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            &self
                .retracted_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> ExtractedMessage {
        ExtractedMessage {
            rowid: 1,
            guid: "guid-1".into(),
            chat_guid: Some("chat-1".into()),
            chat_style: Some(CHAT_STYLE_DIRECT),
            chat_display_name: None,
            sender_handle: Some("+15550104477".into()),
            is_from_me: false,
            sent_at: Some(Utc.with_ymd_and_hms(2026, 7, 1, 9, 14, 0).unwrap()),
            text: Some("Hello".into()),
            text_source: TextSource::TextColumn,
            service: Some("iMessage".into()),
            reply_to_guid: None,
            edited_at: None,
            retracted_at: None,
            is_tapback: false,
            associated_type: None,
            is_system_event: false,
        }
    }

    #[test]
    fn content_hash_is_stable() {
        assert_eq!(sample().content_hash(), sample().content_hash());
    }

    #[test]
    fn content_hash_is_hex_sha256() {
        let hash = sample().content_hash();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_changes_when_text_changes() {
        let mut edited = sample();
        edited.text = Some("Hello!".into());
        assert_ne!(sample().content_hash(), edited.content_hash());
    }

    #[test]
    fn content_hash_changes_when_edited_at_changes() {
        let mut edited = sample();
        edited.edited_at = Some(Utc.with_ymd_and_hms(2026, 7, 1, 9, 20, 0).unwrap());
        assert_ne!(sample().content_hash(), edited.content_hash());
    }

    #[test]
    fn content_hash_changes_when_retracted() {
        let mut retracted = sample();
        retracted.retracted_at = Some(Utc.with_ymd_and_hms(2026, 7, 2, 0, 0, 0).unwrap());
        assert_ne!(sample().content_hash(), retracted.content_hash());
    }

    #[test]
    fn content_hash_distinguishes_none_text_from_empty_text() {
        // None and Some("") hash identically by design (both mean "no
        // content"), but a different guid must always differ.
        let mut other = sample();
        other.guid = "guid-2".into();
        assert_ne!(sample().content_hash(), other.content_hash());
    }

    #[test]
    fn field_boundaries_cannot_collide() {
        // ("ab", "c") must not hash equal to ("a", "bc").
        let mut a = sample();
        a.guid = "ab".into();
        a.text = Some("c".into());
        let mut b = sample();
        b.guid = "a".into();
        b.text = Some("bc".into());
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn group_chat_detection() {
        let mut m = sample();
        assert_eq!(m.is_group_chat(), Some(false));
        m.chat_style = Some(CHAT_STYLE_GROUP);
        assert_eq!(m.is_group_chat(), Some(true));
        m.chat_style = None;
        assert_eq!(m.is_group_chat(), None);
        m.chat_style = Some(99);
        assert_eq!(m.is_group_chat(), None);
    }
}
