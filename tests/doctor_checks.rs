//! Integration tests for doctor checks against real filesystem conditions.

mod common;

use std::path::Path;

use ai_imessage::doctor::{
    CheckStatus, check_destination_writable, check_source_present, check_source_readable,
};
use common::{Fixture, SchemaVariant};

#[test]
fn present_check_passes_on_real_fixture() {
    let f = Fixture::new(SchemaVariant::Modern);
    let check = check_source_present(&f.db_path);
    assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    assert!(check.detail.contains("MB"));
}

#[test]
fn present_check_fails_on_missing_file_with_guidance() {
    let check = check_source_present(Path::new("/nonexistent/dir/chat.db"));
    assert_eq!(check.status, CheckStatus::Fail);
    let resolution = check.resolution.expect("failures carry a resolution");
    assert!(resolution.contains("database_path"));
}

#[cfg(unix)]
#[test]
fn present_check_reports_full_disk_access_on_permission_denied() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("chat.db"), b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let check = check_source_present(&locked.join("chat.db"));

    // Restore so TempDir can clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(check.status, CheckStatus::Fail);
    assert!(check.detail.contains("Full Disk Access"));
    let resolution = check.resolution.unwrap();
    assert!(resolution.contains("System Settings"));
    assert!(resolution.contains("Privacy & Security"));
}

#[test]
fn readable_check_passes_and_reports_count() {
    let f = Fixture::new(SchemaVariant::Modern);
    f.add_message(&common::MessageSpec {
        guid: "d1",
        text: Some("hello"),
        ..Default::default()
    });
    let check = check_source_readable(&f.db_path);
    assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    assert!(check.detail.contains("read-only"));
    assert!(check.detail.contains("1 messages visible"));
}

#[test]
fn readable_check_fails_on_non_messages_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("other.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (x INTEGER);")
        .unwrap();
    let check = check_source_readable(&db);
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(check.detail.contains("missing tables"));
}

#[test]
fn readable_check_fails_on_corrupt_file() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.db");
    std::fs::write(&junk, b"not a sqlite database, not even close.......").unwrap();
    let check = check_source_readable(&junk);
    assert_eq!(check.status, CheckStatus::Fail);
}

#[test]
fn destination_check_creates_and_passes() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("nested/app-dir");
    let check = check_destination_writable(&dest);
    assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    assert!(dest.is_dir(), "check must create the directory");
    assert!(
        !dest.join(".write-probe").exists(),
        "probe file must be cleaned up"
    );
}

#[cfg(unix)]
#[test]
fn destination_check_fails_when_unwritable() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let check = check_destination_writable(&locked.join("sub"));

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(check.status, CheckStatus::Fail);
}
