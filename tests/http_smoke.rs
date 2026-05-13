//! HTTP transport smoke test.
//!
//! Spawns the release binary against a tempdir data dir, hits the MCP
//! endpoint with `initialize` and `tools/list`, and asserts the JSON
//! shape. Wait for daemon readiness is deterministic — we read the
//! `ato-mcp listening on ...` line from stderr before issuing requests.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tempfile::tempdir;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_ato-mcp")
}

struct Daemon {
    child: Child,
    url: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_daemon(data_dir: &std::path::Path) -> Result<Daemon> {
    // Pick a port and write http.json into the tempdir.
    let install = Command::new(bin_path())
        .args(["install-http", "--quiet"])
        .env("ATO_MCP_DATA_DIR", data_dir)
        .output()
        .context("running install-http")?;
    if !install.status.success() {
        return Err(anyhow!(
            "install-http exited {}: {}",
            install.status,
            String::from_utf8_lossy(&install.stderr)
        ));
    }

    // Read the chosen port back so we know the URL.
    let cfg_raw = std::fs::read_to_string(data_dir.join("http.json"))
        .context("reading http.json after install-http")?;
    let cfg: Value = serde_json::from_str(&cfg_raw)?;
    let port = cfg["port"]
        .as_u64()
        .ok_or_else(|| anyhow!("http.json missing port"))?;
    let url = format!("http://127.0.0.1:{port}/mcp");

    // Spawn `serve`, capture stderr, and wait for the readiness line.
    let mut child = Command::new(bin_path())
        .arg("serve")
        .env("ATO_MCP_DATA_DIR", data_dir)
        // Don't let the daemon try to phone home for an update banner — it
        // would add latency and external dep on github.com.
        .env("ATO_MCP_OFFLINE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning serve")?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture stderr"))?;
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let needle = format!("listening on {url}");
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF before readiness — daemon crashed.
            let _ = child.wait();
            return Err(anyhow!("serve exited before emitting readiness line"));
        }
        if line.contains(&needle) {
            break;
        }
    }

    Ok(Daemon { child, url })
}

fn post(url: &str, payload: Value) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let body = serde_json::to_string(&payload)?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}: {text}"));
    }
    Ok(serde_json::from_str(&text)?)
}

#[test]
fn initialize_and_tools_list_over_http() -> Result<()> {
    let dir = tempdir()?;
    let daemon = start_daemon(dir.path())?;

    // initialize
    let init = post(
        &daemon.url,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "smoke", "version": "0" }
            }
        }),
    )?;
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "ato-mcp");
    assert!(
        init["result"]["instructions"].is_string(),
        "initialize must surface server instructions: {init:?}"
    );

    // tools/list
    let tools = post(
        &daemon.url,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
    )?;
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    for expected in ["search", "search_titles", "get_document", "get_chunks", "stats"] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected `{expected}` in tool list, got {names:?}"
        );
    }

    drop(daemon);
    Ok(())
}

#[test]
fn rejects_non_mcp_paths_and_methods() -> Result<()> {
    let dir = tempdir()?;
    let daemon = start_daemon(dir.path())?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let bad_path = client
        .post(format!("{}/wrong", daemon.url.trim_end_matches("/mcp")))
        .body("{}")
        .send()?;
    assert_eq!(bad_path.status().as_u16(), 404);

    let bad_method = client.get(&daemon.url).send()?;
    assert_eq!(bad_method.status().as_u16(), 405);

    drop(daemon);
    Ok(())
}
