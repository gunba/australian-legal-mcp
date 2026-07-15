use crate::adaptive_http::SOURCE_WORKER_CEILING;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client::connect_with_config, protocol::WebSocketConfig, Message, WebSocket};
use url::Url;

const CHROME_START_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PENDING_FETCH_EVENTS: usize = 64;
const CDP_MESSAGE_ENVELOPE_BYTES: u64 = 1024 * 1024;
const CHROME_USER_AGENT_SUFFIX: &str = " Safari/537.36";
const BLOCKED_RESOURCE_PATTERNS: &[&str] = &[
    "*.css",
    "*.png",
    "*.jpg",
    "*.jpeg",
    "*.gif",
    "*.svg",
    "*.webp",
    "*.woff",
    "*.woff2",
    "*google-analytics.com*",
    "*googletagmanager.com*",
];

#[derive(Debug)]
pub(crate) struct BrowserResponse {
    pub(crate) final_url: Url,
    pub(crate) status: u16,
    pub(crate) content_type: Option<String>,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct BrowserHttpTransport {
    child: Mutex<Child>,
    _profile: TempDir,
    debugger: Client,
    port: u16,
    websocket_message_limit: usize,
    tabs: Mutex<Vec<ChromeTab>>,
    available: Condvar,
}

impl BrowserHttpTransport {
    pub(crate) fn new(max_response_bytes: u64) -> Result<Self> {
        let executable = chrome_executable()?;
        let version = chrome_version(&executable)?;
        let user_agent = chrome_user_agent(&version);
        let profile = tempfile::tempdir().context("creating Chrome source-fetch profile")?;
        let mut child = Command::new(&executable)
            .args([
                "--headless=new",
                "--disable-blink-features=AutomationControlled",
                "--lang=en-GB",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--remote-allow-origins=*",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-default-apps",
                "--disable-extensions",
                "--disable-sync",
                "--disable-gpu",
                "--metrics-recording-only",
                "--mute-audio",
            ])
            .arg(format!("--user-agent={user_agent}"))
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting browser transport with {}", executable.display()))?;
        let port = match wait_for_debugger_port(profile.path(), &mut child) {
            Ok(port) => port,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let debugger = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .context("building Chrome debugger client")?;
        let websocket_message_limit = cdp_message_limit(max_response_bytes)?;
        let mut tabs = Vec::with_capacity(SOURCE_WORKER_CEILING);
        for _ in 0..SOURCE_WORKER_CEILING {
            tabs.push(ChromeTab::new(&debugger, port, websocket_message_limit)?);
        }
        close_unpooled_chrome_tabs(&debugger, port, &tabs)?;
        Ok(Self {
            child: Mutex::new(child),
            _profile: profile,
            debugger,
            port,
            websocket_message_limit,
            tabs: Mutex::new(tabs),
            available: Condvar::new(),
        })
    }

    pub(crate) fn get(&self, url: &Url, limit: u64) -> Result<BrowserResponse> {
        let mut tabs = self
            .tabs
            .lock()
            .map_err(|_| anyhow!("browser transport tab lock is poisoned"))?;
        while tabs.is_empty() {
            tabs = self
                .available
                .wait(tabs)
                .map_err(|_| anyhow!("browser transport tab lock is poisoned"))?;
        }
        let mut tab = tabs
            .pop()
            .ok_or_else(|| anyhow!("browser tab pool became empty after waiting"))?;
        drop(tabs);
        let result = tab.get(url, limit);
        if let Err(error) = &result {
            let failed_target_id = tab.target_id.clone();
            let request_error = format!("{error:#}");
            let mut replacement_error = None;
            let mut close_error = None;
            match ChromeTab::new(&self.debugger, self.port, self.websocket_message_limit) {
                Ok(replacement) => {
                    close_error = close_chrome_tab(&self.debugger, self.port, &failed_target_id)
                        .err()
                        .map(|error| format!("{error:#}"));
                    tab = replacement;
                }
                Err(error) => replacement_error = Some(format!("{error:#}")),
            }
            eprintln!(
                "legal-mcp browser-http-audit {}",
                json!({
                    "at": Utc::now().to_rfc3339(),
                    "source": "federal-court",
                    "url": url.as_str(),
                    "status": Value::Null,
                    "bytes": 0,
                    "outcome": if replacement_error.is_none() {
                        "tab_replaced"
                    } else {
                        "tab_replacement_failed"
                    },
                    "target_id": failed_target_id,
                    "error": request_error,
                    "close_error": close_error,
                    "replacement_error": replacement_error,
                })
            );
        }
        let mut tabs = self
            .tabs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tabs.push(tab);
        self.available.notify_one();
        result
    }
}

impl Drop for BrowserHttpTransport {
    fn drop(&mut self) {
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn close_chrome_tab(debugger: &Client, port: u16, target_id: &str) -> Result<()> {
    let endpoint = format!("http://127.0.0.1:{port}/json/close/{target_id}");
    debugger
        .get(endpoint)
        .send()
        .context("closing failed Chrome source-fetch tab")?
        .error_for_status()
        .context("Chrome rejected source-fetch tab closure")?;
    Ok(())
}

fn close_unpooled_chrome_tabs(debugger: &Client, port: u16, tabs: &[ChromeTab]) -> Result<()> {
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let response = debugger
        .get(endpoint)
        .send()
        .context("listing Chrome source-fetch tabs")?
        .error_for_status()
        .context("Chrome rejected source-fetch tab listing")?;
    let descriptors: Vec<Value> = serde_json::from_slice(
        &response
            .bytes()
            .context("reading Chrome source-fetch tab list")?,
    )
    .context("decoding Chrome source-fetch tab list")?;
    let pooled = tabs
        .iter()
        .map(|tab| tab.target_id.as_str())
        .collect::<BTreeSet<_>>();
    for descriptor in descriptors {
        if descriptor.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        let target_id = descriptor
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Chrome page descriptor omitted id"))?;
        if !pooled.contains(target_id) {
            close_chrome_tab(debugger, port, target_id)?;
        }
    }
    Ok(())
}

fn cdp_message_limit(max_response_bytes: u64) -> Result<usize> {
    let bytes = max_response_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(CDP_MESSAGE_ENVELOPE_BYTES))
        .ok_or_else(|| anyhow!("browser source response limit is too large"))?;
    usize::try_from(bytes).context("browser source response limit exceeds this platform")
}

struct ChromeTab {
    target_id: String,
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_command_id: u64,
    pending_fetch_events: VecDeque<Value>,
}

impl ChromeTab {
    fn new(debugger: &Client, port: u16, websocket_message_limit: usize) -> Result<Self> {
        let endpoint = format!("http://127.0.0.1:{port}/json/new?about:blank");
        let response = debugger
            .put(&endpoint)
            .send()
            .context("creating Chrome source-fetch tab")?
            .error_for_status()
            .context("Chrome rejected source-fetch tab creation")?;
        let descriptor: Value = serde_json::from_slice(
            &response
                .bytes()
                .context("reading Chrome source-fetch tab descriptor")?,
        )
        .context("decoding Chrome source-fetch tab descriptor")?;
        let target_id = descriptor
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Chrome tab descriptor omitted id"))?
            .to_owned();
        let websocket_url = descriptor
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Chrome tab descriptor omitted webSocketDebuggerUrl"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(websocket_message_limit))
            .max_frame_size(Some(websocket_message_limit));
        let (mut socket, _) = connect_with_config(websocket_url, Some(config), 0)
            .context("connecting to Chrome source-fetch tab")?;
        set_socket_timeout(&mut socket, BROWSER_REQUEST_TIMEOUT)?;
        let mut tab = Self {
            target_id,
            socket,
            next_command_id: 0,
            pending_fetch_events: VecDeque::new(),
        };
        tab.command("Network.enable", json!({}))?;
        tab.command("Page.enable", json!({}))?;
        tab.command(
            "Network.setBlockedURLs",
            json!({ "urls": BLOCKED_RESOURCE_PATTERNS }),
        )?;
        tab.command(
            "Fetch.enable",
            json!({
                "patterns": [
                    {
                        "urlPattern": "https://www.judgments.fedcourt.gov.au/*",
                        "resourceType": "Document",
                        "requestStage": "Response"
                    },
                    {
                        "urlPattern": "https://www.fedcourt.gov.au/file-store/*",
                        "resourceType": "Document",
                        "requestStage": "Response"
                    }
                ]
            }),
        )?;
        Ok(tab)
    }

    fn get(&mut self, url: &Url, limit: u64) -> Result<BrowserResponse> {
        let started = Instant::now();
        let frame_tree = self.command("Page.getFrameTree", json!({}))?;
        let frame_id = frame_tree
            .pointer("/result/frameTree/frame/id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("Chrome Page.getFrameTree response omitted the main frame id")
            })?;
        self.send_command("Page.navigate", json!({ "url": url.as_str() }))?;
        let mut expected_url = url.clone();
        let mut challenge = None;
        while started.elapsed() < BROWSER_REQUEST_TIMEOUT {
            let message = self.next_fetch_event()?;
            let parameters = message
                .get("params")
                .ok_or_else(|| anyhow!("Chrome Fetch.requestPaused event omitted params"))?;
            let Some(status) = parameters.get("responseStatusCode").and_then(Value::as_u64) else {
                continue;
            };
            let request_id = parameters
                .get("requestId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Chrome response event omitted requestId"))?;
            let final_url = parameters
                .pointer("/request/url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Chrome response event omitted request URL"))?;
            let event_url = Url::parse(final_url).context("parsing Chrome response URL")?;
            if parameters.get("frameId").and_then(Value::as_str) != Some(frame_id)
                || event_url != expected_url
            {
                self.command("Fetch.continueResponse", json!({ "requestId": request_id }))?;
                continue;
            }
            let headers = response_headers(parameters.get("responseHeaders"));
            if (300..400).contains(&status) {
                log_browser_response(final_url, status, 0);
                let location = headers
                    .get("location")
                    .ok_or_else(|| anyhow!("Chrome redirect response omitted Location"))?;
                expected_url = event_url
                    .join(location)
                    .context("resolving Chrome redirect Location")?;
                self.command("Fetch.continueResponse", json!({ "requestId": request_id }))?;
                continue;
            }
            let body_response =
                self.command("Fetch.getResponseBody", json!({ "requestId": request_id }))?;
            let body = decode_response_body(&body_response, limit)?;
            log_browser_response(final_url, status, body.len());
            self.command("Fetch.continueResponse", json!({ "requestId": request_id }))?;
            let response = BrowserResponse {
                final_url: event_url,
                status: u16::try_from(status).context("Chrome returned an invalid HTTP status")?,
                content_type: headers.get("content-type").map(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or(value)
                        .trim()
                        .to_ascii_lowercase()
                }),
                bytes: body,
            };
            if response.status == 403
                && headers
                    .get("cf-mitigated")
                    .is_some_and(|value| value.eq_ignore_ascii_case("challenge"))
            {
                challenge = Some(response);
                continue;
            }
            let _ = self.command("Page.stopLoading", json!({}));
            return Ok(response);
        }
        if let Some(response) = challenge {
            return Ok(response);
        }
        bail!("browser source request timed out after {BROWSER_REQUEST_TIMEOUT:?}")
    }

    fn command(&mut self, method: &str, parameters: Value) -> Result<Value> {
        let id = self.send_command(method, parameters)?;
        loop {
            let response = self.read_json()?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = response.get("error") {
                    bail!("Chrome {method} failed: {error}");
                }
                return Ok(response);
            }
            if response.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
                if self.pending_fetch_events.len() >= MAX_PENDING_FETCH_EVENTS {
                    bail!("Chrome pending Fetch event queue exceeded {MAX_PENDING_FETCH_EVENTS}");
                }
                self.pending_fetch_events.push_back(response);
            }
        }
    }

    fn next_fetch_event(&mut self) -> Result<Value> {
        if let Some(event) = self.pending_fetch_events.pop_front() {
            return Ok(event);
        }
        loop {
            let event = self.read_json()?;
            if event.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
                return Ok(event);
            }
        }
    }

    fn send_command(&mut self, method: &str, parameters: Value) -> Result<u64> {
        self.next_command_id += 1;
        let id = self.next_command_id;
        self.socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": parameters })
                    .to_string()
                    .into(),
            ))
            .with_context(|| format!("sending Chrome {method} command"))?;
        Ok(id)
    }

    fn read_json(&mut self) -> Result<Value> {
        loop {
            match self
                .socket
                .read()
                .context("reading Chrome debugger event")?
            {
                Message::Text(text) => {
                    return serde_json::from_str(text.as_ref())
                        .context("decoding Chrome debugger event");
                }
                Message::Binary(bytes) => {
                    return serde_json::from_slice(&bytes)
                        .context("decoding binary Chrome debugger event");
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .context("answering Chrome debugger ping")?;
                }
                Message::Close(frame) => bail!("Chrome debugger closed the tab: {frame:?}"),
                Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }
}

fn chrome_executable() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("LEGAL_MCP_CHROME") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        bail!("LEGAL_MCP_CHROME is not a file: {}", path.display());
    }
    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chrome.exe",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(path) = executable_on_path(name) {
            return Ok(path);
        }
    }
    for path in [
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ] {
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("Federal Court browser transport requires Google Chrome or Chromium on PATH")
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn chrome_version(executable: &Path) -> Result<String> {
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .with_context(|| format!("reading Chrome version from {}", executable.display()))?;
    if !output.status.success() {
        bail!("Chrome --version exited with {}", output.status);
    }
    let output = String::from_utf8(output.stdout).context("Chrome version was not UTF-8")?;
    parse_chrome_version(&output)
}

fn parse_chrome_version(output: &str) -> Result<String> {
    output
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Chrome --version did not contain a version number"))
}

fn chrome_user_agent(version: &str) -> String {
    let platform = if cfg!(target_os = "windows") {
        "Windows NT 10.0; Win64; x64"
    } else if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else {
        "X11; Linux x86_64"
    };
    format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{version}{CHROME_USER_AGENT_SUFFIX}"
    )
}

fn wait_for_debugger_port(profile: &Path, child: &mut Child) -> Result<u16> {
    let active_port = profile.join("DevToolsActivePort");
    let started = Instant::now();
    while started.elapsed() < CHROME_START_TIMEOUT {
        if let Some(status) = child
            .try_wait()
            .context("checking Chrome source-fetch process")?
        {
            bail!("Chrome source-fetch process exited during startup with {status}");
        }
        if let Ok(contents) = fs::read_to_string(&active_port) {
            return contents
                .lines()
                .next()
                .ok_or_else(|| anyhow!("Chrome DevToolsActivePort was empty"))?
                .parse::<u16>()
                .context("Chrome DevToolsActivePort contained an invalid port");
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("Chrome source-fetch process did not start within {CHROME_START_TIMEOUT:?}")
}

fn set_socket_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            stream
                .set_read_timeout(Some(timeout))
                .context("setting Chrome debugger read timeout")?;
            stream
                .set_write_timeout(Some(timeout))
                .context("setting Chrome debugger write timeout")?;
        }
        _ => bail!("Chrome debugger unexpectedly used TLS for a loopback WebSocket"),
    }
    Ok(())
}

fn response_headers(value: Option<&Value>) -> std::collections::BTreeMap<String, String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|header| {
            Some((
                header.get("name")?.as_str()?.to_ascii_lowercase(),
                header.get("value")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn decode_response_body(response: &Value, limit: u64) -> Result<Vec<u8>> {
    let result = response
        .get("result")
        .ok_or_else(|| anyhow!("Chrome Fetch.getResponseBody omitted result"))?;
    let body = result
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Chrome Fetch.getResponseBody omitted body"))?;
    if body.len() as u64 > limit.saturating_mul(2) {
        bail!("browser source response exceeds its {limit}-byte limit");
    }
    let bytes = if result
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .context("decoding Chrome response body")?
    } else {
        body.as_bytes().to_vec()
    };
    if bytes.len() as u64 > limit {
        bail!("browser source response exceeds its {limit}-byte limit");
    }
    Ok(bytes)
}

fn log_browser_response(url: &str, status: u64, bytes: usize) {
    eprintln!(
        "legal-mcp browser-http-audit {}",
        json!({
            "at": Utc::now().to_rfc3339(),
            "source": "federal-court",
            "url": url,
            "status": status,
            "bytes": bytes,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_version_parser_accepts_the_installed_format() -> Result<()> {
        let version = parse_chrome_version("Google Chrome 147.0.7727.55")?;
        assert!(version.split('.').all(|part| part.parse::<u32>().is_ok()));
        Ok(())
    }

    #[test]
    fn response_body_decoder_enforces_the_uncompressed_limit() -> Result<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"legal");
        let response = json!({
            "result": { "body": encoded, "base64Encoded": true }
        });
        assert_eq!(decode_response_body(&response, 5)?, b"legal");
        assert!(decode_response_body(&response, 4).is_err());
        Ok(())
    }

    #[test]
    fn cdp_message_limit_covers_encoded_body_and_protocol_envelope() -> Result<()> {
        assert_eq!(cdp_message_limit(8 * 1024 * 1024)?, 17 * 1024 * 1024);
        Ok(())
    }

    #[test]
    #[ignore = "requires Chrome and live Federal Court access"]
    fn live_browser_transport_returns_raw_html_and_pdf() -> Result<()> {
        let transport = BrowserHttpTransport::new(128 * 1024 * 1024)?;
        let html = transport.get(
            &Url::parse("https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/2022/2022fca1092")?,
            8 * 1024 * 1024,
        )?;
        assert_eq!(html.status, 200);
        assert!(html
            .bytes
            .windows(b"judgment_content".len())
            .any(|window| window == b"judgment_content"));
        let pdf = transport.get(
            &Url::parse("https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/1981/1981fca0206")?,
            8 * 1024 * 1024,
        )?;
        assert_eq!(pdf.status, 200);
        assert!(pdf.bytes.starts_with(b"%PDF"));
        let large_pdf = transport.get(
            &Url::parse(
                "https://www.judgments.fedcourt.gov.au/judgments/Judgments/fca/single/1986/1986fca0039",
            )?,
            128 * 1024 * 1024,
        )?;
        assert_eq!(large_pdf.status, 200);
        assert!(large_pdf.bytes.starts_with(b"%PDF"));
        assert!(large_pdf.bytes.len() > 12 * 1024 * 1024);
        Ok(())
    }
}
