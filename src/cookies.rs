//! AustLII session cookie acquisition and persistence.
//!
//! AustLII's SINO search endpoint is gated by Cloudflare. Acquiring a
//! `cf_clearance` cookie requires clearing the JS challenge in a real
//! browser, which the user has already done while normally using the
//! internet. We borrow that clearance by reading the AustLII cookies out
//! of the user's browser cookie store via the `rookie` crate, and persist
//! them to disk so subsequent MCP calls don't need to re-read the
//! (potentially-locked) browser DB.
//!
//! Safari is unsupported by rookie's stable API surface; macOS users with
//! Safari as default should override to Chrome/Firefox via
//! `ATO_MCP_BROWSER` or paste the cf_clearance manually using
//! `ato-mcp austlii setup --cookie '<value>'`.

use crate::browser::{self, BrowserFamily};
use crate::config::data_dir;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const SESSION_FILE: &str = "austlii_session.json";

/// On-disk session record. `acquired_at` is an ISO-8601 UTC timestamp so
/// `austlii status` can show cookie age without re-reading the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AustliiSession {
    pub(crate) acquired_at: String,
    pub(crate) browser_name: String,
    pub(crate) user_agent: String,
    pub(crate) cookies: Vec<NamedCookie>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NamedCookie {
    pub(crate) domain: String,
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) expires: Option<u64>,
}

pub(crate) fn session_file_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(SESSION_FILE))
}

const AUSTLII_DOMAINS: &[&str] = &[
    "austlii.edu.au",
    "www.austlii.edu.au",
    "classic.austlii.edu.au",
];

/// Read AustLII cookies from the user's detected browser. Returns Ok(None)
/// when the browser store contains no AustLII cookies (user hasn't visited
/// AustLII yet, or did so under a profile we can't see). Errors are
/// surfaced rather than swallowed so the CLI can offer the manual-paste
/// fallback when EDR / file-lock issues block extraction.
pub(crate) fn read_browser_cookies() -> Result<Option<Vec<NamedCookie>>> {
    let browser = browser::detect()?;
    let domains: Vec<String> = AUSTLII_DOMAINS.iter().map(|s| s.to_string()).collect();
    let cookies = match browser.family {
        BrowserFamily::Chromium => read_chromium_cookies(domains)?,
        BrowserFamily::Firefox => read_firefox_cookies(domains)?,
        BrowserFamily::Safari => {
            bail!(
                "Safari cookie extraction is not supported. Override the \
                 detected browser via `ATO_MCP_BROWSER=chrome` (or firefox/edge), \
                 or paste the cf_clearance value manually with \
                 `ato-mcp austlii setup --cookie <value>`."
            );
        }
    };
    if cookies.is_empty() {
        return Ok(None);
    }
    Ok(Some(cookies))
}

fn read_chromium_cookies(domains: Vec<String>) -> Result<Vec<NamedCookie>> {
    // Chromium-family browsers all share the same on-disk cookie DB
    // format. Try Chrome first, then Edge, then Brave — whichever has the
    // AustLII cookie wins. Errors from one engine don't abort the whole
    // probe so a missing browser doesn't mask a present one.
    let mut last_err: Option<anyhow::Error> = None;
    match rookie::chrome(Some(domains.clone())) {
        Ok(cookies) if !cookies.is_empty() => {
            return Ok(cookies.into_iter().map(convert_cookie).collect());
        }
        Ok(_) => {}
        Err(e) => last_err = Some(anyhow!("rookie chrome: {e}")),
    }
    match rookie::edge(Some(domains.clone())) {
        Ok(cookies) if !cookies.is_empty() => {
            return Ok(cookies.into_iter().map(convert_cookie).collect());
        }
        Ok(_) => {}
        Err(e) => last_err = Some(anyhow!("rookie edge: {e}")),
    }
    match rookie::brave(Some(domains)) {
        Ok(cookies) if !cookies.is_empty() => {
            return Ok(cookies.into_iter().map(convert_cookie).collect());
        }
        Ok(_) => {}
        Err(e) => last_err = Some(anyhow!("rookie brave: {e}")),
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    Ok(Vec::new())
}

fn read_firefox_cookies(domains: Vec<String>) -> Result<Vec<NamedCookie>> {
    let cookies = rookie::firefox(Some(domains))
        .map_err(|e| anyhow!("rookie firefox extraction failed: {e}"))?;
    Ok(cookies.into_iter().map(convert_cookie).collect())
}

fn convert_cookie(c: rookie::common::enums::Cookie) -> NamedCookie {
    NamedCookie {
        domain: c.domain,
        name: c.name,
        value: c.value,
        expires: c.expires,
    }
}

/// Atomically persist the session to disk.
pub(crate) fn save_session(session: &AustliiSession) -> Result<()> {
    let path = session_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("creating data dir {}", parent.display())
        })?;
    }
    let json = serde_json::to_string_pretty(session)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)
        .with_context(|| format!("writing temp session file {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming session file to {}", path.display()))?;
    Ok(())
}

pub(crate) fn load_session() -> Result<Option<AustliiSession>> {
    let path = session_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let session: AustliiSession = serde_json::from_str(&contents)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(session))
}

pub(crate) fn clear_session() -> Result<()> {
    let path = session_file_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Return the `cf_clearance` cookie value if present in the session.
pub(crate) fn cf_clearance(session: &AustliiSession) -> Option<&str> {
    session
        .cookies
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case("cf_clearance"))
        .map(|c| c.value.as_str())
}

/// JSON summary of the persisted AustLII session for `stats` output.
/// Returns `null` when no session is persisted. The `cf_clearance_present`
/// boolean lets callers see whether search is functional without exposing
/// the cookie value itself.
pub(crate) fn session_summary_json() -> serde_json::Value {
    match load_session() {
        Ok(Some(session)) => serde_json::json!({
            "session_present": true,
            "acquired_at": session.acquired_at,
            "browser_name": session.browser_name,
            "user_agent": session.user_agent,
            "cookie_count": session.cookies.len(),
            "cf_clearance_present": cf_clearance(&session).is_some(),
        }),
        _ => serde_json::json!({
            "session_present": false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_clearance_finds_named_cookie_case_insensitively() {
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![
                NamedCookie {
                    domain: "www.austlii.edu.au".to_string(),
                    name: "CF_Clearance".to_string(),
                    value: "abc123".to_string(),
                    expires: None,
                },
                NamedCookie {
                    domain: "www.austlii.edu.au".to_string(),
                    name: "other".to_string(),
                    value: "xyz".to_string(),
                    expires: None,
                },
            ],
        };
        assert_eq!(cf_clearance(&session), Some("abc123"));
    }

    #[test]
    fn cf_clearance_returns_none_when_absent() {
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![NamedCookie {
                domain: "www.austlii.edu.au".to_string(),
                name: "session".to_string(),
                value: "xyz".to_string(),
                expires: None,
            }],
        };
        assert_eq!(cf_clearance(&session), None);
    }
}
