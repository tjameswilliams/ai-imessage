//! The `etl --dry-run` report: read and count, write nothing.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::extract::{SourceDb, SourceError};
use crate::model::TextSource;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct DryRunReport {
    pub source_path: String,
    pub total_messages: u64,
    pub with_text_column: u64,
    pub recovered_from_typedstream: u64,
    pub without_text: u64,
    /// Subset of `without_text` that are tapbacks/reactions.
    pub tapbacks_without_text: u64,
    /// Group-membership changes and similar non-message items.
    pub system_events: u64,
    pub direct_chats: u64,
    pub group_chats: u64,
    pub other_chats: u64,
    pub earliest: Option<DateTime<Utc>>,
    pub latest: Option<DateTime<Utc>>,
}

/// Scan the whole source database and produce counts. Message bodies are
/// never stored in the report.
pub fn build_report(db: &SourceDb) -> Result<DryRunReport, SourceError> {
    let mut r = DryRunReport {
        source_path: db.path().display().to_string(),
        ..DryRunReport::default()
    };

    db.scan_messages(0, |m| {
        r.total_messages += 1;
        match m.text_source {
            TextSource::TextColumn => r.with_text_column += 1,
            TextSource::AttributedBody => r.recovered_from_typedstream += 1,
            TextSource::None => {
                r.without_text += 1;
                if m.is_tapback {
                    r.tapbacks_without_text += 1;
                }
            }
        }
        if m.is_system_event {
            r.system_events += 1;
        }
        if let Some(ts) = m.sent_at {
            r.earliest = Some(r.earliest.map_or(ts, |e| e.min(ts)));
            r.latest = Some(r.latest.map_or(ts, |l| l.max(ts)));
        }
    })?;

    let chats = db.chat_stats()?;
    r.direct_chats = chats.direct;
    r.group_chats = chats.group;
    r.other_chats = chats.total - chats.direct - chats.group;
    Ok(r)
}

/// First `limit` message texts, truncated, for explicit debugging only.
/// The caller is responsible for warning the user before printing these.
pub fn text_samples(db: &SourceDb, limit: usize) -> Result<Vec<String>, SourceError> {
    const MAX_CHARS: usize = 120;
    let mut out = Vec::new();
    db.scan_messages(0, |m| {
        if out.len() >= limit {
            return;
        }
        if let Some(t) = &m.text {
            let mut shown: String = t.chars().take(MAX_CHARS).collect();
            if t.chars().count() > MAX_CHARS {
                shown.push('…');
            }
            let sender = if m.is_from_me {
                "me"
            } else {
                m.sender_handle.as_deref().unwrap_or("unknown")
            };
            let when = m
                .sent_at
                .map(|d| d.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "unknown-time".into());
            out.push(format!("[{when}] {sender}: {shown}"));
        }
    })?;
    Ok(out)
}

fn fmt_ts(t: Option<DateTime<Utc>>) -> String {
    t.map(|d| d.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "n/a".into())
}

impl fmt::Display for DryRunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Source database: {} (opened read-only)",
            self.source_path
        )?;
        writeln!(f)?;
        writeln!(f, "Messages")?;
        writeln!(f, "  Total readable:             {}", self.total_messages)?;
        writeln!(f, "  With plain text:            {}", self.with_text_column)?;
        writeln!(
            f,
            "  Recovered from typedstream: {}",
            self.recovered_from_typedstream
        )?;
        writeln!(f, "  No text content:            {}", self.without_text)?;
        writeln!(
            f,
            "    of which tapbacks:        {}",
            self.tapbacks_without_text
        )?;
        writeln!(f, "  System events (non-chat):   {}", self.system_events)?;
        writeln!(f)?;
        writeln!(f, "Chats")?;
        writeln!(f, "  Direct: {}", self.direct_chats)?;
        writeln!(f, "  Group:  {}", self.group_chats)?;
        if self.other_chats > 0 {
            writeln!(f, "  Other:  {}", self.other_chats)?;
        }
        writeln!(f)?;
        writeln!(f, "Time range")?;
        writeln!(f, "  Earliest: {}", fmt_ts(self.earliest))?;
        write!(f, "  Latest:   {}", fmt_ts(self.latest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn display_shows_na_for_empty_time_range() {
        let r = DryRunReport::default();
        let text = r.to_string();
        assert!(text.contains("Earliest: n/a"));
        assert!(text.contains("Latest:   n/a"));
    }

    #[test]
    fn display_hides_other_chats_when_zero() {
        let mut r = DryRunReport::default();
        assert!(!r.to_string().contains("Other:"));
        r.other_chats = 2;
        assert!(r.to_string().contains("Other:  2"));
    }

    #[test]
    fn display_formats_timestamps_as_utc_rfc3339() {
        let r = DryRunReport {
            earliest: Some(Utc.with_ymd_and_hms(2014, 5, 1, 12, 0, 0).unwrap()),
            latest: Some(Utc.with_ymd_and_hms(2026, 7, 22, 9, 30, 0).unwrap()),
            ..DryRunReport::default()
        };
        let text = r.to_string();
        assert!(text.contains("2014-05-01T12:00:00Z"));
        assert!(text.contains("2026-07-22T09:30:00Z"));
    }

    #[test]
    fn report_stays_serializable_for_future_json_flag() {
        let r = DryRunReport {
            earliest: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            ..DryRunReport::default()
        };
        let serialized = toml::to_string(&r).unwrap();
        assert!(serialized.contains("total_messages"));
    }
}
