//! Default-browser detection and per-user User-Agent construction.
//!
//! AustLII's Cloudflare-managed access gates traffic on TLS fingerprint and
//! User-Agent. Reusing the UA string that the user's own browser sends
//! (rather than a hardcoded Chrome version) keeps our requests
//! indistinguishable from a normal user clicking a link — they land in a
//! behavioural profile Cloudflare already accepts because the user has
//! cleared the JS challenge in their browser. The same machinery feeds
//! the AustLII cookie module so the cf_clearance value we extract is
//! actually valid for the UA we send.
//!
//! Override the auto-detected browser with `ATO_MCP_BROWSER=chrome|edge|firefox|safari`
//! for users on managed endpoints where the registry / xdg-mime lookup
//! returns something unexpected.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DetectedBrowser {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) family: BrowserFamily,
    pub(crate) user_agent: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BrowserFamily {
    Chromium,
    Firefox,
    Safari,
}

impl BrowserFamily {
    /// Domains we expect to find this family's cookies under. Matches
    /// rookie's per-browser API surface.
    pub(crate) fn cookie_source_label(&self) -> &'static str {
        match self {
            BrowserFamily::Chromium => "chromium-based browser cookie store",
            BrowserFamily::Firefox => "firefox cookie store",
            BrowserFamily::Safari => "safari cookie store (macOS Keychain)",
        }
    }
}

static CACHE: OnceLock<DetectedBrowser> = OnceLock::new();

/// Best-effort default-browser detection, memoised across the process
/// lifetime. Returns the cached result on every call after the first.
/// Detection failure is surfaced as an error rather than swallowed —
/// callers can choose between hard-fail and a pinned-UA fallback, but
/// the project default is to surface the gap so per-user fingerprinting
/// stays meaningful.
pub(crate) fn detect() -> Result<&'static DetectedBrowser> {
    if let Some(c) = CACHE.get() {
        return Ok(c);
    }
    let detected = detect_inner()?;
    Ok(CACHE.get_or_init(|| detected))
}

fn detect_inner() -> Result<DetectedBrowser> {
    if let Ok(override_value) = std::env::var("ATO_MCP_BROWSER") {
        return detect_from_override(&override_value);
    }
    detect_native()
}

fn detect_from_override(value: &str) -> Result<DetectedBrowser> {
    // The override accepts a family name; we still probe the binary so we
    // can build a UA string with the actual installed version rather than
    // a fabricated one.
    let family = match value.to_lowercase().as_str() {
        "chrome" | "chromium" => BrowserFamily::Chromium,
        "edge" | "msedge" => BrowserFamily::Chromium,
        "firefox" | "ff" => BrowserFamily::Firefox,
        "safari" => BrowserFamily::Safari,
        other => bail!(
            "unknown ATO_MCP_BROWSER override `{other}`; \
             expected one of: chrome, edge, firefox, safari"
        ),
    };
    detect_for_family(family, value)
}

#[cfg(target_os = "windows")]
fn detect_native() -> Result<DetectedBrowser> {
    use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let user_choice = hkcu
        .open_subkey(
            "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\https\\UserChoice",
        )
        .context(
            "opening HKCU\\...\\UrlAssociations\\https\\UserChoice; the user may not have \
             set a default browser",
        )?;
    let prog_id: String = user_choice
        .get_value("ProgId")
        .context("reading ProgId from UserChoice")?;
    let class_root = RegKey::predef(HKEY_CLASSES_ROOT);
    let cmd_key = class_root
        .open_subkey(format!("{prog_id}\\shell\\open\\command"))
        .with_context(|| format!("opening HKCR\\{prog_id}\\shell\\open\\command"))?;
    let cmd_line: String = cmd_key
        .get_value("")
        .with_context(|| format!("reading default value of HKCR\\{prog_id}\\shell\\open\\command"))?;
    let exe_path = parse_command_line_exe(&cmd_line)?;
    let (family, name) = classify_windows_exe(&exe_path)?;
    let version = run_version_probe(&exe_path)?;
    let user_agent = build_user_agent(family, &version, OsLabel::Windows);
    Ok(DetectedBrowser {
        name,
        version,
        family,
        user_agent,
    })
}

#[cfg(target_os = "macos")]
fn detect_native() -> Result<DetectedBrowser> {
    // macOS doesn't expose a stable command-line interface for the default
    // browser. Probe `/Applications/` and pick the first installed match,
    // ordered Chrome > Edge > Firefox > Safari (most common power-user
    // choices first). Users on macOS with non-default Safari can override
    // via ATO_MCP_BROWSER.
    let candidates: &[(&str, BrowserFamily, &str)] = &[
        (
            "/Applications/Google Chrome.app",
            BrowserFamily::Chromium,
            "Google Chrome",
        ),
        (
            "/Applications/Microsoft Edge.app",
            BrowserFamily::Chromium,
            "Microsoft Edge",
        ),
        (
            "/Applications/Firefox.app",
            BrowserFamily::Firefox,
            "Mozilla Firefox",
        ),
        ("/Applications/Safari.app", BrowserFamily::Safari, "Safari"),
    ];
    for (app_path, family, name) in candidates {
        if !std::path::Path::new(app_path).exists() {
            continue;
        }
        let version = read_macos_app_version(app_path)?;
        let user_agent = build_user_agent(*family, &version, OsLabel::Macos);
        return Ok(DetectedBrowser {
            name: (*name).to_string(),
            version,
            family: *family,
            user_agent,
        });
    }
    bail!(
        "no supported browser found in /Applications/; install Chrome, Edge, \
         Firefox or Safari, or set ATO_MCP_BROWSER"
    )
}

#[cfg(target_os = "linux")]
fn detect_native() -> Result<DetectedBrowser> {
    let output = std::process::Command::new("xdg-mime")
        .args(["query", "default", "x-scheme-handler/https"])
        .output()
        .context("running `xdg-mime query default x-scheme-handler/https`")?;
    let desktop_file = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if desktop_file.is_empty() {
        bail!(
            "xdg-mime returned no default handler for https; set ATO_MCP_BROWSER or \
             configure a default browser"
        );
    }
    let binary = resolve_linux_desktop_exec(&desktop_file)?;
    let version = run_version_probe(&binary)?;
    let (name, family) = classify_linux_binary(&binary, &version);
    let user_agent = build_user_agent(family, &version, OsLabel::Linux);
    Ok(DetectedBrowser {
        name,
        version,
        family,
        user_agent,
    })
}

/// Family-driven detection used by the override path. Walks the same
/// platform-specific binary probes as `detect_native` but locked to a
/// caller-specified family.
#[cfg(target_os = "windows")]
fn detect_for_family(family: BrowserFamily, override_value: &str) -> Result<DetectedBrowser> {
    // The override carries the family; we still try to find an installed
    // binary so the version string is real rather than fabricated.
    let candidates: &[(&str, BrowserFamily, &str)] = &[
        (
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
            BrowserFamily::Chromium,
            "Google Chrome",
        ),
        (
            "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
            BrowserFamily::Chromium,
            "Google Chrome",
        ),
        (
            "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
            BrowserFamily::Chromium,
            "Microsoft Edge",
        ),
        (
            "C:\\Program Files\\Mozilla Firefox\\firefox.exe",
            BrowserFamily::Firefox,
            "Mozilla Firefox",
        ),
    ];
    for (path, fam, name) in candidates {
        if *fam != family {
            continue;
        }
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let version = run_version_probe(path)?;
        let user_agent = build_user_agent(family, &version, OsLabel::Windows);
        return Ok(DetectedBrowser {
            name: (*name).to_string(),
            version,
            family,
            user_agent,
        });
    }
    bail!("ATO_MCP_BROWSER=`{override_value}` requested but no matching binary found")
}

#[cfg(target_os = "macos")]
fn detect_for_family(family: BrowserFamily, override_value: &str) -> Result<DetectedBrowser> {
    let candidates: &[(&str, BrowserFamily, &str)] = &[
        (
            "/Applications/Google Chrome.app",
            BrowserFamily::Chromium,
            "Google Chrome",
        ),
        (
            "/Applications/Microsoft Edge.app",
            BrowserFamily::Chromium,
            "Microsoft Edge",
        ),
        (
            "/Applications/Firefox.app",
            BrowserFamily::Firefox,
            "Mozilla Firefox",
        ),
        ("/Applications/Safari.app", BrowserFamily::Safari, "Safari"),
    ];
    for (app_path, fam, name) in candidates {
        if *fam != family {
            continue;
        }
        if !std::path::Path::new(app_path).exists() {
            continue;
        }
        let version = read_macos_app_version(app_path)?;
        let user_agent = build_user_agent(family, &version, OsLabel::Macos);
        return Ok(DetectedBrowser {
            name: (*name).to_string(),
            version,
            family,
            user_agent,
        });
    }
    bail!("ATO_MCP_BROWSER=`{override_value}` requested but no matching app found")
}

#[cfg(target_os = "linux")]
fn detect_for_family(family: BrowserFamily, override_value: &str) -> Result<DetectedBrowser> {
    let candidates: &[(&str, BrowserFamily, &str)] = &[
        ("google-chrome", BrowserFamily::Chromium, "Google Chrome"),
        (
            "google-chrome-stable",
            BrowserFamily::Chromium,
            "Google Chrome",
        ),
        ("chromium", BrowserFamily::Chromium, "Chromium"),
        ("microsoft-edge", BrowserFamily::Chromium, "Microsoft Edge"),
        ("firefox", BrowserFamily::Firefox, "Mozilla Firefox"),
    ];
    for (bin, fam, name) in candidates {
        if *fam != family {
            continue;
        }
        match run_version_probe(bin) {
            Ok(version) => {
                let user_agent = build_user_agent(family, &version, OsLabel::Linux);
                return Ok(DetectedBrowser {
                    name: (*name).to_string(),
                    version,
                    family,
                    user_agent,
                });
            }
            Err(_) => continue,
        }
    }
    bail!("ATO_MCP_BROWSER=`{override_value}` requested but no matching binary on $PATH")
}

#[derive(Copy, Clone)]
#[allow(dead_code)] // Variants are constructed under #[cfg(target_os = ...)] arms.
enum OsLabel {
    Windows,
    Macos,
    Linux,
}

impl OsLabel {
    fn token(self) -> &'static str {
        match self {
            OsLabel::Windows => "(Windows NT 10.0; Win64; x64)",
            OsLabel::Macos => "(Macintosh; Intel Mac OS X 10_15_7)",
            OsLabel::Linux => "(X11; Linux x86_64)",
        }
    }
}

fn build_user_agent(family: BrowserFamily, version: &str, os: OsLabel) -> String {
    let os_token = os.token();
    match family {
        BrowserFamily::Chromium => format!(
            "Mozilla/5.0 {os_token} AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{version} Safari/537.36"
        ),
        BrowserFamily::Firefox => {
            let major = version
                .split('.')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("0");
            format!("Mozilla/5.0 {os_token} Gecko/20100101 Firefox/{major}.0")
        }
        BrowserFamily::Safari => format!(
            "Mozilla/5.0 {os_token} AppleWebKit/605.1.15 (KHTML, like Gecko) Version/{version} Safari/605.1.15"
        ),
    }
}

/// Take the first line of the `--version` output, strip non-version text
/// and return `(display name, family, version)`. Recognises Chrome, Edge,
/// Chromium, Firefox.
fn parse_browser_version_output(s: &str) -> Result<(String, BrowserFamily, String)> {
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        bail!("empty --version output");
    }
    let version = line
        .split_whitespace()
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .last()
        .ok_or_else(|| anyhow!("could not extract version token from `{line}`"))?
        .to_string();
    let lower = line.to_lowercase();
    let (name, family) = if lower.contains("edge") {
        ("Microsoft Edge".to_string(), BrowserFamily::Chromium)
    } else if lower.contains("chromium") {
        ("Chromium".to_string(), BrowserFamily::Chromium)
    } else if lower.contains("chrome") {
        ("Google Chrome".to_string(), BrowserFamily::Chromium)
    } else if lower.contains("firefox") {
        ("Mozilla Firefox".to_string(), BrowserFamily::Firefox)
    } else {
        bail!("unrecognised browser identity in `{line}`");
    };
    Ok((name, family, version))
}

fn run_version_probe(bin: &str) -> Result<String> {
    let output = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running `{bin} --version`"))?;
    if !output.status.success() {
        bail!(
            "`{bin} --version` failed with exit code {}",
            output
                .status
                .code()
                .map_or_else(|| "<no code>".to_string(), |c| c.to_string())
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let (_, _, version) = parse_browser_version_output(&stdout)?;
    Ok(version)
}

#[cfg(target_os = "windows")]
fn classify_windows_exe(exe_path: &str) -> Result<(BrowserFamily, String)> {
    let lower = exe_path.to_lowercase();
    if lower.contains("msedge") {
        Ok((BrowserFamily::Chromium, "Microsoft Edge".to_string()))
    } else if lower.contains("chrome") {
        Ok((BrowserFamily::Chromium, "Google Chrome".to_string()))
    } else if lower.contains("firefox") {
        Ok((BrowserFamily::Firefox, "Mozilla Firefox".to_string()))
    } else if lower.contains("brave") {
        Ok((BrowserFamily::Chromium, "Brave".to_string()))
    } else if lower.contains("opera") {
        Ok((BrowserFamily::Chromium, "Opera".to_string()))
    } else {
        bail!("unrecognised default browser executable: {exe_path}")
    }
}

#[cfg(target_os = "windows")]
fn parse_command_line_exe(cmd: &str) -> Result<String> {
    let trimmed = cmd.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest
            .find('"')
            .ok_or_else(|| anyhow!("unterminated quote in registry command line `{cmd}`"))?;
        Ok(rest[..end].to_string())
    } else {
        Ok(trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(target_os = "macos")]
fn read_macos_app_version(app_path: &str) -> Result<String> {
    let output = std::process::Command::new("mdls")
        .args(["-name", "kMDItemVersion", "-raw", app_path])
        .output()
        .with_context(|| format!("running `mdls -name kMDItemVersion -raw {app_path}`"))?;
    if !output.status.success() {
        bail!("mdls failed for {app_path}");
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() || s == "(null)" {
        bail!("mdls returned no version for {app_path}");
    }
    Ok(s)
}

#[cfg(target_os = "linux")]
fn resolve_linux_desktop_exec(desktop_file: &str) -> Result<String> {
    let mut search_dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        search_dirs.push(home.join(".local/share/applications"));
    }
    if let Ok(xdg_data_home) = std::env::var("XDG_DATA_HOME") {
        search_dirs.push(std::path::PathBuf::from(format!(
            "{xdg_data_home}/applications"
        )));
    }
    search_dirs.push(std::path::PathBuf::from("/usr/local/share/applications"));
    search_dirs.push(std::path::PathBuf::from("/usr/share/applications"));
    for dir in search_dirs {
        let path = dir.join(desktop_file);
        if !path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        for line in contents.lines() {
            if let Some(exec) = line.strip_prefix("Exec=") {
                let bin = exec.split_whitespace().next().unwrap_or("");
                if !bin.is_empty() {
                    return Ok(bin.to_string());
                }
            }
        }
    }
    bail!(".desktop file `{desktop_file}` not found in standard application directories")
}

#[cfg(target_os = "linux")]
fn classify_linux_binary(binary: &str, version_string: &str) -> (String, BrowserFamily) {
    let lower_bin = binary.to_lowercase();
    if lower_bin.contains("microsoft-edge") || lower_bin.contains("msedge") {
        return ("Microsoft Edge".to_string(), BrowserFamily::Chromium);
    }
    if lower_bin.contains("chromium") || version_string.to_lowercase().contains("chromium") {
        return ("Chromium".to_string(), BrowserFamily::Chromium);
    }
    if lower_bin.contains("chrome") || version_string.to_lowercase().contains("chrome") {
        return ("Google Chrome".to_string(), BrowserFamily::Chromium);
    }
    if lower_bin.contains("firefox") || version_string.to_lowercase().contains("firefox") {
        return ("Mozilla Firefox".to_string(), BrowserFamily::Firefox);
    }
    ("Unknown".to_string(), BrowserFamily::Chromium)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_user_agent_chromium_windows() {
        let ua = build_user_agent(BrowserFamily::Chromium, "136.0.7103.93", OsLabel::Windows);
        assert!(ua.contains("Windows NT 10.0"), "ua = {ua}");
        assert!(ua.contains("Chrome/136.0.7103.93"), "ua = {ua}");
        assert!(ua.contains("Safari/537.36"), "ua = {ua}");
    }

    #[test]
    fn build_user_agent_firefox_macos_uses_major_only() {
        let ua = build_user_agent(BrowserFamily::Firefox, "137.0.1", OsLabel::Macos);
        assert!(ua.contains("Intel Mac OS X"), "ua = {ua}");
        assert!(ua.contains("Firefox/137.0"), "ua = {ua}");
        assert!(!ua.contains("Firefox/137.0.1"), "ua = {ua}");
    }

    #[test]
    fn build_user_agent_safari_macos() {
        let ua = build_user_agent(BrowserFamily::Safari, "17.6", OsLabel::Macos);
        assert!(ua.contains("Macintosh"), "ua = {ua}");
        assert!(ua.contains("Version/17.6"), "ua = {ua}");
    }

    #[test]
    fn parse_browser_version_output_recognises_chrome() {
        let (name, family, version) =
            parse_browser_version_output("Google Chrome 136.0.7103.93\n").unwrap();
        assert_eq!(name, "Google Chrome");
        assert_eq!(family, BrowserFamily::Chromium);
        assert_eq!(version, "136.0.7103.93");
    }

    #[test]
    fn parse_browser_version_output_recognises_edge() {
        let (name, family, version) =
            parse_browser_version_output("Microsoft Edge 136.0.3240.50\n").unwrap();
        assert_eq!(name, "Microsoft Edge");
        assert_eq!(family, BrowserFamily::Chromium);
        assert_eq!(version, "136.0.3240.50");
    }

    #[test]
    fn parse_browser_version_output_recognises_firefox() {
        let (name, family, version) =
            parse_browser_version_output("Mozilla Firefox 137.0\n").unwrap();
        assert_eq!(name, "Mozilla Firefox");
        assert_eq!(family, BrowserFamily::Firefox);
        assert_eq!(version, "137.0");
    }

    #[test]
    fn parse_browser_version_output_recognises_chromium() {
        let (name, family, version) = parse_browser_version_output("Chromium 137.0.7151.119 snap\n").unwrap();
        assert_eq!(name, "Chromium");
        assert_eq!(family, BrowserFamily::Chromium);
        // Trailing "snap" word is skipped because it carries no digits.
        assert_eq!(version, "137.0.7151.119");
    }

    #[test]
    fn parse_browser_version_output_rejects_unknown() {
        let err = parse_browser_version_output("Some other browser 1.2.3\n").unwrap_err();
        assert!(err.to_string().contains("unrecognised browser"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parse_command_line_exe_quoted_path() {
        let exe = parse_command_line_exe(
            r#""C:\Program Files\Google\Chrome\Application\chrome.exe" -- "%1""#,
        )
        .unwrap();
        assert_eq!(exe, r"C:\Program Files\Google\Chrome\Application\chrome.exe");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parse_command_line_exe_unquoted_path() {
        let exe =
            parse_command_line_exe(r"C:\Windows\System32\notepad.exe %1").unwrap();
        assert_eq!(exe, r"C:\Windows\System32\notepad.exe");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parse_command_line_exe_unterminated_quote_errors() {
        let err = parse_command_line_exe(r#""C:\bad path"#).unwrap_err();
        assert!(err.to_string().contains("unterminated quote"));
    }
}
