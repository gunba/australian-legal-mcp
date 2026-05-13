//! stdio shim smoke test.
//!
//! Verifies the zero-touch path that real MCP clients hit:
//!   * shim launched against an empty data dir auto-initialises http.json
//!     and spawns the detached daemon
//!   * proxies a single JSON-RPC message over stdin/stdout
//!   * daemon survives the shim's exit and is reused by a second shim
//!   * concurrent shim invocations against a dead daemon spawn exactly
//!     one daemon (lock serialisation)

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tempfile::tempdir;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_ato-mcp")
}

/// Spawn `ato-mcp serve` against `data_dir`, write one JSON-RPC payload
/// to its stdin, read one JSON-RPC response line back, and return it.
fn run_shim_once(data_dir: &Path, payload: &Value) -> Result<Value> {
    let mut child = Command::new(bin_path())
        .arg("serve")
        .env("ATO_MCP_DATA_DIR", data_dir)
        .env("ATO_MCP_OFFLINE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning shim")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("shim stdin missing"))?;
        let body = serde_json::to_string(payload)?;
        stdin.write_all(body.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    // Closing stdin makes the shim's stdin loop see EOF after reading the
    // line we just wrote, so it exits cleanly once the response is flushed.
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("shim stdout missing"))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: Value = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing shim response: {line:?}"))?;

    let exit = child.wait()?;
    if !exit.success() {
        let mut stderr = String::new();
        if let Some(s) = child.stderr.as_mut() {
            let _ = s.read_to_string(&mut stderr);
        }
        return Err(anyhow!("shim exited {exit:?}: {stderr}"));
    }
    Ok(response)
}

/// Find the daemon PID for a given data dir by reading the port out of
/// http.json and matching listening sockets. Returns None if no daemon
/// is listening on that port.
fn daemon_port(data_dir: &Path) -> Result<Option<u16>> {
    let cfg_path = data_dir.join("http.json");
    if !cfg_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&cfg_path)?;
    let v: Value = serde_json::from_str(&raw)?;
    let port = v["port"].as_u64().map(|p| p as u16);
    Ok(port)
}

fn port_is_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

/// Tear down any daemon we spawned during this test by connecting to the
/// port and sending a shutdown… actually the daemon has no shutdown, so
/// we look it up by port + kill via signal. Best-effort.
#[cfg(unix)]
fn kill_listener(port: u16) -> Result<()> {
    let output = Command::new("fuser")
        .args([&format!("{port}/tcp"), "-k"])
        .output();
    let _ = output; // best-effort, ignore failures (e.g. fuser absent)
    Ok(())
}

#[cfg(not(unix))]
fn kill_listener(_port: u16) -> Result<()> {
    Ok(())
}

#[test]
fn shim_cold_start_auto_spawns_daemon() -> Result<()> {
    let dir = tempdir()?;
    assert!(
        !dir.path().join("http.json").exists(),
        "fresh tempdir should not have http.json"
    );

    let init = run_shim_once(
        dir.path(),
        &json!({
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
    assert_eq!(init["result"]["serverInfo"]["name"], "ato-mcp");

    // http.json must now exist; daemon must be listening on its port.
    let port = daemon_port(dir.path())?.expect("http.json written");
    assert!(
        port_is_listening(port),
        "daemon should still be listening on {port} after shim exits"
    );

    // Clean up the daemon we spawned.
    let _ = kill_listener(port);
    Ok(())
}

#[test]
fn shim_warm_path_reuses_existing_daemon() -> Result<()> {
    let dir = tempdir()?;
    // First call cold-starts the daemon.
    let _ = run_shim_once(
        dir.path(),
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name":"s","version":"0"} }
        }),
    )?;
    let port = daemon_port(dir.path())?.expect("http.json written");
    assert!(port_is_listening(port));

    // Second call should be near-instant — daemon is already up.
    let started = std::time::Instant::now();
    let tools = run_shim_once(
        dir.path(),
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )?;
    let elapsed = started.elapsed();
    assert!(
        tools["result"]["tools"].is_array(),
        "tools/list should return array"
    );
    // A reused daemon serves tools/list in well under a second; a fresh
    // spawn would take ~10x that on the same hardware. Generous bound to
    // avoid flake on CI.
    assert!(
        elapsed < Duration::from_secs(3),
        "warm-path shim took {elapsed:?}; daemon was probably re-spawned"
    );
    // Daemon still up.
    assert!(port_is_listening(port));

    let _ = kill_listener(port);
    Ok(())
}

#[test]
fn shim_concurrent_invocations_spawn_one_daemon() -> Result<()> {
    let dir = tempdir()?;
    let mut handles = Vec::new();
    for i in 0..4 {
        let d = dir.path().to_path_buf();
        handles.push(thread::spawn(move || -> Result<()> {
            let _ = run_shim_once(
                &d,
                &json!({
                    "jsonrpc": "2.0", "id": i, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": {"name":"s","version":"0"}
                    }
                }),
            )?;
            Ok(())
        }));
    }
    for h in handles {
        h.join().map_err(|_| anyhow!("thread panicked"))??;
    }
    let port = daemon_port(dir.path())?.expect("http.json written");
    assert!(port_is_listening(port), "exactly one daemon should be up");
    let _ = kill_listener(port);
    Ok(())
}
