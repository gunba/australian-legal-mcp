//! HTTP transport smoke test.
//!
//! Spawns the release binary against a tempdir data dir, hits the MCP
//! endpoint with `initialize` and `tools/list`, and asserts the JSON
//! shape. Readiness is deterministic — we read the `legal-mcp listening
//! on ...` line from stderr before issuing requests.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

const API_KEY: &str = "automation.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[derive(Clone, Copy)]
enum TestAuth {
    Disabled,
    Entra,
    ApiKey,
}

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_legal-mcp")
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
    start_server_with_auth(data_dir, TestAuth::Disabled)
}

fn start_server_with_auth(data_dir: &std::path::Path, auth: TestAuth) -> Result<Server> {
    start_server_with_auth_and_workers(data_dir, auth, None)
}

fn start_server_with_auth_and_workers(
    data_dir: &std::path::Path,
    auth: TestAuth,
    workers: Option<usize>,
) -> Result<Server> {
    let mut last_error = None;
    for _ in 0..3 {
        match start_server_once(data_dir, auth, workers) {
            Ok(server) => return Ok(server),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("server startup failed")))
}

fn start_server_once(
    data_dir: &std::path::Path,
    auth: TestAuth,
    workers: Option<usize>,
) -> Result<Server> {
    let port = pick_free_port()?;
    let url = format!("http://127.0.0.1:{port}/mcp");

    let mut command = Command::new(bin_path());
    command
        .args(["serve", "--port", &port.to_string()])
        .env("LEGAL_MCP_DATA_DIR", data_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(workers) = workers {
        command.env("LEGAL_MCP_HTTP_WORKERS", workers.to_string());
    }
    match auth {
        TestAuth::Disabled => {}
        TestAuth::Entra => {
            command.envs([
                ("LEGAL_MCP_HTTP_AUTH", "entra"),
                (
                    "LEGAL_MCP_ENTRA_TENANT_ID",
                    "11111111-1111-1111-1111-111111111111",
                ),
                (
                    "LEGAL_MCP_ENTRA_SERVER_APP_ID",
                    "33333333-3333-3333-3333-333333333333",
                ),
                (
                    "LEGAL_MCP_ENTRA_AUDIENCES",
                    "33333333-3333-3333-3333-333333333333",
                ),
                ("LEGAL_MCP_ENTRA_SCOPE", "legal.read"),
                (
                    "LEGAL_MCP_ENTRA_SCOPE_URI",
                    "api://33333333-3333-3333-3333-333333333333/legal.read",
                ),
                (
                    "LEGAL_MCP_ENTRA_ALLOWED_CLIENT_IDS",
                    "22222222-2222-2222-2222-222222222222",
                ),
                ("LEGAL_MCP_EXTERNAL_URL", "https://legal.example/mcp"),
            ]);
        }
        TestAuth::ApiKey => {
            let verifier_path = data_dir.join("api-keys.json");
            let digest = format!("{:x}", Sha256::digest(API_KEY.as_bytes()));
            std::fs::write(
                &verifier_path,
                serde_json::to_vec(&json!({
                    "version": 1,
                    "keys": [{"id": "automation", "sha256": digest}]
                }))?,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&verifier_path, std::fs::Permissions::from_mode(0o400))?;
            }
            command
                .arg("--require-http-auth")
                .env("LEGAL_MCP_HTTP_AUTH", "api-key")
                .env("LEGAL_MCP_API_KEYS_FILE", &verifier_path);
        }
    }
    let mut child = command.spawn().context("spawning serve")?;

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
    request = request
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18");
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
    assert_eq!(init["result"]["serverInfo"]["name"], "australian-legal-mcp");
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
    let listed_tools = tools["result"]["tools"].as_array().expect("tools array");
    let names: Vec<String> = listed_tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    let expected = [
        "search",
        "get_chunks",
        "get_asset",
        "get_doc_anchors",
        "get_definition",
        "stats",
        "fetch",
    ];
    let mut actual = names.clone();
    actual.sort();
    let mut expected = expected.map(str::to_string).to_vec();
    expected.sort();
    assert_eq!(
        actual, expected,
        "the MCP surface must contain exactly seven tools"
    );

    let schema = |name: &str| {
        listed_tools
            .iter()
            .find(|tool| tool["name"] == name)
            .and_then(|tool| tool.get("inputSchema"))
            .unwrap_or_else(|| panic!("missing schema for {name}"))
    };
    assert!(schema("search")["properties"]["source"]
        .get("default")
        .is_none());
    assert!(schema("search")["required"]
        .as_array()
        .is_some_and(|required| required.iter().any(|field| field == "source")));
    assert_eq!(
        schema("search")["properties"]["similar_to_chunk"]["type"],
        "object"
    );
    assert_eq!(
        schema("get_chunks")["properties"]["chunks"]["items"]["type"],
        "object"
    );
    assert_eq!(schema("get_asset")["properties"]["asset"]["type"], "object");
    assert_eq!(
        schema("get_asset")["properties"]["asset"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema("get_doc_anchors")["properties"]["document"]["type"],
        "object"
    );
    assert_eq!(
        schema("get_definition")["properties"]["context_document"]["type"],
        "object"
    );
    assert_eq!(
        schema("get_definition")["properties"]["source"]["type"],
        "string"
    );
    assert!(
        schema("get_definition")["properties"]["source"]
            .get("enum")
            .is_none(),
        "registered source validation is enforced by the runtime without repeating the registry in every tool schema"
    );
    assert!(listed_tools
        .iter()
        .all(|tool| tool["inputSchema"]["additionalProperties"] == false));
    assert!(listed_tools.iter().all(|tool| {
        tool["annotations"]["readOnlyHint"] == true
            && tool["annotations"]["destructiveHint"] == false
            && tool["annotations"]["idempotentHint"] == true
    }));
    assert_eq!(
        listed_tools
            .iter()
            .find(|tool| tool["name"] == "fetch")
            .expect("fetch descriptor")["annotations"]["openWorldHint"],
        true
    );
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

    let trailing_slash = client.post(format!("{}/", server.url)).body("{}").send()?;
    assert_eq!(trailing_slash.status().as_u16(), 404);

    let query = client
        .post(format!("{}?token=forbidden", server.url))
        .body("{}")
        .send()?;
    assert_eq!(query.status().as_u16(), 404);

    let bad_method = client.get(&server.url).send()?;
    assert_eq!(bad_method.status().as_u16(), 405);

    drop(server);
    Ok(())
}

#[test]
fn streamable_http_notifications_batches_and_errors_conform() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;

    let notification = post_raw(
        &server.url,
        r#"{"jsonrpc":"2.0","method":"ping"}"#,
        Some("application/json; charset=utf-8"),
    )?;
    assert_eq!(notification.status, 202);
    assert!(notification.body.is_empty());

    let client_response = post_raw(
        &server.url,
        r#"{"jsonrpc":"2.0","id":"server-request","result":{}}"#,
        Some("application/json"),
    )?;
    assert_eq!(client_response.status, 202);
    assert!(client_response.body.is_empty());

    let batch = post_raw(
        &server.url,
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"ping"}]"#,
        Some("application/json"),
    )?;
    assert_eq!(batch.status, 200);
    assert_eq!(batch.content_type.as_deref(), Some("application/json"));
    let batch: Value = serde_json::from_str(&batch.body)?;
    assert_eq!(batch["error"]["code"], -32600);

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

    for (id, name, arguments) in [
        (
            10,
            "search",
            json!({"query": "tax", "similar_to_chunk_id": 1}),
        ),
        (
            11,
            "search",
            json!({
                "query": "tax",
                "similar_to": {
                    "generation": "test-generation",
                    "source": "ato",
                    "chunk_id": 1
                }
            }),
        ),
        (12, "get_chunks", json!({"chunk_ids": [1]})),
        (13, "get_asset", json!({"asset_ref": "ato-image://DOC/0"})),
        (14, "get_doc_anchors", json!({"doc_id": "PAC/1"})),
        (
            15,
            "get_definition",
            json!({"term": "car", "context_doc_id": "PAC/1"}),
        ),
        (16, "fetch", json!({"uri": "ato:PAC/1"})),
        (17, "fetch", json!({"uri": "legal://ato/PAC/1"})),
    ] {
        let response = post(
            &server.url,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }),
        )?;
        assert_eq!(
            response["error"]["code"], -32602,
            "alternate identity shape unexpectedly accepted: {response}"
        );
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
fn enforces_streamable_http_headers_origins_and_health() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server(dir.path())?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    let missing_accept = client
        .post(&server.url)
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .body(body)
        .send()?;
    assert_eq!(missing_accept.status().as_u16(), 406);

    let wrong_protocol = client
        .post(&server.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2024-11-05")
        .body(body)
        .send()?;
    assert_eq!(wrong_protocol.status().as_u16(), 400);
    let wrong_protocol: Value = serde_json::from_str(&wrong_protocol.text()?)?;
    assert_eq!(wrong_protocol["error"]["code"], -32600);

    let forbidden_origin = client
        .post(&server.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .header("origin", "https://attacker.example")
        .body(body)
        .send()?;
    assert_eq!(forbidden_origin.status().as_u16(), 403);

    let base = server.url.trim_end_matches("/mcp");
    let live = client.get(format!("{base}/livez")).send()?;
    assert_eq!(live.status().as_u16(), 200);
    assert_eq!(
        live.headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        live.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&live.text()?)?,
        json!({"status": "ok", "generation": null})
    );

    let ready = client.get(format!("{base}/readyz")).send()?;
    assert_eq!(ready.status().as_u16(), 503);
    assert_eq!(
        ready
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&ready.text()?)?,
        json!({"status": "not-ready", "generation": null})
    );
    assert_eq!(
        client
            .post(format!("{base}/readyz"))
            .send()?
            .status()
            .as_u16(),
        405
    );
    assert_eq!(
        client
            .get(format!("{base}/readyz?probe=1"))
            .send()?
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        client
            .get(format!("{base}/readyz/"))
            .send()?
            .status()
            .as_u16(),
        404
    );
    Ok(())
}

#[test]
fn health_routes_bypass_a_saturated_worker() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server_with_auth_and_workers(dir.path(), TestAuth::Disabled, Some(1))?;
    let authority = server
        .url
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix("/mcp"))
        .ok_or_else(|| anyhow!("test server URL is malformed"))?;
    let mut stalled = BufReader::new(TcpStream::connect(authority)?);
    stalled
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    stalled.get_mut().write_all(
        format!(
            "POST /mcp HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-06-18\r\nContent-Length: 4096\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    stalled.get_mut().flush()?;
    let mut status = String::new();
    stalled.read_line(&mut status)?;
    assert_eq!(status.trim_end(), "HTTP/1.1 100 Continue");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = server.url.trim_end_matches("/mcp");
    assert_eq!(
        client
            .get(format!("{base}/livez"))
            .send()?
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()?
            .status()
            .as_u16(),
        503
    );
    assert_eq!(
        client
            .post(format!("{base}/readyz"))
            .send()?
            .status()
            .as_u16(),
        405
    );

    stalled.get_mut().shutdown(Shutdown::Both)?;
    Ok(())
}

#[test]
fn entra_mode_publishes_resource_metadata_and_challenges_every_mcp_request() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server_with_auth(dir.path(), TestAuth::Entra)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = server.url.trim_end_matches("/mcp");

    let metadata_response = client
        .get(format!("{base}/.well-known/oauth-protected-resource/mcp"))
        .send()?;
    assert_eq!(metadata_response.status().as_u16(), 200);
    assert_eq!(
        metadata_response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let metadata: Value = serde_json::from_str(&metadata_response.text()?)?;
    assert_eq!(metadata["resource"], "https://legal.example/mcp");
    assert_eq!(
        metadata["authorization_servers"][0],
        "https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/v2.0"
    );
    let origin_metadata = client
        .get(format!("{base}/.well-known/oauth-protected-resource"))
        .send()?;
    assert_eq!(origin_metadata.status().as_u16(), 404);

    let unauthorized_get = client.get(&server.url).send()?;
    assert_eq!(unauthorized_get.status().as_u16(), 401);
    assert!(unauthorized_get.headers().contains_key("www-authenticate"));

    let unauthorized = client
        .post(&server.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()?;
    assert_eq!(unauthorized.status().as_u16(), 401);
    let challenge = unauthorized
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .expect("Bearer challenge");
    assert!(challenge.contains(
        "resource_metadata=\"https://legal.example/.well-known/oauth-protected-resource/mcp\""
    ));
    assert!(challenge.contains("scope=\"api://33333333-3333-3333-3333-333333333333/legal.read\""));

    let malformed = client
        .post(&server.url)
        .header("authorization", "Bearer not-a-jwt")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()?;
    assert_eq!(malformed.status().as_u16(), 401);
    assert!(malformed
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("error=\"invalid_token\"")));

    assert_eq!(
        client
            .get(format!("{base}/livez"))
            .send()?
            .status()
            .as_u16(),
        200
    );
    Ok(())
}

#[test]
fn hosted_mode_refuses_disabled_authentication() -> Result<()> {
    let dir = tempdir()?;
    let output = Command::new(bin_path())
        .args([
            "serve",
            "--port",
            &pick_free_port()?.to_string(),
            "--require-http-auth",
        ])
        .env("LEGAL_MCP_DATA_DIR", dir.path())
        .env("LEGAL_MCP_HTTP_AUTH", "disabled")
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refuses to start"));
    Ok(())
}

#[test]
fn container_network_scope_requires_hosted_auth_guard() -> Result<()> {
    let dir = tempdir()?;
    let output = Command::new(bin_path())
        .args(["serve", "--port", "0", "--network-scope", "container"])
        .env("LEGAL_MCP_DATA_DIR", dir.path())
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --require-http-auth"));
    Ok(())
}

#[test]
fn api_key_authentication_rejects_ambiguity_and_supports_rotation_identity() -> Result<()> {
    let dir = tempdir()?;
    let server = start_server_with_auth(dir.path(), TestAuth::ApiKey)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let payload =
        serde_json::to_string(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))?;
    let request = || {
        client
            .post(&server.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(payload.clone())
    };

    let missing = request().send()?;
    assert_eq!(missing.status().as_u16(), 401);
    assert!(missing
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("ApiKey realm=")));

    let allowed = request().header("x-api-key", API_KEY).send()?;
    assert_eq!(allowed.status().as_u16(), 200);

    let invalid = request()
        .header(
            "x-api-key",
            "automation.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
        )
        .send()?;
    assert_eq!(invalid.status().as_u16(), 401);

    let ambiguous = request()
        .header("x-api-key", API_KEY)
        .header("authorization", "Bearer not-a-token")
        .send()?;
    assert_eq!(ambiguous.status().as_u16(), 401);

    let duplicate = request()
        .header("x-api-key", API_KEY)
        .header("x-api-key", API_KEY)
        .send()?;
    assert_eq!(duplicate.status().as_u16(), 400);
    Ok(())
}

#[cfg(unix)]
#[test]
fn sigterm_drains_workers_and_removes_endpoint_state() -> Result<()> {
    let dir = tempdir()?;
    let mut server = start_server(dir.path())?;
    let result = unsafe { libc::kill(server.child.id() as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let status = server.child.wait()?;
    assert!(status.success(), "graceful server exit was {status}");
    assert!(!dir.path().join("state/http.json").exists());
    Ok(())
}
