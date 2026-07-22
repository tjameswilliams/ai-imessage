//! End-to-end tests running the actual `ai-imessage` binary.

mod common;

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};
use predicates::prelude::*;

/// Write a config file pointing the CLI at a fixture database and a
/// temp destination, so tests never touch the real user environment.
fn write_config(fixture_db: &Path, dir: &Path) -> PathBuf {
    let config_path = dir.join("config.toml");
    let index_path = dir.join("index/index.sqlite");
    std::fs::write(
        &config_path,
        format!(
            "[source]\ndatabase_path = \"{}\"\n\n[index]\ndatabase_path = \"{}\"\n",
            fixture_db.display(),
            index_path.display()
        ),
    )
    .unwrap();
    config_path
}

fn cmd() -> Command {
    Command::cargo_bin("ai-imessage").unwrap()
}

fn populated_fixture() -> Fixture {
    let f = Fixture::new(SchemaVariant::Modern);
    let alice = f.add_handle("+15550100001");
    let chat = f.add_chat("direct-chat", ai_imessage::model::CHAT_STYLE_DIRECT, None);
    let m1 = f.add_message(&MessageSpec {
        guid: "cli-1",
        text: Some("SECRET BODY ONE"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:14:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m1);
    let m2 = f.add_message(&MessageSpec {
        guid: "cli-2",
        attributed_text: Some("SECRET BODY TWO"),
        is_from_me: true,
        date: apple_ns("2026-07-02T10:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m2);
    f
}

#[test]
fn dry_run_reports_counts_without_message_bodies() {
    let f = populated_fixture();
    let config = write_config(&f.db_path, f.dir.path());

    cmd()
        .args(["--config", config.to_str().unwrap(), "etl", "--dry-run"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Total readable:             2")
                .and(predicate::str::contains("With plain text:            1"))
                .and(predicate::str::contains("Recovered from typedstream: 1"))
                .and(predicate::str::contains("Direct: 1"))
                .and(predicate::str::contains("2026-07-01T09:14:00Z"))
                .and(predicate::str::contains("2026-07-02T10:00:00Z"))
                // Privacy: bodies must never appear without the debug flag.
                .and(predicate::str::contains("SECRET BODY").not()),
        );
}

#[test]
fn dry_run_debug_flag_shows_bodies_with_warning() {
    let f = populated_fixture();
    let config = write_config(&f.db_path, f.dir.path());

    cmd()
        .args([
            "--config",
            config.to_str().unwrap(),
            "etl",
            "--dry-run",
            "--debug-show-text",
            "5",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("SECRET BODY ONE")
                .and(predicate::str::contains("SECRET BODY TWO")),
        )
        .stderr(predicate::str::contains("private message content"));
}

#[test]
fn etl_ingests_into_the_index_without_printing_bodies() {
    let f = populated_fixture();
    let config = write_config(&f.db_path, f.dir.path());

    cmd()
        .args(["--config", config.to_str().unwrap(), "etl"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("initial full sync")
                .and(predicate::str::contains("Inserted:  2"))
                .and(predicate::str::contains("Messages:  2"))
                .and(predicate::str::contains("SECRET BODY").not()),
        );

    // A second run is incremental and changes nothing.
    cmd()
        .args(["--config", config.to_str().unwrap(), "etl"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("incremental")
                .and(predicate::str::contains("Inserted:  0"))
                .and(predicate::str::contains("Unchanged: 2")),
        );

    // --rebuild starts over from an empty index.
    cmd()
        .args(["--config", config.to_str().unwrap(), "etl", "--rebuild"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("initial full sync")
                .and(predicate::str::contains("Inserted:  2")),
        );
}

#[test]
fn etl_rebuild_conflicts_with_dry_run() {
    cmd()
        .args(["etl", "--dry-run", "--rebuild"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn dry_run_against_missing_database_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(&dir.path().join("no-such.db"), dir.path());

    cmd()
        .args(["--config", config.to_str().unwrap(), "etl", "--dry-run"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no Messages database found")
                .and(predicate::str::contains("doctor")),
        );
}

#[test]
fn doctor_succeeds_against_fixture() {
    let f = populated_fixture();
    let config = write_config(&f.db_path, f.dir.path());

    cmd()
        .args(["--config", config.to_str().unwrap(), "doctor"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("source database present")
                .and(predicate::str::contains("source database readable"))
                .and(predicate::str::contains("destination directory writable"))
                .and(predicate::str::contains("0 failed")),
        );
}

#[test]
fn doctor_fails_with_nonzero_exit_when_source_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(&dir.path().join("no-such.db"), dir.path());

    cmd()
        .args(["--config", config.to_str().unwrap(), "doctor"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("FAIL"));
}

#[test]
fn config_show_redacts_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[embeddings]\napi_key = \"super-secret-key\"\n",
    )
    .unwrap();

    cmd()
        .args(["--config", config_path.to_str().unwrap(), "config", "show"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("<redacted>")
                .and(predicate::str::contains("super-secret-key").not()),
        );
}

#[test]
fn config_show_reports_defaults_when_no_file_exists() {
    // No --config flag: the default path almost certainly has no file in CI,
    // but this test must not depend on the developer's real config either,
    // so only assert on structure common to both cases.
    cmd().args(["config", "show"]).assert().success().stdout(
        predicate::str::contains("# config file:")
            .and(predicate::str::contains("[source]"))
            .and(predicate::str::contains("[privacy]")),
    );
}

#[test]
fn config_path_prints_default_location() {
    cmd()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ai-imessage/config.toml"));
}

#[test]
fn invalid_config_file_fails_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[index]\nchunk_gap_minutess = 60\n").unwrap();

    cmd()
        .args(["--config", config_path.to_str().unwrap(), "doctor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config"));
}

#[test]
fn nonexistent_config_flag_fails() {
    cmd()
        .args(["--config", "/definitely/not/here.toml", "doctor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not read config file"));
}

#[test]
fn version_flag_works() {
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("ai-imessage"));
}

#[test]
fn help_lists_subcommands() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("doctor")
            .and(predicate::str::contains("etl"))
            .and(predicate::str::contains("config")),
    );
}
