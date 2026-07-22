//! End-to-end test of the streamable-HTTP MCP transport: spawn the real
//! binary with `serve --http 127.0.0.1:0`, discover the bound port from
//! stderr, and drive it with a real HTTP client.

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use common::{Fixture, MessageSpec, SchemaVariant, apple_ns};
use serde_json::{Value, json};

const TOKEN: &str = "test-http-token";

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_config(fixture_db: &Path, dir: &Path) -> std::path::PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[source]\ndatabase_path = \"{}\"\ncontacts_path = \"\"\n\n\
             [index]\ndatabase_path = \"{}\"\n\n\
             [embeddings]\nprovider = \"debug-hash\"\n\n\
             [service]\nhttp_token = \"{TOKEN}\"\n",
            fixture_db.display(),
            dir.join("index/index.sqlite").display(),
        ),
    )
    .unwrap();
    config_path
}

/// Build an index from a fixture, then start `serve --http` and wait for
/// the "listening" line to learn the port.
fn start_server() -> (Fixture, Server) {
    let f = Fixture::new(SchemaVariant::Modern);
    let alice = f.add_handle("+15550100001");
    let chat = f.add_chat("direct-chat", ai_imessage::model::CHAT_STYLE_DIRECT, None);
    let m = f.add_message(&MessageSpec {
        guid: "h1",
        text: Some("the SECRET launch is thursday"),
        handle_id: Some(alice),
        date: apple_ns("2026-07-01T09:00:00Z"),
        ..Default::default()
    });
    f.link_chat_message(chat, m);
    let config = write_config(&f.db_path, f.dir.path());

    let bin = env!("CARGO_BIN_EXE_ai-imessage");
    let ok = Command::new(bin)
        .args(["--config", config.to_str().unwrap(), "etl"])
        .output()
        .unwrap();
    assert!(ok.status.success(), "etl failed: {ok:?}");

    let mut child = Command::new(bin)
        .args([
            "--config",
            config.to_str().unwrap(),
            "serve",
            "--http",
            "127.0.0.1:0",
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut url = None;
    for line in stderr.lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix("MCP listening on ") {
            url = Some(rest.trim().to_string());
            break;
        }
    }
    let url = url.expect("server printed its listening address");
    (f, Server { child, url })
}

fn post(url: &str, auth: Option<&str>, body: &Value) -> (u16, Value) {
    let mut req = ureq::post(url);
    if let Some(token) = auth {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.into_string().unwrap_or_default();
            let value = serde_json::from_str(&text).unwrap_or(Value::Null);
            (status, value)
        }
        Err(ureq::Error::Status(status, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let value = serde_json::from_str(&text).unwrap_or(Value::Null);
            (status, value)
        }
        Err(e) => panic!("transport error: {e}"),
    }
}

#[test]
fn http_transport_speaks_mcp_with_bearer_auth() {
    let (_f, server) = start_server();
    let url = &server.url;

    // Wrong or missing token: 401, and no protocol details leak.
    let (status, _) = post(url, None, &json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
    assert_eq!(status, 401);
    let (status, _) = post(
        url,
        Some("wrong-token"),
        &json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    );
    assert_eq!(status, 401);

    // Handshake.
    let (status, resp) = post(
        url,
        Some(TOKEN),
        &json!({"jsonrpc":"2.0","id":0,"method":"initialize",
                "params":{"protocolVersion":"2025-06-18"}}),
    );
    assert_eq!(status, 200);
    assert_eq!(resp["result"]["serverInfo"]["name"], "ai-imessage");

    // Notifications are accepted with no body.
    let (status, _) = post(
        url,
        Some(TOKEN),
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    assert_eq!(status, 202);

    // A real tool call round-trips.
    let (status, resp) = post(
        url,
        Some(TOKEN),
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"search_messages","arguments":{"query":"SECRET"}}}),
    );
    assert_eq!(status, 200);
    assert_eq!(resp["result"]["isError"], false);
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("SECRET launch")
    );

    // Malformed JSON is a -32700, not a crash.
    let req = ureq::post(url).set("Authorization", &format!("Bearer {TOKEN}"));
    let status = match req.send_string("not json") {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("transport error: {e}"),
    };
    assert_eq!(status, 400);

    // GET is not supported (no server-initiated streams).
    let status = match ureq::get(url)
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("transport error: {e}"),
    };
    assert_eq!(status, 405);

    // Unknown paths 404.
    let (status, _) = post(
        &server.url.replace("/mcp", "/other"),
        Some(TOKEN),
        &json!({}),
    );
    assert_eq!(status, 404);
}
