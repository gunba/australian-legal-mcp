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

pub(crate) fn server_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("SERVER_LOCK"))
}

pub(crate) fn http_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("http.json"))
}

pub(crate) fn server_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("server.log"))
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
    // [UM-02] Single-writer guard around update/install: cross-platform
    // advisory lock via fs2::FileExt on the app LOCK file.
    file.lock_exclusive()?;
    Ok(file)
}

pub(crate) fn server_lock_file() -> Result<File> {
    let path = server_lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    Ok(file)
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

/// Locate the `.mcp.json` file Claude Code will read for the installed plugin.
///
/// Directory marketplaces are special: Claude keeps an install record in
/// `installed_plugins.json`, but loads plugin components from the source
/// directory recorded in `known_marketplaces.json`. In that case the source
/// `.mcp.json` is the file that must be rewritten.
pub(crate) fn locate_plugin_mcp_json() -> Result<Option<PathBuf>> {
    let manifest = match claude_plugins_manifest_path()? {
        Some(p) => p,
        None => return Ok(None),
    };
    if !manifest.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", manifest.display()))?;
    let known_marketplaces = read_claude_known_marketplaces()?;
    Ok(plugin_mcp_json_from_manifests(
        &value,
        known_marketplaces.as_ref(),
    ))
}

fn plugin_mcp_json_from_manifests(
    installed_plugins: &JsonValue,
    known_marketplaces: Option<&JsonValue>,
) -> Option<PathBuf> {
    let plugins = installed_plugins
        .get("plugins")
        .and_then(|v| v.as_object())?;
    // Entries are keyed like `ato-mcp@<marketplace>`; the value is an array of
    // install records. Take the newest install of any entry whose key starts
    // with `ato-mcp@`.
    let mut newest: Option<(String, String, PathBuf)> = None;
    for (key, installs) in plugins {
        let Some(marketplace) = key.strip_prefix("ato-mcp@") else {
            continue;
        };
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
            let candidate = (
                updated.to_string(),
                marketplace.to_string(),
                PathBuf::from(path),
            );
            if newest
                .as_ref()
                .map(|(prev, _, _)| candidate.0 > *prev)
                .unwrap_or(true)
            {
                newest = Some(candidate);
            }
        }
    }
    let (_, marketplace, install_path) = newest?;
    if let Some(source_mcp_json) = directory_marketplace_mcp_json(known_marketplaces, &marketplace)
    {
        return Some(source_mcp_json);
    }
    let mcp_json = install_path.join(".mcp.json");
    if mcp_json.exists() {
        Some(mcp_json)
    } else {
        None
    }
}

/// Path to Claude Code's `installed_plugins.json` registry. Honours the
/// `CLAUDE_HOME` env var when set, otherwise falls back to `~/.claude/`.
fn claude_plugins_manifest_path() -> Result<Option<PathBuf>> {
    if let Ok(home) = std::env::var("CLAUDE_HOME") {
        return Ok(Some(
            PathBuf::from(home)
                .join("plugins")
                .join("installed_plugins.json"),
        ));
    }
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    Ok(Some(
        home.join(".claude")
            .join("plugins")
            .join("installed_plugins.json"),
    ))
}

fn read_claude_known_marketplaces() -> Result<Option<JsonValue>> {
    let Some(path) = claude_known_marketplaces_path()? else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(value))
}

fn claude_known_marketplaces_path() -> Result<Option<PathBuf>> {
    if let Ok(home) = std::env::var("CLAUDE_HOME") {
        return Ok(Some(
            PathBuf::from(home)
                .join("plugins")
                .join("known_marketplaces.json"),
        ));
    }
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    Ok(Some(
        home.join(".claude")
            .join("plugins")
            .join("known_marketplaces.json"),
    ))
}

fn directory_marketplace_mcp_json(
    known_marketplaces: Option<&JsonValue>,
    marketplace: &str,
) -> Option<PathBuf> {
    let entry = known_marketplaces?.get(marketplace)?;
    let source = entry.get("source")?;
    if source.get("source").and_then(|v| v.as_str()) != Some("directory") {
        return None;
    }
    let path = source.get("path").and_then(|v| v.as_str())?;
    let mcp_json = PathBuf::from(path).join(".mcp.json");
    if mcp_json.exists() {
        Some(mcp_json)
    } else {
        None
    }
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
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let entry = value
        .get_mut("ato")
        .ok_or_else(|| anyhow!("{} has no `ato` entry", path.display()))?;
    if entry.get("command").is_some() {
        return Ok(false);
    }
    let Some(current_url) = entry.get("url").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
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
    let raw =
        fs::read_to_string(&mcp_json).with_context(|| format!("reading {}", mcp_json.display()))?;
    let value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", mcp_json.display()))?;
    let entry = value.get("ato");
    if entry.and_then(|v| v.get("command")).is_some() {
        return Ok(PortChoice::Cli(pick_free_port()?));
    }
    let url = entry
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if url.is_empty() {
        return Ok(PortChoice::Cli(pick_free_port()?));
    }
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

    #[test]
    fn update_plugin_mcp_json_url_ignores_command_entry() {
        let root = std::env::temp_dir().join(format!(
            "ato-mcp-config-test-{}-command-entry",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".mcp.json");
        fs::write(
            &path,
            r#"{
  "ato": {
    "command": "ato-mcp",
    "args": ["mcp"],
    "url": "http://127.0.0.1:9/mcp"
  }
}
"#,
        )
        .unwrap();

        assert!(!update_plugin_mcp_json_url(&path, "http://127.0.0.1:12345/mcp").unwrap());
        let value: JsonValue = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["ato"]["url"].as_str(), Some("http://127.0.0.1:9/mcp"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_marketplace_mcp_json_wins_over_cache_install() {
        let root = std::env::temp_dir().join(format!(
            "ato-mcp-config-test-{}-directory",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let source_dir = root.join("source");
        let cache_dir = root.join("cache");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(source_dir.join(".mcp.json"), "{}\n").unwrap();
        fs::write(cache_dir.join(".mcp.json"), "{}\n").unwrap();

        let installed = serde_json::json!({
            "plugins": {
                "ato-mcp@ato-mcp": [{
                    "installPath": cache_dir,
                    "lastUpdated": "2026-05-24T10:42:04Z"
                }]
            }
        });
        let known = serde_json::json!({
            "ato-mcp": {
                "source": {
                    "source": "directory",
                    "path": source_dir
                }
            }
        });

        assert_eq!(
            plugin_mcp_json_from_manifests(&installed, Some(&known)),
            Some(root.join("source").join(".mcp.json"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn plugin_mcp_json_falls_back_to_cache_install() {
        let root =
            std::env::temp_dir().join(format!("ato-mcp-config-test-{}-cache", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join(".mcp.json"), "{}\n").unwrap();

        let installed = serde_json::json!({
            "plugins": {
                "ato-mcp@ato-mcp": [{
                    "installPath": cache_dir,
                    "lastUpdated": "2026-05-24T10:42:04Z"
                }]
            }
        });

        assert_eq!(
            plugin_mcp_json_from_manifests(&installed, None),
            Some(root.join("cache").join(".mcp.json"))
        );
        let _ = fs::remove_dir_all(root);
    }
}
