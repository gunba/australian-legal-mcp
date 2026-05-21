//! Paths, file locks, and HTTP port discovery under the user's data dir.

use crate::APP_NAME;
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::net::TcpListener;
use std::path::PathBuf;

pub(crate) fn data_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("ATO_MCP_DATA_DIR") {
        let path = PathBuf::from(path);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let mut path =
        dirs::data_dir().ok_or_else(|| anyhow!("could not resolve user data directory"))?;
    path.push(APP_NAME);
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn live_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("live");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn staging_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("staging");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn db_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("ato.db"))
}

pub(crate) fn installed_manifest_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("installed_manifest.json"))
}

pub(crate) fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LOCK"))
}

pub(crate) fn http_config_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("http.json"))
}

pub(crate) fn model_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model_fp16.onnx"))
}

pub(crate) fn model_data_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model_fp16.onnx_data"))
}

pub(crate) fn tokenizer_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("tokenizer.json"))
}

pub(crate) fn model_marker_path() -> Result<PathBuf> {
    Ok(live_dir()?.join(".model.sha256"))
}

pub(crate) fn lock_file() -> Result<File> {
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

pub(crate) fn default_manifest_url() -> String {
    format!(
        "{}/manifest.json",
        crate::DEFAULT_RELEASES_URL.trim_end_matches('/')
    )
}

/// Persisted HTTP-server config. Written by `ato-mcp install` (and lazily by
/// the first `ato-mcp serve` when no file exists) so both the binary and the
/// plugin's `.mcp.json` can agree on a port without a shim or a hardcoded
/// number. The plugin's `.mcp.json` references `${env:ATO_MCP_PORT}`; `serve`
/// reads that env var first, falling back to this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HttpConfig {
    pub(crate) bind: String,
    pub(crate) port: u16,
}

impl HttpConfig {
    pub(crate) fn url(&self) -> String {
        format!("http://{}:{}/mcp", self.bind, self.port)
    }

    pub(crate) fn load() -> Result<Option<Self>> {
        let path = http_config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(cfg))
    }

    pub(crate) fn save(&self) -> Result<()> {
        let path = http_config_path()?;
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Bind 127.0.0.1:0, ask the OS for a free port, release the socket, and
/// return the chosen port. There's a tight race between the discover and
/// the next bind — `serve` reports the error cleanly and the user retries.
pub(crate) fn pick_free_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1:0 to discover a free port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Resolve the port `serve` should bind. Precedence: explicit `--port`
/// argument > `ATO_MCP_PORT` env var > persisted `<data_dir>/http.json` >
/// freshly-picked free port (persisted on first run). The picked port is
/// not exposed back to the plugin's `.mcp.json` — the user's environment
/// (or `ato-mcp install --port <n>`) controls that side.
pub(crate) fn resolve_serve_port(cli_override: Option<u16>) -> Result<u16> {
    if let Some(port) = cli_override {
        return Ok(port);
    }
    if let Ok(value) = std::env::var("ATO_MCP_PORT") {
        let port: u16 = value
            .parse()
            .with_context(|| format!("ATO_MCP_PORT env var must be a u16; got `{value}`"))?;
        return Ok(port);
    }
    if let Some(cfg) = HttpConfig::load()? {
        return Ok(cfg.port);
    }
    let port = pick_free_port()?;
    HttpConfig {
        bind: "127.0.0.1".to_string(),
        port,
    }
    .save()?;
    Ok(port)
}
