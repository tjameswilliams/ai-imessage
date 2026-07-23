# ai-imessage

![ai-imessage — local-first Apple Messages search for AI agents](assets/header.jpg)

Local-first Apple Messages RAG for AI agents. Indexes your Messages history
into a private local database and exposes read-only search to MCP clients
like Claude Code, Claude Desktop, and LM Studio.

**Everything stays on your machine.** The Apple Messages database is only
ever opened read-only; the index, embeddings, and search never leave your
Mac unless you explicitly configure a remote embedding endpoint (off by
default).

## Status

All eight planned milestones are complete:

- [x] **M1** Read-only extraction, typedstream decoding, `doctor`, `etl --dry-run`
- [x] **M2** Normalized destination database, incremental ETL
- [x] **M3** Conversation chunking + FTS5 keyword search
- [x] **M4** Local embeddings + vector search
- [x] **M5** Hybrid retrieval (rank fusion)
- [x] **M6** MCP server
- [x] **M7** Scheduled ETL (LaunchAgent)
- [x] **M8** Homebrew release

## Install

```bash
brew install tjameswilliams/tap/ai-imessage
```

or from source: `cargo install --path .` (puts the binary in `~/.cargo/bin`).

## Quick start

```bash
ai-imessage doctor          # diagnose access & permissions
ai-imessage etl --dry-run   # count what's readable, write nothing
ai-imessage etl             # sync messages into the local index
ai-imessage search pizza    # keyword search over your history
ai-imessage search --semantic "plans for the weekend"
```

`doctor` will walk you through granting Full Disk Access, which macOS
requires for any app reading `~/Library/Messages/chat.db`.

To keep the index fresh automatically, install the background agent:

```bash
./target/release/ai-imessage service install
```

macOS attributes Full Disk Access to whatever launchd runs, so the
**binary itself** must be added under System Settings → Privacy &
Security → Full Disk Access (the install command prints the exact path).
`service status` shows the agent state and its recent log — sync reports
only, never message content.

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
| `ai-imessage serve --http ADDR` | MCP over streamable HTTP with bearer-token auth (for Open WebUI, remote/mobile MCP clients) |
| `ai-imessage service install` | Install a launchd agent that runs `etl` every `service.interval_seconds` (default 300). `--no-load` writes the plist without loading |
| `ai-imessage service install --http [ADDR]` | Opt-in: ALSO keep the MCP HTTP server running persistently (default `127.0.0.1:8787`). Opt back out with `service uninstall --http-only` |
| `ai-imessage service start` / `stop` | Pause and resume installed agents without uninstalling (`--http-only` scopes to the HTTP server) |
| `ai-imessage service status` | Agent state and the tail of its log |
| `ai-imessage service uninstall` | Unload the agent and remove its plist |
| `ai-imessage connect` | Ready-to-paste MCP client JSON for stdio and HTTP, plus the bearer token (`--token-only` for scripting). Detects a running Tailscale (installed CLI + `tailscale serve` proxy or tailnet bind) and prints the tailnet JSON too |
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

The fastest path: `ai-imessage connect` prints ready-to-paste JSON with
the real paths, URL, and bearer token filled in for both transports.

Four read-only tools are exposed:

- `search_messages` — hybrid keyword + semantic retrieval by topic
- `get_recent_messages` — the chronological tail: when someone was last
  talked to and what was said, scoped to a contact (their messages plus
  yours in direct chats with them), a chat, or everything
- `get_conversation` — a search hit expanded with surrounding messages
- `list_chats` — chats by recency, with contact-name labels

The server never writes and only ever sees the local index.

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

### HTTP clients (Open WebUI, remote/mobile)

Clients that speak MCP streamable HTTP (Open WebUI ≥ 0.6.31 under Admin
Settings → External Tools, remote-connector mobile apps, …) connect to:

```bash
ai-imessage serve --http 127.0.0.1:8787
# endpoint: http://127.0.0.1:8787/mcp
# auth:     Authorization: Bearer <token>
```

Every request must present the bearer token — there is no unauthenticated
mode. Set it via `[service].http_token` in the config, or let the server
generate one on first run (stored owner-only next to the index, path
printed at startup). Bind loopback or a private tailnet address only: the
server exposes your entire message history to anyone holding the token.

To keep the HTTP server running permanently, opt in when installing the
background service (nothing listens unless you ask):

```bash
ai-imessage service install --http            # loopback, 127.0.0.1:8787
ai-imessage connect                           # client JSON + bearer token
ai-imessage service stop --http-only          # pause (start resumes)
ai-imessage service uninstall --http-only     # opt back out entirely
```

#### Example: phone access over Tailscale

Keep the server on loopback and let `tailscale serve` add tailnet-only
exposure with TLS (Tailscale is one deployment option, not a dependency):

```bash
ai-imessage service install --http
tailscale serve --bg --https=8443 http://127.0.0.1:8787
ai-imessage connect   # detects the proxy, prints the tailnet JSON ready to paste
```

Mobile MCP clients (e.g. Cumbersome ≥ 1.56 with its remote MCP support)
can then call the tools directly, pairing with any model backend — such
as LM Studio's OpenAI-compatible server on the same Mac.

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
- API keys and the HTTP bearer token are redacted from `config show` output.

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
