# The recommended path

The shortest route from a fresh Mac to "my AI agent can search my
Messages history — including from my phone." Every step and warning here
comes from a real deployment; the gotchas are ordered roughly by how
likely they are to bite.

## 1. Install and build your index

```bash
brew install tjameswilliams/tap/ai-imessage
ai-imessage doctor
```

`doctor` will fail its source-database checks until your **terminal app**
has Full Disk Access (System Settings → Privacy & Security → Full Disk
Access). That grant is unavoidable — macOS protects the Messages database
from every program — and `doctor` tells you exactly what is missing.
When it shows all green:

```bash
ai-imessage etl
```

The first run ingests your entire history, resolves contact names, and
computes embeddings locally (a one-time model download; expect several
minutes for a large history). Try it:

```bash
ai-imessage search "dinner plans"
```

## 2. Keep it fresh, serve it always

```bash
ai-imessage service install --http
```

This installs two launchd agents: a sync every 5 minutes, and a
persistent MCP server on `127.0.0.1:8787` (strictly loopback; nothing
is exposed to any network). Then the **second** Full Disk Access grant —
this one for the binary itself, because launchd runs it directly and
macOS attributes permissions to the program, not the terminal that
installed it:

> System Settings → Privacy & Security → Full Disk Access → add
> `/opt/homebrew/opt/ai-imessage/bin/ai-imessage`

Confirm with `ai-imessage service status`: within 5 minutes the log
should show a sync report instead of a permission error.

**The upgrade trap:** when the binary is replaced (upgrades, rebuilds),
macOS may silently invalidate its Full Disk Access grant. The symptom is
always the same — permission errors reappear in `service status` — and
the fix is toggling the binary's FDA entry off and on. Signed release
binaries avoid this entirely (the grant anchors to the signing identity,
which survives upgrades).

## 3. Connect your clients

```bash
ai-imessage connect
```

Everything is printed ready to paste: stdio JSON for desktop clients
(Claude Desktop, LM Studio, Codex, …), HTTP JSON with the bearer token
filled in, and — if you use Tailscale (below) — the tailnet URL too.
`ai-imessage connect --token-only` prints just the token for scripts.

## 4. Phone access (the Tailscale pattern)

The design rule: **servers stay on loopback; the tailnet provides reach
and TLS.** Install Tailscale on the Mac and your phone, then:

```bash
tailscale serve --bg --https=8443 http://127.0.0.1:8787
ai-imessage connect     # now prints the https://…ts.net:8443/mcp JSON
```

Paste that block into a mobile MCP client (e.g. Cumbersome ≥ 1.56).
Pair it with a model backend — if that's LM Studio on the same Mac, give
it the identical treatment:

```bash
tailscale serve --bg --https=8444 http://127.0.0.1:1234
```

and use `https://<your-mac>.<tailnet>.ts.net:8444/v1` as the
OpenAI-compatible endpoint. Once proxied, disable LM Studio's "Serve on
Local Network" so port 1234 is loopback-only.

**Why HTTPS proxies instead of binding tailnet IPs directly:** iOS App
Transport Security forces TLS — a plain-http endpoint fails in mobile
apps with an unhelpful TLS error — and a loopback-only server can't be
reached from your LAN even if the token leaks.

**Two different tokens.** Easy to mix up:

| Connection | Token | Where to get it |
| --- | --- | --- |
| MCP tools (`:8443/mcp`) | ai-imessage bearer token | `ai-imessage connect --token-only` |
| LM Studio inference (`:8444/v1`) | LM Studio API token | LM Studio → Developer settings |

## 5. When something breaks

- **502 from the tailnet URL** — Tailscale is fine; the backend behind it
  is down. `ai-imessage service status`, then `service start`.
- **Permission denied in the sync log** — the binary's FDA grant lapsed
  (usually after an upgrade). Re-toggle it in System Settings.
- **TLS error on the phone** — you're pointing at a plain-http port.
  Front it with `tailscale serve` and use the https URL.
- **`tailscale serve` on port 443 doesn't answer** — something else owns
  443 (Docker Desktop is a known squatter). Use a high port: `--https=8443`.
- **Certificate errors right after the first `tailscale serve`** — the
  Let's Encrypt certificate takes a moment to provision. Retry shortly.
- **401 from `/mcp`** — missing or stale bearer token; re-check with
  `ai-imessage connect`.

## What this setup exposes, and to whom

Message content lives in one place: the local index (owner-only file
permissions). The MCP server is read-only, requires the bearer token on
every request, and listens on loopback unless you explicitly opted
otherwise; the tailnet proxy is reachable only by devices in your own
tailnet. Nothing is sent to any third party — the only network traffic
in the default configuration is the one-time embedding-model download,
which contains no user data.
