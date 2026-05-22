//! AustLII session cookie persistence.
//!
//! AustLII SINO full-text search is Cloudflare-gated. `ato-mcp austlii setup`
//! stores the user's browser User-Agent plus the AustLII cookies needed to
//! replay a verified session. Stats expose only shape and validation state, not
//! cookie values. Direct fetches and title-index fallback do not send persisted
//! cookies because stale SINO sessions can break otherwise valid document
//! requests.

use crate::config::data_dir;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
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

pub(crate) fn save_session(session: &AustliiSession) -> Result<()> {
    let path = session_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(session).context("serializing AustLII session")?;
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub(crate) fn clear_session() -> Result<()> {
    let path = session_file_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn parse_manual_cookie(
    value: &str,
    user_agent: &str,
    browser_name: &str,
) -> Result<AustliiSession> {
    let value = value.trim();
    if value.is_empty() {
        bail!("AustLII cookie value is empty");
    }
    Ok(AustliiSession {
        acquired_at: Utc::now().to_rfc3339(),
        sino_validated_at: None,
        sino_validation_query: None,
        browser_name: browser_name.to_string(),
        user_agent: user_agent.to_string(),
        cookies: vec![NamedCookie {
            domain: ".austlii.edu.au".to_string(),
            name: "cf_clearance".to_string(),
            value: value.to_string(),
            expires: None,
        }],
    })
}

pub(crate) fn parse_manual_cookie_header(
    header: &str,
    user_agent: &str,
    browser_name: &str,
) -> Result<AustliiSession> {
    let cookies = header
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                return None;
            }
            Some(NamedCookie {
                domain: ".austlii.edu.au".to_string(),
                name: name.to_string(),
                value: value.to_string(),
                expires: None,
            })
        })
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        bail!("AustLII cookie header did not contain any name=value cookies");
    }
    Ok(AustliiSession {
        acquired_at: Utc::now().to_rfc3339(),
        sino_validated_at: None,
        sino_validation_query: None,
        browser_name: browser_name.to_string(),
        user_agent: user_agent.to_string(),
        cookies,
    })
}

pub(crate) fn load_browser_session(user_agent: &str, browser_name: &str) -> Result<AustliiSession> {
    let cookies = rookie::load(Some(vec!["austlii.edu.au".to_string()]))
        .map_err(|err| anyhow::anyhow!("loading AustLII cookies from browsers: {err}"))?
        .into_iter()
        .filter(|cookie| cookie.domain.contains("austlii.edu.au"))
        .map(|cookie| NamedCookie {
            domain: cookie.domain,
            name: cookie.name,
            value: cookie.value,
            expires: cookie.expires,
        })
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        bail!("no AustLII cookies found in local browsers");
    }
    Ok(AustliiSession {
        acquired_at: Utc::now().to_rfc3339(),
        sino_validated_at: None,
        sino_validation_query: None,
        browser_name: browser_name.to_string(),
        user_agent: user_agent.to_string(),
        cookies,
    })
}

/// Return the `cf_clearance` cookie value if present in the session.
pub(crate) fn cf_clearance(session: &AustliiSession) -> Option<&str> {
    session
        .cookies
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case("cf_clearance"))
        .map(|c| c.value.as_str())
}

pub(crate) fn cookie_header_for_host(session: &AustliiSession, host: &str) -> String {
    let now = Utc::now().timestamp().max(0) as u64;
    let mut selected = Vec::<&NamedCookie>::new();
    for cookie in session
        .cookies
        .iter()
        .filter(|cookie| {
            cookie
                .expires
                .map(|expires| expires == 0 || expires > now)
                .unwrap_or(true)
        })
        .filter(|cookie| cookie_matches_host(cookie, host))
    {
        match selected
            .iter()
            .position(|existing| existing.name.eq_ignore_ascii_case(&cookie.name))
        {
            Some(pos) if cookie_preferred(cookie, selected[pos], host) => selected[pos] = cookie,
            Some(_) => {}
            None => selected.push(cookie),
        }
    }
    selected
        .into_iter()
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ")
}

pub(crate) fn has_unexpired_cookie_for_host(
    session: &AustliiSession,
    host: &str,
    name: &str,
) -> bool {
    let now = Utc::now().timestamp().max(0) as u64;
    session.cookies.iter().any(|cookie| {
        cookie.name.eq_ignore_ascii_case(name)
            && cookie_matches_host(cookie, host)
            && cookie
                .expires
                .map(|expires| expires == 0 || expires > now)
                .unwrap_or(true)
    })
}

pub(crate) fn session_cookie_shapes(session: &AustliiSession) -> Vec<String> {
    session
        .cookies
        .iter()
        .map(|cookie| {
            format!(
                "{} | {} | expires={} | value_len={}",
                cookie.domain,
                cookie.name,
                cookie
                    .expires
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "session".to_string()),
                cookie.value.len()
            )
        })
        .collect()
}

pub(crate) fn write_curl_cookie_jar(session: &AustliiSession, path: &Path) -> Result<()> {
    let mut contents = String::from("# Netscape HTTP Cookie File\n");
    let now = Utc::now().timestamp().max(0) as u64;
    for cookie in &session.cookies {
        if cookie
            .expires
            .map(|expires| expires != 0 && expires <= now)
            .unwrap_or(false)
        {
            continue;
        }
        let include_subdomains = if cookie.domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let expires = cookie.expires.unwrap_or(0);
        contents.push_str(&format!(
            "{}\t{}\t/\tTRUE\t{}\t{}\t{}\n",
            cookie.domain, include_subdomains, expires, cookie.name, cookie.value
        ));
    }
    fs::write(path, contents).with_context(|| format!("writing curl cookie jar {}", path.display()))
}

pub(crate) fn merge_curl_cookie_jar(session: &mut AustliiSession, path: &Path) -> Result<bool> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading curl cookie jar {}", path.display()))?;
    let mut changed = false;
    for line in raw.lines() {
        if line.trim().is_empty() || (line.starts_with('#') && !line.starts_with("#HttpOnly_")) {
            continue;
        }
        let mut fields = line.split('\t');
        let Some(domain) = fields.next() else {
            continue;
        };
        let domain = domain.strip_prefix("#HttpOnly_").unwrap_or(domain);
        let _include_subdomains = fields.next();
        let _path = fields.next();
        let _secure = fields.next();
        let expires = fields.next().and_then(|v| v.parse::<u64>().ok());
        let Some(name) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next() else {
            continue;
        };
        if !domain.contains("austlii.edu.au") || name.is_empty() || value.is_empty() {
            continue;
        }
        let incoming = NamedCookie {
            domain: domain.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            expires,
        };
        match session.cookies.iter_mut().find(|existing| {
            existing.domain.eq_ignore_ascii_case(&incoming.domain)
                && existing.name.eq_ignore_ascii_case(&incoming.name)
        }) {
            Some(existing)
                if existing.value != incoming.value || existing.expires != incoming.expires =>
            {
                *existing = incoming;
                changed = true;
            }
            Some(_) => {}
            None => {
                session.cookies.push(incoming);
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn cookie_matches_host(cookie: &NamedCookie, host: &str) -> bool {
    let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn cookie_preferred(candidate: &NamedCookie, existing: &NamedCookie, host: &str) -> bool {
    let candidate_exact = candidate
        .domain
        .trim_start_matches('.')
        .eq_ignore_ascii_case(host);
    let existing_exact = existing
        .domain
        .trim_start_matches('.')
        .eq_ignore_ascii_case(host);
    match (candidate_exact, existing_exact) {
        (true, false) => true,
        (false, true) => false,
        _ => candidate.expires.unwrap_or(0) > existing.expires.unwrap_or(0),
    }
}

/// JSON summary of the persisted AustLII session for `stats` output.
/// The `cf_clearance_present` boolean reports legacy session shape without
/// exposing the cookie value itself.
pub(crate) fn session_summary_json() -> serde_json::Value {
    match load_session() {
        Ok(Some(session)) => session_summary_json_for(&session),
        _ => serde_json::json!({
            "session_present": false,
            "search_available": true,
            "search_backend": "austlii_title_index",
            "search_status": "title-index fallback available; run `ato-mcp austlii setup` to verify Cloudflare and enable native AustLII SINO full-text search",
            "native_search_available": false,
            "title_index_fallback_available": true,
        }),
    }
}

fn session_summary_json_for(session: &AustliiSession) -> serde_json::Value {
    let cf_clearance_present =
        has_unexpired_cookie_for_host(session, "classic.austlii.edu.au", "cf_clearance");
    let cf_bm_present = has_unexpired_cookie_for_host(session, "classic.austlii.edu.au", "__cf_bm");
    let sino_validated = session.sino_validated_at.is_some();
    let native_search_available = sino_validated && cf_clearance_present && cf_bm_present;
    let search_backend = if native_search_available {
        "austlii_sino"
    } else {
        "austlii_title_index"
    };
    let search_status = if native_search_available {
        "native AustLII SINO full-text search configured; title-index fallback remains available"
    } else if cf_clearance_present {
        "AustLII session present but native SINO is not currently validated; search will try local browser cookies, or run `ato-mcp austlii setup`"
    } else {
        "AustLII session present without cf_clearance; run `ato-mcp austlii setup` to verify Cloudflare"
    };
    serde_json::json!({
        "session_present": true,
        "search_available": true,
        "search_backend": search_backend,
        "search_status": search_status,
        "native_search_available": native_search_available,
        "title_index_fallback_available": true,
        "acquired_at": session.acquired_at,
        "sino_validated": sino_validated,
        "sino_validated_at": session.sino_validated_at,
        "sino_validation_query": session.sino_validation_query,
        "browser_name": session.browser_name,
        "user_agent": session.user_agent,
        "cookie_count": session.cookies.len(),
        "cf_clearance_present": cf_clearance_present,
        "cf_bm_present": cf_bm_present,
    })
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

    #[test]
    fn cookie_header_filters_expired_and_prefers_exact_host() {
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            sino_validated_at: None,
            sino_validation_query: None,
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "cf_clearance".to_string(),
                    value: "domain".to_string(),
                    expires: None,
                },
                NamedCookie {
                    domain: "www.austlii.edu.au".to_string(),
                    name: "cf_clearance".to_string(),
                    value: "host".to_string(),
                    expires: None,
                },
                NamedCookie {
                    domain: ".example.com".to_string(),
                    name: "ignored".to_string(),
                    value: "x".to_string(),
                    expires: None,
                },
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "expired".to_string(),
                    value: "x".to_string(),
                    expires: Some(1),
                },
            ],
        };
        assert_eq!(
            cookie_header_for_host(&session, "www.austlii.edu.au"),
            "cf_clearance=host"
        );
    }

    #[test]
    fn session_summary_requires_current_bot_cookie_for_native_search() {
        let now = Utc::now().timestamp().max(0) as u64;
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            sino_validated_at: Some("2026-05-21T00:01:00Z".to_string()),
            sino_validation_query: Some("Mabo Queensland".to_string()),
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "cf_clearance".to_string(),
                    value: "clearance".to_string(),
                    expires: Some(now + 3600),
                },
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "__cf_bm".to_string(),
                    value: "bot-cookie".to_string(),
                    expires: Some(now.saturating_sub(1)),
                },
            ],
        };
        let summary = session_summary_json_for(&session);
        assert_eq!(summary["cf_clearance_present"], true);
        assert_eq!(summary["cf_bm_present"], false);
        assert_eq!(summary["native_search_available"], false);
        assert_eq!(summary["search_backend"], "austlii_title_index");
    }

    #[test]
    fn session_summary_reports_native_search_for_valid_cookie_pair() {
        let now = Utc::now().timestamp().max(0) as u64;
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            sino_validated_at: Some("2026-05-21T00:01:00Z".to_string()),
            sino_validation_query: Some("Mabo Queensland".to_string()),
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "cf_clearance".to_string(),
                    value: "clearance".to_string(),
                    expires: Some(now + 3600),
                },
                NamedCookie {
                    domain: ".austlii.edu.au".to_string(),
                    name: "__cf_bm".to_string(),
                    value: "bot-cookie".to_string(),
                    expires: Some(now + 3600),
                },
            ],
        };
        let summary = session_summary_json_for(&session);
        assert_eq!(summary["cf_clearance_present"], true);
        assert_eq!(summary["cf_bm_present"], true);
        assert_eq!(summary["native_search_available"], true);
        assert_eq!(summary["search_backend"], "austlii_sino");
    }
}
