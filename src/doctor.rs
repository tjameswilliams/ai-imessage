//! `ai-imessage doctor`: diagnose everything that can stand between the
//! user and a working index, with actionable resolutions.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use rusqlite::Connection;

use crate::config::LoadedConfig;
use crate::extract::{SourceDb, SourceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub resolution: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            status: CheckStatus::Pass,
            detail: detail.into(),
            resolution: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, resolution: impl Into<String>) -> Self {
        Check {
            name,
            status: CheckStatus::Warn,
            detail: detail.into(),
            resolution: Some(resolution.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, resolution: impl Into<String>) -> Self {
        Check {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
            resolution: Some(resolution.into()),
        }
    }
}

pub const FDA_HELP: &str = "Grant Full Disk Access:\n\
  System Settings → Privacy & Security → Full Disk Access\n\
For interactive use, add the app you run this command from (Terminal, iTerm, …).\n\
The scheduled background service will need the ai-imessage binary itself added.\n\
Access is inherited from the launching app, so a check passing in your terminal\n\
does not guarantee the background service has access.\n\
After changing the setting, re-run: ai-imessage doctor";

/// Run every diagnostic check. Order matters: results read top to bottom as
/// a setup narrative.
pub fn run_checks(loaded: &LoadedConfig) -> Vec<Check> {
    let mut checks = vec![check_platform(), check_config(loaded)];

    match loaded.config.source_db_path() {
        Ok(source) => {
            let present = check_source_present(&source);
            let can_read = present.status != CheckStatus::Fail;
            checks.push(present);
            if can_read {
                checks.push(check_source_readable(&source));
            }
        }
        Err(e) => checks.push(Check::fail(
            "source path",
            format!("could not resolve source database path: {e}"),
            "Fix [source].database_path in the config file.",
        )),
    }

    match loaded.config.index_dir() {
        Ok(dir) => checks.push(check_destination_writable(&dir)),
        Err(e) => checks.push(Check::fail(
            "destination directory",
            format!("could not resolve index path: {e}"),
            "Fix [index].database_path in the config file.",
        )),
    }

    checks.push(check_contacts(loaded));
    checks.push(check_fts5());
    checks
}

/// Contact names are enrichment, so problems here warn instead of fail.
pub fn check_contacts(loaded: &LoadedConfig) -> Check {
    const NAME: &str = "contacts database";
    match loaded.config.contacts_path() {
        Ok(None) => Check::pass(NAME, "contact-name resolution disabled by config"),
        Ok(Some(path)) => match crate::contacts::ContactBook::load(&path) {
            Ok(book) => Check::pass(
                NAME,
                format!(
                    "{} names loaded from {} database(s)",
                    book.names(),
                    book.databases()
                ),
            ),
            Err(e) => Check::warn(
                NAME,
                e.to_string(),
                "Messages will be labeled by phone number / email instead of \
                 contact names. If the path looks right, this is usually a \
                 missing Full Disk Access grant.",
            ),
        },
        Err(e) => Check::warn(
            NAME,
            format!("could not resolve contacts path: {e}"),
            "Fix [source].contacts_path in the config file, or set it to \"\" \
             to disable contact-name resolution.",
        ),
    }
}

pub fn check_platform() -> Check {
    if cfg!(target_os = "macos") {
        Check::pass("platform", "macOS")
    } else {
        Check::warn(
            "platform",
            format!("not macOS ({})", std::env::consts::OS),
            "ai-imessage reads the Apple Messages database, which only exists on macOS. \
             Non-macOS use is only meaningful for development.",
        )
    }
}

pub fn check_config(loaded: &LoadedConfig) -> Check {
    if loaded.from_file {
        Check::pass(
            "configuration",
            format!("loaded from {}", loaded.path.display()),
        )
    } else {
        Check::pass(
            "configuration",
            format!(
                "no file at {} — built-in defaults in use",
                loaded.path.display()
            ),
        )
    }
}

pub fn check_source_present(path: &Path) -> Check {
    const NAME: &str = "source database present";
    match fs::metadata(path) {
        Ok(md) => Check::pass(
            NAME,
            format!("{} ({:.1} MB)", path.display(), md.len() as f64 / 1e6),
        ),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Check::fail(
            NAME,
            format!(
                "permission denied for {} — Full Disk Access is missing",
                path.display()
            ),
            FDA_HELP,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Check::fail(
            NAME,
            format!("nothing at {}", path.display()),
            "If Messages has ever been used on this Mac the database should exist. \
             Check [source].database_path in the config file. Note that a missing \
             Full Disk Access grant can also make the file appear absent.",
        ),
        Err(e) => Check::fail(
            NAME,
            format!("could not inspect {}: {e}", path.display()),
            "Check the path and filesystem permissions.",
        ),
    }
}

pub fn check_source_readable(path: &Path) -> Check {
    const NAME: &str = "source database readable";
    match SourceDb::open(path) {
        Ok(db) => match db.message_count() {
            Ok(n) => Check::pass(
                NAME,
                format!(
                    "opened read-only; {n} messages visible; columns: {}",
                    db.caps().summary()
                ),
            ),
            Err(e) => Check::fail(
                NAME,
                format!("opened, but counting messages failed: {e}"),
                "The database may be corrupt or mid-migration. Try again; if it \
                 persists, check Messages.app itself.",
            ),
        },
        Err(SourceError::PermissionDenied(_)) => Check::fail(
            NAME,
            "SQLite could not open the database: permission denied",
            FDA_HELP,
        ),
        Err(e) => Check::fail(
            NAME,
            e.to_string(),
            "Run with --verbose for more detail. If the file is not a Messages \
             database, fix [source].database_path.",
        ),
    }
}

pub fn check_destination_writable(dir: &Path) -> Check {
    const NAME: &str = "destination directory writable";
    if let Err(e) = fs::create_dir_all(dir) {
        return Check::fail(
            NAME,
            format!("could not create {}: {e}", dir.display()),
            "Check [index].database_path and directory permissions.",
        );
    }
    let probe = dir.join(".write-probe");
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Check::pass(NAME, dir.display().to_string())
        }
        Err(e) => Check::fail(
            NAME,
            format!("cannot write inside {}: {e}", dir.display()),
            "Check directory permissions, or point [index].database_path elsewhere.",
        ),
    }
}

pub fn check_fts5() -> Check {
    const NAME: &str = "SQLite FTS5 support";
    let probe = Connection::open_in_memory()
        .and_then(|conn| conn.execute_batch("CREATE VIRTUAL TABLE fts_probe USING fts5(content);"));
    match probe {
        Ok(()) => Check::pass(NAME, "available in bundled SQLite"),
        Err(e) => Check::warn(
            NAME,
            format!("FTS5 unavailable: {e}"),
            "Full-text search (Milestone 3) requires FTS5. This build's bundled \
             SQLite lacks it — please report this as a packaging bug.",
        ),
    }
}

pub fn has_failure(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == CheckStatus::Fail)
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!("{}  {} — {}\n", c.status, c.name, c.detail));
        if let Some(res) = &c.resolution {
            for line in res.lines() {
                out.push_str(&format!("        {line}\n"));
            }
        }
    }
    let pass = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Pass)
        .count();
    let warn = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warn)
        .count();
    let fail = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Fail)
        .count();
    out.push_str(&format!(
        "\n{pass} passed, {warn} warnings, {fail} failed\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> Check {
        Check::pass("example", "fine")
    }

    #[test]
    fn render_includes_status_name_and_detail() {
        let out = render(&[passing()]);
        assert!(out.contains("PASS  example — fine"));
        assert!(out.contains("1 passed, 0 warnings, 0 failed"));
    }

    #[test]
    fn render_indents_resolution_lines() {
        let out = render(&[Check::fail("thing", "broke", "line one\nline two")]);
        assert!(out.contains("FAIL  thing — broke"));
        assert!(out.contains("        line one"));
        assert!(out.contains("        line two"));
    }

    #[test]
    fn has_failure_only_on_fail() {
        assert!(!has_failure(&[passing()]));
        assert!(!has_failure(&[Check::warn("w", "d", "r")]));
        assert!(has_failure(&[passing(), Check::fail("f", "d", "r")]));
    }

    #[test]
    fn fts5_is_available_in_bundled_sqlite() {
        // The whole search architecture (Milestone 3) depends on this.
        let check = check_fts5();
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    }

    #[test]
    fn fda_help_names_the_settings_path() {
        assert!(FDA_HELP.contains("System Settings"));
        assert!(FDA_HELP.contains("Full Disk Access"));
        assert!(FDA_HELP.contains("doctor"));
    }

    #[test]
    fn status_display_matches_expected_tokens() {
        assert_eq!(CheckStatus::Pass.to_string(), "PASS");
        assert_eq!(CheckStatus::Warn.to_string(), "WARN");
        assert_eq!(CheckStatus::Fail.to_string(), "FAIL");
    }
}
