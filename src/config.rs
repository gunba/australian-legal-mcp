//! Paths, file locks, and plugin-side `.mcp.json` discovery under the user's
//! data dir.

use crate::legal_source::SourceId;
use crate::APP_NAME;
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde_json::Value as JsonValue;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MCP_SERVER_CONFIG_NAME: &str = "australian-legal";
pub(crate) const LEGAL_DB_FILENAME: &str = "legal.db";
pub(crate) const GENERATION_MANIFEST_FILENAME: &str = "generation.json";

pub(crate) fn data_dir() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("LEGAL_MCP_DATA_DIR") {
        let value = value
            .to_str()
            .ok_or_else(|| anyhow!("LEGAL_MCP_DATA_DIR must contain valid Unicode"))?;
        if value.trim().is_empty() {
            return Err(anyhow!(
                "LEGAL_MCP_DATA_DIR must not be empty or whitespace"
            ));
        }
        let path = PathBuf::from(value);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let mut path =
        dirs::data_dir().ok_or_else(|| anyhow!("could not resolve user data directory"))?;
    path.push(APP_NAME);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn validate_generation_key(key: &str) -> Result<()> {
    if key.len() != 64
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(anyhow!("active corpus generation key is malformed"));
    }
    Ok(())
}

pub(crate) fn generations_dir() -> Result<PathBuf> {
    let path = data_dir()?.join("generations");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub(crate) fn generation_dir(key: &str) -> Result<PathBuf> {
    validate_generation_key(key)?;
    Ok(generations_dir()?.join(key))
}

fn require_real_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {description} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "{description} must be a real non-symlink directory: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn active_generation_key() -> Result<Option<String>> {
    let path = data_dir()?.join("active-generation");
    if !path.exists() {
        return Ok(None);
    }
    let key = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    validate_generation_key(&key)?;
    let generation = generation_dir(&key)?;
    if !generation.exists() {
        return Err(anyhow!(
            "active corpus generation {key} is missing; activate a valid local generation"
        ));
    }
    require_real_directory(&generation, "active corpus generation")?;
    Ok(Some(key))
}

pub(crate) fn activate_generation(key: &str) -> Result<()> {
    validate_generation_key(key)?;
    let generation = generation_dir(key)?;
    if !generation.exists() {
        return Err(anyhow!(
            "cannot activate missing corpus generation {}",
            generation.display()
        ));
    }
    require_real_directory(&generation, "corpus generation")?;
    atomic_write(&data_dir()?.join("active-generation"), key.as_bytes())
}

pub(crate) fn live_dir() -> Result<PathBuf> {
    let key = active_generation_key()?
        .ok_or_else(|| anyhow!("no active legal corpus generation; run `legal-mcp activate`"))?;
    generation_dir(&key)
}

pub(crate) fn db_path() -> Result<PathBuf> {
    Ok(live_dir()?.join(LEGAL_DB_FILENAME))
}

pub(crate) fn ann_path(source_id: &SourceId) -> Result<PathBuf> {
    Ok(live_dir()?.join(crate::ann::sidecar_relative_path(source_id)))
}

pub(crate) fn lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LOCK"))
}

pub(crate) fn server_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("SERVER_LOCK"))
}

pub(crate) fn lifecycle_lock_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("LIFECYCLE_LOCK"))
}

pub(crate) fn http_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("http.json"))
}

pub(crate) fn server_log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("server.log"))
}

pub(crate) fn model_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("model.onnx"))
}

pub(crate) fn tokenizer_path() -> Result<PathBuf> {
    Ok(live_dir()?.join("tokenizer.json"))
}

pub(crate) fn lock_file() -> Result<File> {
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    // Single-writer guard around update/install: cross-platform
    // advisory lock via fs2::FileExt on the app LOCK file.
    file.lock_exclusive()?;
    Ok(file)
}

pub(crate) fn corpus_read_lock() -> Result<File> {
    let path = lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::lock_shared(&file)?;
    Ok(file)
}

pub(crate) fn lifecycle_lock_file() -> Result<File> {
    let path = lifecycle_lock_path()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
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
    // Entries are keyed like `australian-legal-mcp@<marketplace>`; the value is
    // an array of install records. Take the newest install for this plugin.
    let mut newest: Option<(String, String, PathBuf)> = None;
    for (key, installs) in plugins {
        let Some(marketplace) = key.strip_prefix("australian-legal-mcp@") else {
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
    let parsed = Url::parse(url).ok()?;
    let port = parsed.port()?;
    (parsed.as_str() == server_url(port)).then_some(port)
}

fn mcp_server_entry(value: &JsonValue) -> Option<&JsonValue> {
    value
        .get("mcpServers")
        .and_then(|servers| servers.get(MCP_SERVER_CONFIG_NAME))
}

fn mcp_server_entry_mut(value: &mut JsonValue) -> Option<&mut JsonValue> {
    value
        .get_mut("mcpServers")
        .and_then(|servers| servers.get_mut(MCP_SERVER_CONFIG_NAME))
}

/// Update the `australian-legal` server entry in the plugin's `.mcp.json` to
/// point at the given URL. Used when `legal-mcp serve` had to pick a new
/// port and the plugin's existing URL is stale. Returns `true` when the file
/// was actually changed.
pub(crate) fn update_plugin_mcp_json_url(path: &std::path::Path, new_url: &str) -> Result<bool> {
    parse_url_port(new_url).ok_or_else(|| {
        anyhow!("MCP endpoint must be a canonical loopback URL like http://127.0.0.1:<port>/mcp")
    })?;
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: JsonValue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let entry = mcp_server_entry_mut(&mut value)
        .ok_or_else(|| anyhow!("{} has no `{MCP_SERVER_CONFIG_NAME}` entry", path.display()))?;
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
    let serialised = format!("{}\n", serde_json::to_string_pretty(&value)?);
    atomic_write(path, serialised.as_bytes())?;
    Ok(true)
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;

    let mut last_error = None;
    for _ in 0..100 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("australian-legal-mcp-state");
        let temp = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(err);
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("creating {}", temp.display())),
        };

        let result = (|| -> Result<()> {
            if let Ok(metadata) = fs::metadata(path) {
                file.set_permissions(metadata.permissions())?;
            }
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temp, path)?;
            sync_parent_directory(parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result.with_context(|| format!("atomically replacing {}", path.display()));
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "temporary file collision")
    }))
    .with_context(|| format!("creating a temporary file beside {}", path.display()))
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    let existing: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

/// Pick the port `legal-mcp serve` should bind. Precedence:
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
    let entry = mcp_server_entry(&value);
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
        assert_eq!(parse_url_port("http://127.0.0.1:0/mcp"), Some(0));
        assert_eq!(parse_url_port("http://localhost:51234/mcp"), None);
        assert_eq!(parse_url_port("http://192.0.2.1:51234/mcp"), None);
        assert_eq!(parse_url_port("http://127.0.0.1:51234/other"), None);
        assert_eq!(parse_url_port("https://127.0.0.1:51234/mcp"), None);
        assert_eq!(parse_url_port("http://127.0.0.1/mcp"), None);
        assert_eq!(parse_url_port("not a url"), None);
    }

    #[test]
    fn data_dir_rejects_empty_or_whitespace_override() {
        for value in ["", " ", "\t\n"] {
            let _environment =
                crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", std::ffi::OsStr::new(value))]);
            let error = data_dir().unwrap_err();
            assert!(error
                .to_string()
                .contains("must not be empty or whitespace"));
        }
    }

    #[test]
    fn generation_paths_require_one_strict_active_generation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _environment =
            crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", root.path().as_os_str())]);

        assert!(live_dir().is_err());
        assert!(!root.path().join("live").exists());

        fs::write(root.path().join("active-generation"), "A".repeat(64))?;
        assert!(active_generation_key().is_err());
        fs::remove_file(root.path().join("active-generation"))?;

        let key = "a".repeat(64);
        let generation = generation_dir(&key)?;
        fs::create_dir_all(&generation)?;
        activate_generation(&key)?;

        let source_id: SourceId = "ato".parse().expect("valid test source id");
        assert_eq!(live_dir()?, generation);
        assert_eq!(db_path()?, generation.join(LEGAL_DB_FILENAME));
        assert_eq!(
            ann_path(&source_id)?,
            generation.join("ann").join("ato.ann")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn active_generation_rejects_directory_symlinks() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let _environment =
            crate::TestEnvironment::set(&[("LEGAL_MCP_DATA_DIR", root.path().as_os_str())]);
        let outside = tempfile::tempdir()?;
        let key = "c".repeat(64);
        fs::create_dir_all(generations_dir()?)?;
        symlink(outside.path(), generation_dir(&key)?)?;
        fs::write(root.path().join("active-generation"), &key)?;

        assert!(active_generation_key().is_err());
        assert!(activate_generation(&key).is_err());
        Ok(())
    }

    #[test]
    fn atomic_write_replaces_existing_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state.json");
        fs::write(&path, b"old")?;
        atomic_write(&path, b"new")?;
        assert_eq!(fs::read(&path)?, b"new");
        assert!(fs::read_dir(root.path())?.all(|entry| {
            !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        Ok(())
    }

    #[test]
    fn update_plugin_mcp_json_url_ignores_command_entry() {
        let root = std::env::temp_dir().join(format!(
            "australian-legal-mcp-config-test-{}-command-entry",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".mcp.json");
        fs::write(
            &path,
            r#"{
  "mcpServers": {
    "australian-legal": {
      "command": "legal-mcp",
      "args": ["mcp"],
      "url": "http://127.0.0.1:9/mcp"
    }
  }
}
"#,
        )
        .unwrap();

        assert!(!update_plugin_mcp_json_url(&path, "http://127.0.0.1:12345/mcp").unwrap());
        let value: JsonValue = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value["mcpServers"]["australian-legal"]["url"].as_str(),
            Some("http://127.0.0.1:9/mcp")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_marketplace_mcp_json_wins_over_cache_install() {
        let root = std::env::temp_dir().join(format!(
            "australian-legal-mcp-config-test-{}-directory",
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
                "australian-legal-mcp@australian-legal-mcp": [{
                    "installPath": cache_dir,
                    "lastUpdated": "2026-05-24T10:42:04Z"
                }]
            }
        });
        let known = serde_json::json!({
            "australian-legal-mcp": {
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
        let root = std::env::temp_dir().join(format!(
            "australian-legal-mcp-config-test-{}-cache",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cache_dir = root.join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join(".mcp.json"), "{}\n").unwrap();

        let installed = serde_json::json!({
            "plugins": {
                "australian-legal-mcp@australian-legal-mcp": [{
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
