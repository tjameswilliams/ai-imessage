//! The scheduled background sync: a per-user launchd agent that runs
//! `ai-imessage etl` every `service.interval_seconds`.
//!
//! launchd runs the binary directly, so macOS attributes Full Disk Access
//! to the *binary*, not the terminal that installed it — `install` says so
//! loudly. Logs go to a file next to the index and contain only sync
//! reports (counts), never message content.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::LoadedConfig;

pub const LABEL: &str = "com.ai-imessage.etl";
/// Opt-in second agent that keeps the MCP HTTP server running.
pub const SERVE_LABEL: &str = "com.ai-imessage.serve";
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8787";

pub fn plist_path() -> Result<PathBuf> {
    plist_path_for(LABEL)
}

pub fn serve_plist_path() -> Result<PathBuf> {
    plist_path_for(SERVE_LABEL)
}

fn plist_path_for(label: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn log_path(loaded: &LoadedConfig) -> Result<PathBuf> {
    Ok(loaded.config.index_dir()?.join("logs/etl.log"))
}

fn serve_log_path(loaded: &LoadedConfig) -> Result<PathBuf> {
    Ok(loaded.config.index_dir()?.join("logs/serve.log"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// How launchd keeps an agent running.
enum Schedule {
    /// Run every N seconds (the sync agent).
    Every(u32),
    /// Keep the process alive, restarting on exit (the HTTP server).
    KeepAlive,
}

fn build_plist(
    label: &str,
    binary: &Path,
    explicit_config: Option<&Path>,
    subcommand: &[&str],
    schedule: &Schedule,
    log: &Path,
) -> String {
    let mut args = vec![binary.display().to_string()];
    if let Some(cfg) = explicit_config {
        args.push("--config".into());
        args.push(cfg.display().to_string());
    }
    args.extend(subcommand.iter().map(|s| s.to_string()));
    let args_xml: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let schedule_xml = match schedule {
        Schedule::Every(interval) => {
            format!("  <key>StartInterval</key>\n  <integer>{interval}</integer>")
        }
        Schedule::KeepAlive => "  <key>KeepAlive</key>\n  <true/>".to_string(),
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args_xml}  </array>
{schedule_xml}
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        log = xml_escape(&log.display().to_string()),
    )
}

/// The sync-agent plist. `explicit_config` is the `--config` the user
/// passed when installing, if any; it is baked into the agent so both
/// always read the same configuration.
pub fn render_plist(
    binary: &Path,
    interval: u32,
    log: &Path,
    explicit_config: Option<&Path>,
) -> String {
    build_plist(
        LABEL,
        binary,
        explicit_config,
        &["etl"],
        &Schedule::Every(interval),
        log,
    )
}

/// The opt-in MCP HTTP server plist: kept alive rather than scheduled.
pub fn render_serve_plist(
    binary: &Path,
    addr: &str,
    log: &Path,
    explicit_config: Option<&Path>,
) -> String {
    build_plist(
        SERVE_LABEL,
        binary,
        explicit_config,
        &["serve", "--http", addr],
        &Schedule::KeepAlive,
        log,
    )
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("launchctl")
        .args(args)
        .output()
        .context("could not run launchctl")
}

fn write_and_load(plist: &Path, content: &str, label: &str, no_load: bool) -> Result<()> {
    if let Some(dir) = plist.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(plist, content)?;
    println!("wrote {}", plist.display());
    if no_load {
        return Ok(());
    }
    let uid = uid()?;
    // Reload cleanly if an older agent is already bootstrapped.
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{label}")]);
    let out = launchctl(&[
        "bootstrap",
        &format!("gui/{uid}"),
        &plist.display().to_string(),
    ])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed for {label}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("loaded {label}");
    Ok(())
}

/// Install the sync agent, and — only when `http` is given — the opt-in
/// MCP HTTP server agent. `no_load` writes plists without touching
/// launchd (used by tests and by users who prefer to load manually).
pub fn install(
    loaded: &LoadedConfig,
    explicit_config: Option<&Path>,
    no_load: bool,
    http: Option<&str>,
) -> Result<()> {
    let binary = std::env::current_exe().context("could not determine the binary's own path")?;
    let log = log_path(loaded)?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }

    write_and_load(
        &plist_path()?,
        &render_plist(
            &binary,
            loaded.config.service.interval_seconds,
            &log,
            explicit_config,
        ),
        LABEL,
        no_load,
    )?;
    println!(
        "sync agent runs `etl` every {}s",
        loaded.config.service.interval_seconds
    );

    if let Some(addr) = http {
        let serve_log = serve_log_path(loaded)?;
        write_and_load(
            &serve_plist_path()?,
            &render_serve_plist(&binary, addr, &serve_log, explicit_config),
            SERVE_LABEL,
            no_load,
        )?;
        println!(
            "MCP HTTP server kept running at http://{addr}/mcp\n\
             every request needs the bearer token (config service.http_token, \
             or the generated one next to the index)"
        );
        if !addr.starts_with("127.0.0.1") && !addr.starts_with("localhost") {
            println!(
                "note: {addr} is not loopback — anyone who can reach it and \
                 holds the token can read your message history"
            );
        }
    }

    println!(
        "\nIMPORTANT: launchd runs the binary directly, so macOS needs Full \
         Disk Access granted to the binary itself:\n  System Settings → \
         Privacy & Security → Full Disk Access → add {}\nUntil then the \
         scheduled sync will log permission errors. Check with:\n  \
         ai-imessage service status",
        binary.display()
    );
    Ok(())
}

/// Remove agents. `http_only` opts back out of just the HTTP server,
/// leaving the sync agent in place.
pub fn uninstall(http_only: bool) -> Result<()> {
    let uid = uid()?;
    let mut targets = vec![(SERVE_LABEL, serve_plist_path()?)];
    if !http_only {
        targets.push((LABEL, plist_path()?));
    }
    for (label, plist) in targets {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{label}")]);
        if plist.exists() {
            fs::remove_file(&plist)?;
            println!("removed {}", plist.display());
        } else {
            println!("{label}: not installed");
        }
    }
    Ok(())
}

fn agent_status(label: &str, plist: &Path, log: Option<&Path>) {
    if !plist.exists() {
        let hint = if label == SERVE_LABEL {
            " — opt in with `ai-imessage service install --http [ADDR]`"
        } else {
            " — run `ai-imessage service install`"
        };
        println!("{label}: not installed{hint}");
        return;
    }
    let loaded_in_launchd = uid()
        .and_then(|uid| launchctl(&["print", &format!("gui/{uid}/{label}")]))
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "{label}: installed, {}",
        if loaded_in_launchd {
            "loaded"
        } else {
            "NOT loaded — re-run `ai-imessage service install` to (re)load"
        }
    );
    if let Some(log) = log {
        match fs::read_to_string(log) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().rev().take(8).collect();
                println!("  log {} (last {} line(s)):", log.display(), lines.len());
                for line in lines.iter().rev() {
                    println!("    {line}");
                }
            }
            Err(_) => println!("  log {} (no runs logged yet)", log.display()),
        }
    }
}

pub fn status(loaded: &LoadedConfig) -> Result<()> {
    agent_status(LABEL, &plist_path()?, Some(&log_path(loaded)?));
    agent_status(
        SERVE_LABEL,
        &serve_plist_path()?,
        Some(&serve_log_path(loaded)?),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_label_binary_interval_and_log() {
        let plist = render_plist(
            Path::new("/opt/bin/ai-imessage"),
            300,
            Path::new("/tmp/logs/etl.log"),
            None,
        );
        assert!(plist.contains("<string>com.ai-imessage.etl</string>"));
        assert!(plist.contains("<string>/opt/bin/ai-imessage</string>"));
        assert!(plist.contains("<string>etl</string>"));
        assert!(plist.contains("<integer>300</integer>"));
        assert!(plist.contains("<string>/tmp/logs/etl.log</string>"));
        assert!(!plist.contains("--config"));
    }

    #[test]
    fn explicit_config_is_baked_into_the_agent() {
        let plist = render_plist(
            Path::new("/opt/bin/ai-imessage"),
            60,
            Path::new("/tmp/etl.log"),
            Some(Path::new("/etc/custom.toml")),
        );
        let config_pos = plist.find("<string>--config</string>").unwrap();
        let etl_pos = plist.find("<string>etl</string>").unwrap();
        assert!(plist.contains("<string>/etc/custom.toml</string>"));
        assert!(config_pos < etl_pos, "--config must precede the subcommand");
    }

    #[test]
    fn paths_with_xml_metacharacters_are_escaped() {
        let plist = render_plist(
            Path::new("/odd & <weird>/ai-imessage"),
            300,
            Path::new("/logs & more/etl.log"),
            None,
        );
        assert!(plist.contains("/odd &amp; &lt;weird&gt;/ai-imessage"));
        assert!(plist.contains("/logs &amp; more/etl.log"));
        assert!(!plist.contains("& <"));
    }

    #[test]
    fn plist_path_is_under_launch_agents() {
        let p = plist_path().unwrap();
        assert!(p.ends_with("Library/LaunchAgents/com.ai-imessage.etl.plist"));
        let p = serve_plist_path().unwrap();
        assert!(p.ends_with("Library/LaunchAgents/com.ai-imessage.serve.plist"));
    }

    #[test]
    fn serve_plist_keeps_alive_instead_of_scheduling() {
        let plist = render_serve_plist(
            Path::new("/opt/bin/ai-imessage"),
            "127.0.0.1:8787",
            Path::new("/tmp/serve.log"),
            None,
        );
        assert!(plist.contains("<string>com.ai-imessage.serve</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>--http</string>"));
        assert!(plist.contains("<string>127.0.0.1:8787</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(!plist.contains("<key>StartInterval</key>"));
        assert!(plist.contains("<string>/tmp/serve.log</string>"));
    }

    #[test]
    fn serve_plist_bakes_in_explicit_config_before_the_subcommand() {
        let plist = render_serve_plist(
            Path::new("/b"),
            "0.0.0.0:9000",
            Path::new("/l"),
            Some(Path::new("/etc/c.toml")),
        );
        let config_pos = plist.find("<string>--config</string>").unwrap();
        let serve_pos = plist.find("<string>serve</string>").unwrap();
        assert!(config_pos < serve_pos);
    }

    #[test]
    fn plist_is_valid_enough_for_launchd_basics() {
        let plist = render_plist(Path::new("/b"), 1, Path::new("/l"), None);
        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>ProcessType</key>"));
        // Balanced dict: one open, one close.
        assert_eq!(plist.matches("<dict>").count(), 1);
        assert_eq!(plist.matches("</dict>").count(), 1);
    }
}
