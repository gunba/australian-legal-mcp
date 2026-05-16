//! Paths, HTTP config, and file locks under the user's data dir.

use crate::APP_NAME;
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
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

pub(crate) fn backups_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("backups");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn db_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("ato.db"))
}

pub(crate) fn installed_manifest_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("installed_manifest.json"))
}

pub(crate) fn http_config_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("http.json"))
}

pub(crate) fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LOCK"))
}

pub(crate) fn spawn_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("spawn.lock"))
}

pub(crate) fn daemon_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("daemon.log"))
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
    // [UM-02] fs2::FileExt gives the update/install path a cross-platform advisory lock.
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
    format!("{}/manifest.json", releases_url().trim_end_matches('/'))
}

pub(crate) fn releases_url() -> String {
    std::env::var("ATO_MCP_RELEASES_URL")
        .unwrap_or_else(|_| crate::DEFAULT_RELEASES_URL.to_string())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct HttpConfig {
    pub(crate) bind: String,
    pub(crate) port: u16,
}

impl HttpConfig {
    pub(crate) fn load() -> Result<Option<Self>> {
        let p = http_config_path()?;
        if !p.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        let cfg: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
        Ok(Some(cfg))
    }

    /// Return the existing config or pick a free port and persist a new one.
    /// Used by both the shim (auto-init on first run) and the daemon itself
    /// so the user never has to call `install-http` for the auto-managed
    /// path to work. Serialised against concurrent creators via the spawn
    /// lock so two parallel shims don't write conflicting ports.
    pub(crate) fn load_or_init() -> Result<Self> {
        if let Some(cfg) = Self::load()? {
            return Ok(cfg);
        }
        let lock_path = spawn_lock_path()?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        lock_file
            .lock_exclusive()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        if let Some(cfg) = Self::load()? {
            return Ok(cfg);
        }
        let cfg = Self {
            bind: "127.0.0.1".to_string(),
            port: pick_free_port()?,
        };
        cfg.save()?;
        drop(lock_file);
        Ok(cfg)
    }

    pub(crate) fn save(&self) -> Result<()> {
        let p = http_config_path()?;
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&p, raw).with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}:{}/mcp", self.bind, self.port)
    }
}

/// Bind 127.0.0.1:0, ask the OS for a free port in the ephemeral range,
/// release the socket, and return the chosen port. The port is not held
/// across the function so a tight race with another process can still claim
/// it; `serve()` then errors at bind time and the user can re-run install.
pub(crate) fn pick_free_port() -> Result<u16> {
    use std::net::TcpListener;
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1:0 to discover a free port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
