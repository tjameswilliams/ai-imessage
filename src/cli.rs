//! Command-line interface. Human-readable output on stdout, logs on stderr.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::chunk::ChunkParams;
use crate::config::{self, LoadedConfig};
use crate::doctor;
use crate::dryrun;
use crate::embed;
use crate::etl;
use crate::extract::SourceDb;
use crate::index::IndexDb;
use crate::retrieve;

#[derive(Parser)]
#[command(
    name = "ai-imessage",
    version,
    about = "Local-first Apple Messages RAG index and MCP server",
    propagate_version = true
)]
pub struct Cli {
    /// Path to a config file (default: ~/Library/Application Support/ai-imessage/config.toml)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Increase log verbosity on stderr (-v info, -vv debug)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Diagnose database access, permissions, and configuration
    Doctor,
    /// Extract, transform, and load messages into the local index
    Etl(EtlArgs),
    /// Keyword search over the indexed history (prints message content)
    Search(SearchArgs),
    /// Serve the index to MCP clients over stdio (default) or HTTP
    Serve(ServeArgs),
    /// Manage the scheduled background sync (launchd agent)
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Print ready-to-paste MCP client configuration (and the HTTP token)
    Connect(ConnectArgs),
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Args)]
pub struct EtlArgs {
    /// Read and report on the source database without writing anything
    #[arg(long)]
    pub dry_run: bool,

    /// DEBUG: print the first N message texts to stdout (message content
    /// will appear in your terminal and shell history)
    #[arg(long, value_name = "N", requires = "dry_run")]
    pub debug_show_text: Option<usize>,

    /// Discard the existing index and re-ingest everything from scratch
    #[arg(long, conflicts_with = "dry_run")]
    pub rebuild: bool,

    /// Skip the embedding stage (sync and chunk only)
    #[arg(long, conflicts_with = "dry_run")]
    pub no_embed: bool,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Search terms (matched as keywords, best conversation chunks first)
    #[arg(required = true)]
    pub query: Vec<String>,

    /// Maximum number of results (default: retrieval.result_limit)
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,

    /// Semantic (embedding) search only, skipping keyword match
    #[arg(long, conflicts_with = "keyword")]
    pub semantic: bool,

    /// Keyword (FTS5) search only, skipping embeddings
    #[arg(long)]
    pub keyword: bool,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Serve MCP over streamable HTTP on this address (e.g. 127.0.0.1:8787)
    /// instead of stdio. Requests must present the bearer token.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<String>,
}

#[derive(Args)]
pub struct ConnectArgs {
    /// Print only the HTTP bearer token (for scripting)
    #[arg(long)]
    pub token_only: bool,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Install and load the sync agent (runs every service.interval_seconds)
    Install {
        /// Write plists without loading them into launchd
        #[arg(long)]
        no_load: bool,

        /// ALSO keep the MCP HTTP server running (opt-in; bearer-token
        /// auth; ADDR defaults to 127.0.0.1:8787)
        #[arg(
            long,
            value_name = "ADDR",
            num_args = 0..=1,
            default_missing_value = crate::service::DEFAULT_HTTP_ADDR
        )]
        http: Option<String>,
    },
    /// Resume installed agents without reinstalling
    Start {
        /// Start only the MCP HTTP server agent
        #[arg(long)]
        http_only: bool,
    },
    /// Pause agents, keeping them installed (`start` resumes)
    Stop {
        /// Stop only the MCP HTTP server agent
        #[arg(long)]
        http_only: bool,
    },
    /// Unload agents and remove their plists
    Uninstall {
        /// Remove only the MCP HTTP server agent, keeping the sync agent
        #[arg(long)]
        http_only: bool,
    },
    /// Show whether the agents are installed, loaded, and their recent logs
    Status,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print the effective configuration (secrets redacted)
    Show,
    /// Print the config file path
    Path,
}

pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let loaded = config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Doctor => run_doctor(&loaded),
        Command::Etl(args) => run_etl(&loaded, &args),
        Command::Search(args) => run_search(&loaded, &args),
        Command::Serve(args) => run_serve(&loaded, &args),
        Command::Service { command } => run_service(&loaded, cli.config.as_deref(), &command),
        Command::Connect(args) => run_connect(&loaded, &args),
        Command::Config { command } => run_config(&loaded, &command),
    }
}

fn run_doctor(loaded: &LoadedConfig) -> Result<ExitCode> {
    let checks = doctor::run_checks(loaded);
    print!("{}", doctor::render(&checks));
    Ok(if doctor::has_failure(&checks) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn run_etl(loaded: &LoadedConfig, args: &EtlArgs) -> Result<ExitCode> {
    let source_path = loaded.config.source_db_path()?;
    let db = SourceDb::open(&source_path).context("run `ai-imessage doctor` for diagnostics")?;

    if args.dry_run {
        let report = dryrun::build_report(&db)?;
        println!("{report}");

        if let Some(n) = args.debug_show_text {
            eprintln!("\nwarning: --debug-show-text prints private message content");
            let samples = dryrun::text_samples(&db, n)?;
            println!("\nFirst {} message texts (truncated):", samples.len());
            for s in samples {
                println!("  {s}");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let index_path = loaded.config.index_db_path()?;
    let mut index = IndexDb::open(&index_path)?;
    if args.rebuild {
        index.reset()?;
    }
    // Contact names are enrichment, never a requirement: a missing or
    // unreadable AddressBook downgrades to handle-only labels.
    let contacts = match loaded.config.contacts_path()? {
        Some(path) => match crate::contacts::ContactBook::load(&path) {
            Ok(book) => Some(book),
            Err(e) => {
                eprintln!("warning: contact names unavailable: {e}");
                None
            }
        },
        None => None,
    };
    let report = etl::sync(
        &db,
        &mut index,
        loaded.config.source.recent_overlap_rows,
        &chunk_params(loaded),
        contacts.as_ref(),
    )?;
    println!("{report}");

    if !args.no_embed {
        let mut embedder = embed::make_embedder(&loaded.config, &model_cache_dir(loaded)?)?;
        let report = etl::embed_missing(&mut index, embedder.as_mut(), |done, todo| {
            if done % 2048 == 0 || done == todo {
                eprintln!("embedding… {done}/{todo}");
            }
        })?;
        println!("\n{report}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Downloaded model weights live next to the index they serve.
fn model_cache_dir(loaded: &LoadedConfig) -> Result<PathBuf> {
    Ok(loaded.config.index_dir()?.join("models"))
}

fn chunk_params(loaded: &LoadedConfig) -> ChunkParams {
    ChunkParams {
        gap_minutes: loaded.config.index.chunk_gap_minutes,
        target_tokens: loaded.config.index.chunk_target_tokens,
        overlap_messages: loaded.config.index.chunk_overlap_messages,
    }
}

fn run_search(loaded: &LoadedConfig, args: &SearchArgs) -> Result<ExitCode> {
    let index_path = loaded.config.index_db_path()?;
    if !index_path.exists() {
        anyhow::bail!(
            "no index at {} — run `ai-imessage etl` first",
            index_path.display()
        );
    }
    let index = IndexDb::open(&index_path)?;
    let query = args.query.join(" ");
    let limit = args.limit.unwrap_or(loaded.config.retrieval.result_limit);
    let hits = if args.semantic {
        let mut embedder = embed::make_embedder(&loaded.config, &model_cache_dir(loaded)?)?;
        let query_vec = embedder.embed_query(&query)?;
        index.vector_search(&query_vec, limit)?
    } else if args.keyword {
        index.search(&query, limit)?
    } else {
        // Default: hybrid. Without embeddings (etl --no-embed), the vector
        // half contributes nothing and this is plain keyword ranking.
        let query_vec = if index.embedding_count()? > 0 {
            let mut embedder = embed::make_embedder(&loaded.config, &model_cache_dir(loaded)?)?;
            Some(embedder.embed_query(&query)?)
        } else {
            None
        };
        let params = retrieve::RetrievalParams {
            fts_candidates: loaded.config.retrieval.fts_candidates,
            vector_candidates: loaded.config.retrieval.vector_candidates,
            limit,
        };
        retrieve::hybrid_search(&index, &query, query_vec.as_deref(), &params)?
    };

    if hits.is_empty() {
        println!("No matches for \"{query}\".");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{} result(s) for \"{query}\"\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        let score = h
            .score
            .map(|s| format!(" · similarity {s:.2}"))
            .unwrap_or_default();
        println!(
            "{}. {} — {} ({} messages{score})",
            i + 1,
            h.chat_label,
            format_range(h.started_at_ms, h.ended_at_ms),
            h.message_count
        );
        println!("   {}\n", h.snippet.replace('\n', "\n   "));
    }
    Ok(ExitCode::SUCCESS)
}

fn format_range(start_ms: Option<i64>, end_ms: Option<i64>) -> String {
    let day = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into())
    };
    match (start_ms, end_ms) {
        (Some(s), Some(e)) if day(s) == day(e) => day(s),
        (Some(s), Some(e)) => format!("{} → {}", day(s), day(e)),
        (Some(s), None) | (None, Some(s)) => day(s),
        (None, None) => "unknown date".into(),
    }
}

fn run_serve(loaded: &LoadedConfig, args: &ServeArgs) -> Result<ExitCode> {
    use std::io::{BufRead, Write};

    let index_path = loaded.config.index_db_path()?;
    if !index_path.exists() {
        anyhow::bail!(
            "no index at {} — run `ai-imessage etl` first",
            index_path.display()
        );
    }
    let index = IndexDb::open(&index_path)?;
    let mut server = crate::mcp::McpServer::new(index, loaded.config.clone());

    if let Some(addr) = &args.http {
        let token = match &loaded.config.service.http_token {
            Some(t) => t.clone(),
            None => load_or_create_http_token(&loaded.config.index_dir()?)?,
        };
        crate::mcp::serve_http(&mut server, addr, &token)?;
        return Ok(ExitCode::SUCCESS);
    }

    // stdout is the protocol channel: newline-delimited JSON-RPC frames,
    // nothing else. EOF on stdin is a clean shutdown.
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(crate::mcp::rpc_error(
                serde_json::Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(resp) = response {
            serde_json::to_writer(&mut stdout, &resp)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_service(
    loaded: &LoadedConfig,
    explicit_config: Option<&std::path::Path>,
    command: &ServiceCommand,
) -> Result<ExitCode> {
    match command {
        ServiceCommand::Install { no_load, http } => {
            crate::service::install(loaded, explicit_config, *no_load, http.as_deref())?
        }
        ServiceCommand::Start { http_only } => crate::service::start(*http_only)?,
        ServiceCommand::Stop { http_only } => crate::service::stop(*http_only)?,
        ServiceCommand::Uninstall { http_only } => crate::service::uninstall(*http_only)?,
        ServiceCommand::Status => crate::service::status(loaded)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Everything an MCP client needs, ready to paste. The token IS printed —
/// that is the point of the command; it never leaves this terminal unless
/// the user pastes it somewhere.
fn run_connect(loaded: &LoadedConfig, args: &ConnectArgs) -> Result<ExitCode> {
    let token = match &loaded.config.service.http_token {
        Some(t) => t.clone(),
        None => load_or_create_http_token(&loaded.config.index_dir()?)?,
    };
    if args.token_only {
        println!("{token}");
        return Ok(ExitCode::SUCCESS);
    }

    let binary = std::env::current_exe()?;
    let stdio = serde_json::json!({
        "mcpServers": {
            "imessage": { "command": binary, "args": ["serve"] }
        }
    });
    println!("MCP over stdio — Claude Desktop, LM Studio, Codex, …:\n");
    println!("{}\n", serde_json::to_string_pretty(&stdio)?);

    match crate::service::installed_http_addr()? {
        Some(addr) => {
            let http = serde_json::json!({
                "mcpServers": {
                    "imessage": {
                        "url": format!("http://{addr}/mcp"),
                        "headers": { "Authorization": format!("Bearer {token}") }
                    }
                }
            });
            println!("MCP over HTTP — served persistently at http://{addr}/mcp:");
            println!("(check it is running: ai-imessage service status)\n");
            println!("{}\n", serde_json::to_string_pretty(&http)?);

            match crate::tailnet::mcp_url_for(&addr) {
                Some(url) => {
                    let tailnet = serde_json::json!({
                        "mcpServers": {
                            "imessage": {
                                "url": url,
                                "headers": { "Authorization": format!("Bearer {token}") }
                            }
                        }
                    });
                    println!("MCP over your tailnet — for phones and other tailnet devices:\n");
                    println!("{}\n", serde_json::to_string_pretty(&tailnet)?);
                }
                None => println!(
                    "For access beyond this machine, front the loopback server \
                     with a private proxy (e.g. `tailscale serve --bg \
                     --https=8443 http://{addr}`), then re-run `ai-imessage \
                     connect` — the tailnet URL will be detected and printed \
                     as ready-to-paste JSON.\n"
                ),
            }
            println!("bearer token: {token}");
        }
        None => {
            println!(
                "MCP over HTTP: not enabled. Opt in with\n  \
                 ai-imessage service install --http\nthen re-run \
                 `ai-imessage connect` for the URL, token, and JSON."
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The HTTP bearer token: from config when set, else generated once and
/// kept (owner-only) next to the index.
fn load_or_create_http_token(index_dir: &std::path::Path) -> Result<String> {
    use std::io::Read;

    let path = index_dir.join("http-token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let token = existing.trim().to_string();
        if !token.is_empty() {
            eprintln!("using bearer token from {}", path.display());
            return Ok(token);
        }
    }

    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read randomness for token generation")?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    std::fs::create_dir_all(index_dir)?;
    std::fs::write(&path, format!("{token}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("generated bearer token, stored at {}", path.display());
    Ok(token)
}

fn run_config(loaded: &LoadedConfig, command: &ConfigCommand) -> Result<ExitCode> {
    match command {
        ConfigCommand::Show => {
            let origin = if loaded.from_file {
                "loaded"
            } else {
                "missing — built-in defaults shown"
            };
            println!("# config file: {} ({origin})", loaded.path.display());
            print!("{}", loaded.config.redacted().to_toml()?);
        }
        ConfigCommand::Path => println!("{}", loaded.path.display()),
    }
    Ok(ExitCode::SUCCESS)
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    // stdout stays reserved for command output (and, later, MCP protocol
    // messages); logs go to stderr. try_init: tests may init repeatedly.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dry_run_flag_parses() {
        let cli = Cli::parse_from(["ai-imessage", "etl", "--dry-run"]);
        match cli.command {
            Command::Etl(args) => assert!(args.dry_run),
            _ => panic!("expected etl"),
        }
    }

    #[test]
    fn debug_show_text_requires_dry_run() {
        assert!(Cli::try_parse_from(["ai-imessage", "etl", "--debug-show-text", "3"]).is_err());
        assert!(
            Cli::try_parse_from(["ai-imessage", "etl", "--dry-run", "--debug-show-text", "3"])
                .is_ok()
        );
    }

    #[test]
    fn global_config_flag_parses_anywhere() {
        let cli = Cli::parse_from(["ai-imessage", "doctor", "--config", "/tmp/x.toml"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/x.toml")));
    }

    #[test]
    fn missing_subcommand_is_an_error() {
        assert!(Cli::try_parse_from(["ai-imessage"]).is_err());
    }
}
