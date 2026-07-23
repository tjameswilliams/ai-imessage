# ai-imessage

![ai-imessage — local-first Apple Messages search for AI agents](assets/header.jpg)

**Want an AI assistant that actually knows your text messages — without
shipping a decade of your most private conversations to OpenAI?**
Here's the way.

Your Messages history is the most personal dataset you own: family,
money, health, plans, every relationship you have. It's also exactly
what makes an AI assistant genuinely useful:

> *"When did I last talk to Jaina, and about what?"*
> *"What did Melissa and I decide about the trip?"*
> *"Catch me up on the group chat — what am I on the hook for?"*

The usual price for that is uploading everything to someone else's
cloud. ai-imessage refuses the trade. It indexes your Apple Messages
history into a private database **on your Mac** and serves read-only
search tools to any AI you point at it over MCP — including a model
running entirely on the same machine:

- **Fully local, end to end.** Pair it with LM Studio (or any local
  model) and the entire loop — indexing, embeddings, retrieval, and the
  AI itself — runs on hardware you own. No OpenAI. No Anthropic. No
  cloud. Verifiably: there is no telemetry, and the only network access
  in the default configuration is a one-time public model download that
  contains none of your data.
- **Read-only by construction.** The Messages database is opened
  read-only with a second enforcement layer at the SQL level, backed by
  tests. The index is owner-only on disk; the search server can never
  write and requires a bearer token on every HTTP request.
- **Your phone, your network.** Take it mobile over your own private
  tailnet with TLS — not through anyone's relay.
- **Cloud AI only if — and when — you choose it.** The same tools plug
  into Claude Desktop, Claude Code, Codex, and friends. That's a
  decision you make per client, not a default made for you.

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

## The happy path

The tested route from zero to "my AI can search my messages — even from
my phone." (Deeper background and troubleshooting:
[the recommended path](docs/recommended-path.md).)

### Step 1 — Install and build your index

```bash
brew install tjameswilliams/tap/ai-imessage
ai-imessage doctor
```

`doctor` will fail until your **terminal app** has Full Disk Access
(System Settings → Privacy & Security → Full Disk Access) — macOS
requires it for anything that reads `~/Library/Messages/chat.db`. When
all checks pass:

```bash
ai-imessage etl                      # first sync: full history + local embeddings
ai-imessage search "dinner plans"    # try it
```

### Step 2 — Keep it synced and serve MCP

```bash
ai-imessage service install --http
```

This installs two background agents: a sync every 5 minutes and a
persistent MCP server on `127.0.0.1:8787` (loopback only). Then grant
Full Disk Access to the **binary itself** — launchd runs it directly, so
your terminal's grant doesn't apply:

> System Settings → Privacy & Security → Full Disk Access → add
> `/opt/homebrew/opt/ai-imessage/bin/ai-imessage`

Confirm with `ai-imessage service status` (a sync report should replace
any permission error within 5 minutes). Signed releases keep this grant
across upgrades.

### Step 3 — Connect LM Studio (desktop)

```bash
ai-imessage connect
```

Copy the **stdio** JSON block it prints into LM Studio's `mcp.json`
(Program tab in the right sidebar → Install → Edit `mcp.json`, or edit
`~/.lmstudio/mcp.json` directly). Toggle the server on and the four
tools appear for any loaded model.

### Step 4 — Connect Open WebUI

Open WebUI (≥ 0.6.31) speaks MCP over streamable HTTP. In **Admin
Settings → External Tools → Add server (MCP)**:

- URL: `http://127.0.0.1:8787/mcp` — or, if Open WebUI runs in Docker,
  `http://host.docker.internal:8787/mcp` (loopback inside a container is
  the container, not your Mac)
- Auth: Bearer token from `ai-imessage connect --token-only`

### Step 5 — Secure mobile access (Tailscale + Cumbersome)

1. Install [Tailscale](https://tailscale.com) on the Mac and your phone,
   signed into the same tailnet.
2. Publish the MCP server to your tailnet with TLS (loopback stays the
   only real listener):

   ```bash
   tailscale serve --bg --https=8443 http://127.0.0.1:8787
   ```

3. For on-Mac inference, do the same for LM Studio's API server, then
   disable LM Studio's "Serve on Local Network" so port 1234 is
   loopback-only:

   ```bash
   tailscale serve --bg --https=8444 http://127.0.0.1:1234
   ```

4. Run `ai-imessage connect` again — it detects the proxy and prints a
   tailnet JSON block (`https://<your-mac>.<tailnet>.ts.net:8443/mcp`
   with the auth header filled in). Paste it into Cumbersome's remote
   MCP settings (Cumbersome ≥ 1.56).
5. In Cumbersome, add the inference endpoint as a custom
   OpenAI-compatible provider:
   `https://<your-mac>.<tailnet>.ts.net:8444/v1` with your **LM Studio
   API token** (from LM Studio's developer settings).

Mind the two different tokens: the MCP bearer token
(`connect --token-only`) authenticates the tools; the LM Studio API
token authenticates inference. HTTPS via `tailscale serve` is not
optional on iOS — App Transport Security rejects plain-http endpoints
with a TLS error.

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
ever touches a real `chat.db`.

Releases are signed and notarized — the full cycle is documented in
[docs/releasing.md](docs/releasing.md). `scripts/release.sh <version>`
builds, signs with a Developer ID identity (hardened runtime +
timestamp), optionally notarizes, and uploads the artifact. If you build from source and use the
background agents, re-sign your binary with any stable identity after
each rebuild (`codesign -f -s "<identity>" ~/.cargo/bin/ai-imessage`) so
its Full Disk Access grant survives rebuilds. The typedstream parser is a clean-room
implementation; do not copy code from GPL-licensed iMessage tooling into
this repository.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
