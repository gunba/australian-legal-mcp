//! Paths and file locks under the user's data dir.

use crate::APP_NAME;
use anyhow::{anyhow, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

/// Default port the HTTP MCP server binds when the user doesn't pass
/// `--port`. The plugin's `.mcp.json` hardcodes this URL so the agent
/// can reach the server without any per-install config file.
pub(crate) const DEFAULT_HTTP_PORT: u16 = 51234;

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
    // fs2::FileExt gives the update/install path a cross-platform advisory lock.
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
