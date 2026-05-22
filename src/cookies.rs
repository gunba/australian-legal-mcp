//! AustLII session cookie persistence.
//!
//! Older versions could persist browser cookies for AustLII SINO search.
//! Native SINO is no longer a supported retrieval path, but we still load
//! existing sessions so direct document fetches can reuse the recorded
//! User-Agent and `stats` can report the installation state without
//! exposing cookie values. Direct fetches and title-index search do not send
//! legacy persisted cookies because stale SINO sessions can break otherwise
//! valid document requests. Title-index search may use a temporary curl cookie
//! jar for AustLII's short-lived bot-management cookie.

use crate::config::data_dir;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const SESSION_FILE: &str = "austlii_session.json";

/// On-disk session record. `acquired_at` is an ISO-8601 UTC timestamp so
/// `austlii status` can show cookie age without re-reading the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AustliiSession {
    pub(crate) acquired_at: String,
    #[serde(default)]
    pub(crate) sino_validated_at: Option<String>,
    #[serde(default)]
    pub(crate) sino_validation_query: Option<String>,
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

pub(crate) fn load_session() -> Result<Option<AustliiSession>> {
    let path = session_file_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let session: AustliiSession =
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(session))
}

pub(crate) fn clear_session() -> Result<()> {
    let path = session_file_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
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
/// The `cf_clearance_present` boolean reports legacy session shape without
/// exposing the cookie value itself.
pub(crate) fn session_summary_json() -> serde_json::Value {
    match load_session() {
        Ok(Some(session)) => serde_json::json!({
            "session_present": true,
            "search_available": true,
            "search_backend": "austlii_title_index",
            "search_status": "available via AustLII title-index search with a temporary curl cookie jar; native AustLII SINO CGI endpoint is unavailable",
            "acquired_at": session.acquired_at,
            "sino_validated": session.sino_validated_at.is_some(),
            "sino_validated_at": session.sino_validated_at,
            "sino_validation_query": session.sino_validation_query,
            "browser_name": session.browser_name,
            "user_agent": session.user_agent,
            "cookie_count": session.cookies.len(),
            "cf_clearance_present": cf_clearance(&session).is_some(),
        }),
        _ => serde_json::json!({
            "session_present": false,
            "search_available": true,
            "search_backend": "austlii_title_index",
            "search_status": "available via AustLII title-index search with a temporary curl cookie jar; native AustLII SINO CGI endpoint is unavailable",
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
            sino_validated_at: None,
            sino_validation_query: None,
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
            sino_validated_at: None,
            sino_validation_query: None,
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
