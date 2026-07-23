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

pub fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn log_path(loaded: &LoadedConfig) -> Result<PathBuf> {
    Ok(loaded.config.index_dir()?.join("logs/etl.log"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent plist. `explicit_config` is the `--config` the user passed
/// when installing, if any; it is baked into the agent so both always
/// read the same configuration.
pub fn render_plist(
    binary: &Path,
    interval: u32,
    log: &Path,
    explicit_config: Option<&Path>,
) -> String {
    let mut args = vec![binary.display().to_string()];
    if let Some(cfg) = explicit_config {
        args.push("--config".into());
        args.push(cfg.display().to_string());
    }
    args.push("etl".into());
    let args_xml: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args_xml}  </array>
  <key>StartInterval</key>
  <integer>{interval}</integer>
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
        log = xml_escape(&log.display().to_string()),
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

/// Write the plist and load it. `no_load` writes the plist only (used by
/// tests and by users who prefer to load manually).
pub fn install(loaded: &LoadedConfig, explicit_config: Option<&Path>, no_load: bool) -> Result<()> {
    let binary = std::env::current_exe().context("could not determine the binary's own path")?;
    let log = log_path(loaded)?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    let plist = plist_path()?;
    if let Some(dir) = plist.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(
        &plist,
        render_plist(
            &binary,
            loaded.config.service.interval_seconds,
            &log,
            explicit_config,
        ),
    )?;
    println!("wrote {}", plist.display());

    if !no_load {
        let uid = uid()?;
        // Reload cleanly if an older agent is already bootstrapped.
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")]);
        let out = launchctl(&[
            "bootstrap",
            &format!("gui/{uid}"),
            &plist.display().to_string(),
        ])?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!(
            "loaded {LABEL} (runs `etl` every {}s)",
            loaded.config.service.interval_seconds
        );
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

pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;
    let uid = uid()?;
    let _ = launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")]);
    if plist.exists() {
        fs::remove_file(&plist)?;
        println!("removed {}", plist.display());
    } else {
        println!("no agent installed ({} not present)", plist.display());
    }
    Ok(())
}

pub fn status(loaded: &LoadedConfig) -> Result<()> {
    let plist = plist_path()?;
    if !plist.exists() {
        println!(
            "not installed — run `ai-imessage service install` to sync every {}s",
            loaded.config.service.interval_seconds
        );
        return Ok(());
    }
    println!("installed: {}", plist.display());

    let uid = uid()?;
    let loaded_in_launchd = launchctl(&["print", &format!("gui/{uid}/{LABEL}")])
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "launchd:   {}",
        if loaded_in_launchd {
            "loaded"
        } else {
            "NOT loaded — run `ai-imessage service install` to (re)load"
        }
    );

    let log = log_path(loaded)?;
    match fs::read_to_string(&log) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().rev().take(8).collect();
            println!(
                "log:       {} (last {} line(s)):",
                log.display(),
                lines.len()
            );
            for line in lines.iter().rev() {
                println!("  {line}");
            }
        }
        Err(_) => println!("log:       {} (no runs logged yet)", log.display()),
    }
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
