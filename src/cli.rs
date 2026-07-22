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

    /// Semantic (embedding) search instead of keyword match
    #[arg(long)]
    pub semantic: bool,
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
    let report = etl::sync(
        &db,
        &mut index,
        loaded.config.source.recent_overlap_rows,
        &chunk_params(loaded),
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
    } else {
        index.search(&query, limit)?
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
