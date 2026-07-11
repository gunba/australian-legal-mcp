//! HTTP transport smoke test.
//!
//! Spawns the release binary against a tempdir data dir, hits the MCP
//! endpoint with `initialize` and `tools/list`, and asserts the JSON
//! shape. Readiness is deterministic — we read the `ato-mcp listening
//! on ...` line from stderr before issuing requests.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tempfile::tempdir;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_ato-mcp")
}

fn pick_free_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1:0 to discover a free port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

struct Server {
    child: Child,
    url: String,
    stderr_thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_server(data_dir: &std::path::Path) -> Result<Server> {
    let mut last_error = None;
    for _ in 0..3 {
        match start_server_once(data_dir) {
            Ok(server) => return Ok(server),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("server startup failed")))
}

fn start_server_once(data_dir: &std::path::Path) -> Result<Server> {
    let port = pick_free_port()?;
    let url = format!("http://127.0.0.1:{port}/mcp");

    let mut child = Command::new(bin_path())
        .args(["serve", "--port", &port.to_string()])
        .env("ATO_MCP_DATA_DIR", data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning serve")?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture stderr"))?;
    let (line_sender, line_receiver) = mpsc::channel();
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            let _ = line_sender.send(line);
        }
    });
    let needle = format!("listening on {url}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            let _ = stderr_thread.join();
            return Err(anyhow!(
                "serve exited with {status} before emitting readiness line"
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_thread.join();
            return Err(anyhow!("timed out waiting for serve readiness"));
        }
        match line_receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) if line.contains(&needle) => break,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_thread.join();
                return Err(anyhow!("serve stderr closed before readiness"));
            }
        }
    }

    Ok(Server {
        child,
        url,
        stderr_thread: Some(stderr_thread),
    })
}

fn post(url: &str, payload: Value) -> Result<Value> {
    let response = post_raw(
        url,
        &serde_json::to_string(&payload)?,
        Some("application/json"),
    )?;
    if !(200..300).contains(&response.status) {
        return Err(anyhow!("HTTP {}: {}", response.status, response.body));
    }
    Ok(serde_json::from_str(&response.body)?)
}

struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    body: String,
}

fn post_raw(url: &str, body: &str, content_type: Option<&str>) -> Result<HttpResponse> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut request = client.post(url).body(body.to_string());
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let resp = request.send()?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = resp.text()?;
    Ok(HttpResponse {
        status: status.as_u16(),
        content_type,
        body,
    })
}

#[test]
fn initialize_and_tools_list_over_http() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;

    let init = post(
        &server.url,
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

    let tools = post(
        &server.url,
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
    for expected in ["search", "get_chunks", "get_doc_anchors", "fetch", "stats"] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected `{expected}` in tool list, got {names:?}"
        );
    }
    for removed in [
        "search_titles",
        "get_document",
        "fetch_external_doc",
        "doctor",
        "install_http",
    ] {
        assert!(
            !names.iter().any(|n| n == removed),
            "`{removed}` should no longer be exposed, got {names:?}"
        );
    }

    drop(server);
    Ok(())
}

#[test]
fn rejects_non_mcp_paths_and_methods() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let bad_path = client
        .post(format!("{}/wrong", server.url.trim_end_matches("/mcp")))
        .body("{}")
        .send()?;
    assert_eq!(bad_path.status().as_u16(), 404);

    let bad_method = client.get(&server.url).send()?;
    assert_eq!(bad_method.status().as_u16(), 405);

    drop(server);
    Ok(())
}

#[test]
fn json_rpc_notifications_batches_and_errors_conform() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;

    let notification = post_raw(
        &server.url,
        r#"{"jsonrpc":"2.0","method":"ping"}"#,
        Some("application/json; charset=utf-8"),
    )?;
    assert_eq!(notification.status, 204);
    assert!(notification.body.is_empty());

    let batch = post_raw(
        &server.url,
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"ping"}]"#,
        Some("application/json"),
    )?;
    assert_eq!(batch.status, 200);
    assert_eq!(batch.content_type.as_deref(), Some("application/json"));
    let batch: Value = serde_json::from_str(&batch.body)?;
    assert_eq!(batch.as_array().map(Vec::len), Some(1));
    assert_eq!(batch[0]["id"], 1);

    for (request, code) in [
        ("[]", -32600),
        (r#"{"jsonrpc":"1.0","id":2,"method":"ping"}"#, -32600),
        (r#"{"jsonrpc":"2.0","id":3,"method":"missing"}"#, -32601),
        (
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search","arguments":{"query":42,"k":"8"}}}"#,
            -32602,
        ),
    ] {
        let response = post_raw(&server.url, request, Some("application/json"))?;
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some("application/json"));
        let value: Value = serde_json::from_str(&response.body)?;
        assert_eq!(value["error"]["code"], code, "response: {value}");
    }

    let parse_error = post_raw(&server.url, "{", Some("application/json"))?;
    let parse_error: Value = serde_json::from_str(&parse_error.body)?;
    assert_eq!(parse_error["error"]["code"], -32700);
    Ok(())
}

#[test]
fn enforces_json_content_type_and_body_limit() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;

    let wrong_type = post_raw(
        &server.url,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        None,
    )?;
    assert_eq!(wrong_type.status, 415);
    assert_eq!(wrong_type.content_type.as_deref(), Some("application/json"));

    let oversized = post_raw(
        &server.url,
        &format!("\"{}\"", "x".repeat(1024 * 1024)),
        Some("application/json"),
    )?;
    assert_eq!(oversized.status, 413);
    assert_eq!(oversized.content_type.as_deref(), Some("application/json"));
    Ok(())
}

#[test]
fn rejects_non_loopback_bind() -> Result<()> {
    let dir = tempdir()?;
    let output = Command::new(bin_path())
        .args(["serve", "--port", "0", "--bind", "0.0.0.0"])
        .env("ATO_MCP_DATA_DIR", dir.path())
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback-only"));
    Ok(())
}
