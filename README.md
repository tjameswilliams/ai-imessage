# ai-imessage

Local-first Apple Messages RAG for AI agents. Indexes your Messages history
into a private local database and exposes read-only search to MCP clients
like Claude Code and Claude Desktop.

**Everything stays on your machine.** The Apple Messages database is only
ever opened read-only; the index, embeddings, and search never leave your
Mac unless you explicitly configure a remote embedding endpoint (off by
default).

## Status

Early development. Milestone 6 of 8 (MCP server) is complete:

- [x] **M1** Read-only extraction, typedstream decoding, `doctor`, `etl --dry-run`
- [x] **M2** Normalized destination database, incremental ETL
- [x] **M3** Conversation chunking + FTS5 keyword search
- [x] **M4** Local embeddings + vector search
- [x] **M5** Hybrid retrieval (rank fusion)
- [x] **M6** MCP server
- [ ] **M7** Scheduled ETL (LaunchAgent)
- [ ] **M8** Homebrew release

## Quick start (from source)

```bash
cargo build --release
./target/release/ai-imessage doctor          # diagnose access & permissions
./target/release/ai-imessage etl --dry-run   # count what's readable, write nothing
./target/release/ai-imessage etl             # sync messages into the local index
./target/release/ai-imessage search pizza    # keyword search over your history
./target/release/ai-imessage search --semantic "plans for the weekend"
```

`doctor` will walk you through granting Full Disk Access, which macOS
requires for any app reading `~/Library/Messages/chat.db`.

## Commands

| Command | Purpose |
| --- | --- |
| `ai-imessage doctor` | Check platform, permissions, config, and SQLite features |
| `ai-imessage etl` | Incremental sync into the local index (first run ingests everything; later runs rescan only the recent tail to catch edits/retractions). `--rebuild` starts over |
| `ai-imessage etl --dry-run` | Read-only scan: message/chat counts, time range. No bodies printed unless `--debug-show-text N` is passed explicitly |
| `ai-imessage search <terms>` | Hybrid search (keyword + semantic, fused by reciprocal rank) over conversation chunks; prints matching snippets (`--limit N`). Falls back to keyword-only when no embeddings exist |
| `ai-imessage search --keyword <terms>` | FTS5 keyword match only |
| `ai-imessage search --semantic <terms>` | Embedding similarity only |
| `ai-imessage serve` | MCP server over stdio for Claude Code / Claude Desktop |
| `ai-imessage config show` | Print effective config (secrets redacted) |
| `ai-imessage config path` | Print config file location |

Configuration lives at `~/Library/Application Support/ai-imessage/config.toml`
(TOML, all keys optional — no file is needed at all). See `config show` for
the full schema and defaults.

## Using from MCP clients

`ai-imessage serve` speaks MCP over stdio, so any MCP client can use it.
Run `ai-imessage etl` once first so there is an index to serve, and
replace `/path/to/ai-imessage` below with your binary's absolute path
(e.g. `$(pwd)/target/release/ai-imessage` from a source build).

Three read-only tools are exposed: `search_messages` (hybrid keyword +
semantic retrieval), `get_conversation` (a hit expanded with surrounding
messages), and `list_chats`. The server never writes and only ever sees
the local index.

### Claude Code

```bash
claude mcp add imessage -- /path/to/ai-imessage serve
```

### Claude Desktop

Add to `claude_desktop_config.json` (Settings → Developer → Edit Config):

```json
{ "mcpServers": { "imessage": { "command": "/path/to/ai-imessage", "args": ["serve"] } } }
```

### LM Studio

Program tab (right sidebar) → Install → Edit `mcp.json` — or edit
`~/.lmstudio/mcp.json` directly:

```json
{ "mcpServers": { "imessage": { "command": "/path/to/ai-imessage", "args": ["serve"] } } }
```

### Codex

```bash
codex mcp add imessage -- /path/to/ai-imessage serve
```

or in `~/.codex/config.toml` (the section must be spelled `mcp_servers`;
other spellings are silently ignored):

```toml
[mcp_servers.imessage]
command = "/path/to/ai-imessage"
args = ["serve"]
```

### OpenCode

In `opencode.json` (project) or `~/.config/opencode/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "imessage": {
      "type": "local",
      "command": ["/path/to/ai-imessage", "serve"],
      "enabled": true
    }
  }
}
```

### Hermes

In `~/.hermes/config.yaml`, then `/reload-mcp` in a running session:

```yaml
mcp_servers:
  imessage:
    command: "/path/to/ai-imessage"
    args: ["serve"]
```

### OpenClaw

```bash
openclaw mcp add imessage --command /path/to/ai-imessage --arg serve
openclaw mcp doctor imessage --probe   # verify
```

or in `~/.openclaw/openclaw.json`:

```json
{ "mcp": { "servers": { "imessage": { "command": "/path/to/ai-imessage", "args": ["serve"] } } } }
```

## Privacy

- Source database opened with `SQLITE_OPEN_READONLY` + `PRAGMA query_only`, enforced by tests.
- Contact names are resolved from the local macOS Contacts store (read-only)
  so search results say "Alice Smith" instead of "+1 916…". Names never
  leave the machine; set `[source].contacts_path = ""` to disable.
- The local index (`~/Library/Application Support/ai-imessage/index.sqlite`) contains full message bodies and is created with owner-only permissions (0600, directory 0700).
- No telemetry. The only network access in the default configuration is a
  one-time download of the embedding model weights (public model, no user
  data sent); pass `etl --no-embed` to avoid even that.
- Embedding runs locally via ONNX. An `openai-compatible` endpoint may be
  configured instead; non-loopback endpoints are refused unless
  `privacy.allow_remote_embedding_endpoint = true` is set explicitly.
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
