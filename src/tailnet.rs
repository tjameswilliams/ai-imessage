//! Best-effort Tailscale detection for `connect`.
//!
//! Tailscale is one deployment option, never a dependency: everything
//! here degrades to `None` when the CLI is absent, the backend is not
//! running, or no serve proxy fronts our server. Parsers are pure and
//! tested against captured CLI output; only the thin wrappers shell out.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// A running tailnet node, as reported by `tailscale status`.
#[derive(Debug, Clone, PartialEq)]
pub struct TailnetInfo {
    /// MagicDNS name without the trailing dot, e.g. `mac.tail1234.ts.net`.
    pub dns_name: String,
    pub ips: Vec<String>,
}

/// The tailnet URL (if any) where our MCP server at `backend_addr` is
/// reachable — either through a `tailscale serve` proxy (HTTPS) or by
/// having bound a tailnet/unspecified address directly (HTTP).
pub fn mcp_url_for(backend_addr: &str) -> Option<String> {
    let bin = tailscale_bin()?;
    let info = parse_status(&run(&bin, &["status", "--json"])?)?;
    if let Some(base) = parse_serve_proxy(&run(&bin, &["serve", "status", "--json"])?, backend_addr)
    {
        return Some(format!("{base}/mcp"));
    }
    let (host, port) = backend_addr.rsplit_once(':')?;
    if host == "0.0.0.0" || info.ips.iter().any(|ip| ip == host) {
        return Some(format!("http://{}:{port}/mcp", info.dns_name));
    }
    None
}

fn tailscale_bin() -> Option<PathBuf> {
    let candidates = [
        "tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    candidates.iter().map(PathBuf::from).find(|c| {
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn run(bin: &PathBuf, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `tailscale status --json` → node info, only when the backend runs.
pub fn parse_status(json: &str) -> Option<TailnetInfo> {
    let v: Value = serde_json::from_str(json).ok()?;
    if v.get("BackendState")?.as_str()? != "Running" {
        return None;
    }
    let node = v.get("Self")?;
    let dns_name = node
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.')
        .to_string();
    if dns_name.is_empty() {
        return None;
    }
    let ips = node
        .get("TailscaleIPs")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Some(TailnetInfo { dns_name, ips })
}

/// `tailscale serve status --json` → the `https://host:port` that proxies
/// to `http://backend_addr`, if such a serve config exists.
pub fn parse_serve_proxy(json: &str, backend_addr: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let target = format!("http://{backend_addr}");
    for (host_port, site) in v.get("Web")?.as_object()? {
        let Some(handlers) = site.get("Handlers").and_then(Value::as_object) else {
            continue;
        };
        for (mount, handler) in handlers {
            if handler.get("Proxy").and_then(Value::as_str) == Some(target.as_str()) {
                let mount = mount.trim_end_matches('/');
                return Some(format!("https://{host_port}{mount}"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "BackendState": "Running",
      "Self": {
        "DNSName": "mac.tail1234.ts.net.",
        "TailscaleIPs": ["100.1.2.3", "fd7a::1"]
      }
    }"#;

    const SERVE: &str = r#"{
      "TCP": { "8443": { "HTTPS": true } },
      "Web": {
        "mac.tail1234.ts.net:8443": {
          "Handlers": { "/": { "Proxy": "http://127.0.0.1:8787" } }
        }
      }
    }"#;

    #[test]
    fn status_parses_dns_name_without_trailing_dot() {
        let info = parse_status(STATUS).unwrap();
        assert_eq!(info.dns_name, "mac.tail1234.ts.net");
        assert_eq!(info.ips, vec!["100.1.2.3", "fd7a::1"]);
    }

    #[test]
    fn stopped_backend_is_not_detected() {
        let stopped = STATUS.replace("Running", "Stopped");
        assert_eq!(parse_status(&stopped), None);
        assert_eq!(parse_status("not json"), None);
        assert_eq!(parse_status("{}"), None);
    }

    #[test]
    fn serve_proxy_is_found_for_the_matching_backend() {
        assert_eq!(
            parse_serve_proxy(SERVE, "127.0.0.1:8787").as_deref(),
            Some("https://mac.tail1234.ts.net:8443")
        );
    }

    #[test]
    fn serve_proxy_ignores_other_backends_and_bad_json() {
        assert_eq!(parse_serve_proxy(SERVE, "127.0.0.1:9999"), None);
        assert_eq!(parse_serve_proxy("{}", "127.0.0.1:8787"), None);
        assert_eq!(parse_serve_proxy("nope", "127.0.0.1:8787"), None);
    }

    #[test]
    fn serve_proxy_keeps_non_root_mounts() {
        let nested = SERVE.replace(r#""/":"#, r#""/imessage":"#);
        assert_eq!(
            parse_serve_proxy(&nested, "127.0.0.1:8787").as_deref(),
            Some("https://mac.tail1234.ts.net:8443/imessage")
        );
    }
}
