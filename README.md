# ai-imessage

Local-first Apple Messages RAG for AI agents. Indexes your Messages history
into a private local database and exposes read-only search to MCP clients
like Claude Code and Claude Desktop.

**Everything stays on your machine.** The Apple Messages database is only
ever opened read-only; the index, embeddings, and search never leave your
Mac unless you explicitly configure a remote embedding endpoint (off by
default).

## Status

Early development. Milestone 3 of 8 (keyword search) is complete:

- [x] **M1** Read-only extraction, typedstream decoding, `doctor`, `etl --dry-run`
- [x] **M2** Normalized destination database, incremental ETL
- [x] **M3** Conversation chunking + FTS5 keyword search
- [ ] **M4** Local embeddings + vector search
- [ ] **M5** Hybrid retrieval (rank fusion)
- [ ] **M6** MCP server
- [ ] **M7** Scheduled ETL (LaunchAgent)
- [ ] **M8** Homebrew release

## Quick start (from source)

```bash
cargo build --release
./target/release/ai-imessage doctor          # diagnose access & permissions
./target/release/ai-imessage etl --dry-run   # count what's readable, write nothing
./target/release/ai-imessage etl             # sync messages into the local index
./target/release/ai-imessage search pizza    # keyword search over your history
```

`doctor` will walk you through granting Full Disk Access, which macOS
requires for any app reading `~/Library/Messages/chat.db`.

## Commands

| Command | Purpose |
| --- | --- |
| `ai-imessage doctor` | Check platform, permissions, config, and SQLite features |
| `ai-imessage etl` | Incremental sync into the local index (first run ingests everything; later runs rescan only the recent tail to catch edits/retractions). `--rebuild` starts over |
| `ai-imessage etl --dry-run` | Read-only scan: message/chat counts, time range. No bodies printed unless `--debug-show-text N` is passed explicitly |
| `ai-imessage search <terms>` | FTS5 keyword search over conversation chunks; prints matching snippets (`--limit N`) |
| `ai-imessage config show` | Print effective config (secrets redacted) |
| `ai-imessage config path` | Print config file location |

Configuration lives at `~/Library/Application Support/ai-imessage/config.toml`
(TOML, all keys optional — no file is needed at all). See `config show` for
the full schema and defaults.

## Privacy

- Source database opened with `SQLITE_OPEN_READONLY` + `PRAGMA query_only`, enforced by tests.
- The local index (`~/Library/Application Support/ai-imessage/index.sqlite`) contains full message bodies and is created with owner-only permissions (0600, directory 0700).
- No telemetry, no network calls in the default configuration.
- Logs and reports never contain message content; `--debug-show-text` is the
  single, explicit, warned exception.
- API keys are redacted from `config show` output.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test
cargo llvm-cov --summary-only   # coverage (cargo install cargo-llvm-cov)
```

Tests run against synthetic Messages databases in `tests/common/` — no test
ever touches a real `chat.db`. The typedstream parser is a clean-room
implementation; do not copy code from GPL-licensed iMessage tooling into
this repository.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
