//! Paths, file locks, and plugin-side `.mcp.json` discovery under the user's
//! data dir.

use crate::APP_NAME;
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde_json::Value as JsonValue;
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

/// Bind 127.0.0.1:0, ask the OS for a free port, release the socket, and
/// return the chosen port. There's a tight race between discovery and the
/// next bind; the caller reports any error cleanly.
pub(crate) fn pick_free_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("binding 127.0.0.1:0 to discover a free port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Locate the plugin's installed `.mcp.json` by reading Claude Code's
/// `installed_plugins.json`. Returns `Ok(None)` when the file doesn't exist
/// or has no entry for this plugin — that's the "binary running outside any
/// plugin context" case, which is fine; the caller falls back to standalone
/// serve and the URL written to the plugin's `.mcp.json` simply isn't kept
/// in sync.
pub(crate) fn locate_plugin_mcp_json() -> Result<Option<PathBuf>> {
    let manifest = match claude_plugins_manifest_path()? {
        Some(p) => p,
        None => return Ok(None),
    };
    if !manifest.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let value: JsonValue = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", manifest.display()))?;
    let plugins = value.get("plugins").and_then(|v| v.as_object());
    let Some(plugins) = plugins else {
        return Ok(None);
    };
    // Entries are keyed like `ato-mcp@<marketplace>`; the value is an array of
    // install records. Take the newest install of any entry whose key starts
    // with `ato-mcp@`.
    let mut newest: Option<(String, PathBuf)> = None;
    for (key, installs) in plugins {
        if !key.starts_with("ato-mcp@") {
            continue;
        }
        let Some(records) = installs.as_array() else {
            continue;
        };
        for record in records {
            let Some(path) = record.get("installPath").and_then(|v| v.as_str()) else {
                continue;
            };
            let updated = record
                .get("lastUpdated")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let candidate = (updated.to_string(), PathBuf::from(path));
            if newest
                .as_ref()
                .map(|(prev, _)| candidate.0 > *prev)
                .unwrap_or(true)
            {
                newest = Some(candidate);
            }
        }
    }
    let Some((_, install_path)) = newest else {
        return Ok(None);
    };
    let mcp_json = install_path.join(".mcp.json");
    if mcp_json.exists() {
        Ok(Some(mcp_json))
    } else {
        Ok(None)
    }
}

/// Path to Claude Code's `installed_plugins.json` registry. Honours the
/// `CLAUDE_HOME` env var when set, otherwise falls back to `~/.claude/`.
fn claude_plugins_manifest_path() -> Result<Option<PathBuf>> {
    if let Ok(home) = std::env::var("CLAUDE_HOME") {
        return Ok(Some(
            PathBuf::from(home).join("plugins").join("installed_plugins.json"),
        ));
    }
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    Ok(Some(
        home.join(".claude").join("plugins").join("installed_plugins.json"),
    ))
}

/// Read the port out of an `http://host:port/...` URL string.
fn parse_url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host_port = rest.split_once('/').map(|(hp, _)| hp).unwrap_or(rest);
    let port = host_port.rsplit_once(':')?.1;
    port.parse().ok()
}

/// Update the `ato` MCP server entry in the plugin's `.mcp.json` to point at
/// the given URL. Used when `ato-mcp serve` had to pick a new port and the
/// plugin's existing URL is stale. Returns `true` when the file was actually
/// changed.
pub(crate) fn update_plugin_mcp_json_url(path: &std::path::Path, new_url: &str) -> Result<bool> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let entry = value
        .get_mut("ato")
        .ok_or_else(|| anyhow!("{} has no `ato` entry", path.display()))?;
    let current_url = entry.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if current_url == new_url {
        return Ok(false);
    }
    entry["url"] = JsonValue::String(new_url.to_string());
    let serialised = serde_json::to_string_pretty(&value)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{serialised}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(true)
}

/// Pick the port `ato-mcp serve` should bind. Precedence:
///   1. `--port` flag, if the caller supplied one.
///   2. The port in the plugin's installed `.mcp.json` (if it's bindable —
///      i.e. not the `:0` sentinel and not already in use).
///   3. A freshly-picked free port.
///
/// Returns the chosen port and the source of truth so the caller can decide
/// whether to rewrite the plugin's `.mcp.json`.
pub(crate) enum PortChoice {
    /// Port came from the `--port` flag. Caller shouldn't touch `.mcp.json`.
    Cli(u16),
    /// Port matched what `.mcp.json` already has; no rewrite needed.
    PluginUnchanged(u16),
    /// Port was freshly picked or differs from what was in `.mcp.json`; the
    /// caller should rewrite the plugin's `.mcp.json` so Claude Code picks
    /// up the new URL on its next start.
    PluginNeedsRewrite { port: u16, mcp_json: PathBuf },
}

pub(crate) fn resolve_serve_port(cli_override: Option<u16>) -> Result<PortChoice> {
    if let Some(port) = cli_override {
        return Ok(PortChoice::Cli(port));
    }
    let Some(mcp_json) = locate_plugin_mcp_json()? else {
        // Standalone serve (no plugin installed locally) — just pick a port.
        return Ok(PortChoice::Cli(pick_free_port()?));
    };
    let raw = fs::read_to_string(&mcp_json)
        .with_context(|| format!("reading {}", mcp_json.display()))?;
    let value: JsonValue = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", mcp_json.display()))?;
    let url = value
        .get("ato")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stored_port = parse_url_port(url);
    if let Some(port) = stored_port.filter(|p| *p != 0) {
        // Try to claim the stored port. If something else has it, fall
        // through to picking a fresh one.
        if port_is_bindable(port) {
            return Ok(PortChoice::PluginUnchanged(port));
        }
    }
    let port = pick_free_port()?;
    Ok(PortChoice::PluginNeedsRewrite { port, mcp_json })
}

fn port_is_bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Format the canonical MCP URL for a bound port.
pub(crate) fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_port_from_typical_shapes() {
        assert_eq!(parse_url_port("http://127.0.0.1:51234/mcp"), Some(51234));
        assert_eq!(parse_url_port("http://localhost:0/mcp"), Some(0));
        assert_eq!(parse_url_port("http://127.0.0.1/mcp"), None);
        assert_eq!(parse_url_port("not a url"), None);
    }
}
