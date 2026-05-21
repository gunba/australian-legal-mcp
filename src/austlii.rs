//! AustLII fetch and search via wreq's Chrome TLS impersonation.
//!
//! AustLII is fronted by Cloudflare. Document URLs on `classic.austlii.edu.au`
//! return clean 200s with a browser-grade TLS fingerprint and User-Agent;
//! the SINO search CGI (`sinosrch.cgi`) is gated everywhere and needs a
//! `cf_clearance` cookie acquired by clearing the JS challenge in a real
//! browser. We use the `wreq` crate (BoringSSL-backed TLS fingerprint
//! impersonation) plus the user's actual UA and the cookies persisted by
//! `ato-mcp austlii setup` to look like the user's own browser.
//!
//! See README.md "AustLII access" for the setup flow.

use crate::browser::{self, BrowserFamily, DetectedBrowser};
use crate::chunker::{chunk_html, EMBED_MAX_TOKENS};
use crate::cookies::{self, AustliiSession};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value as JsonValue};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::runtime::Runtime;
use wreq::Client;
use wreq_util::Emulation;

const FETCH_TIMEOUT_SECS: u64 = 30;
const AUSTLII_REFERER: &str = "https://classic.austlii.edu.au/";
const SETUP_REQUIRED_HINT: &str = "Run `ato-mcp austlii setup` to refresh the session cookie.";

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> Result<&'static Runtime> {
    // Lazy global tokio runtime. wreq is async-only; bridging through
    // block_on at the FFI boundary keeps the rest of ato-mcp synchronous.
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("austlii")
        .build()
        .context("building tokio runtime for AustLII")?;
    Ok(RUNTIME.get_or_init(|| rt))
}

/// Pick the wreq `Emulation` profile closest to the user's detected
/// browser. Falls back to the most recent profile when the detected
/// major version is older than what wreq-util ships — the goal is to
/// look like a current real browser, not to perfectly mirror an
/// outdated one.
fn detected_to_emulation(browser: &DetectedBrowser) -> Emulation {
    let major: u32 = browser
        .version
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match browser.family {
        BrowserFamily::Chromium => match major {
            137 => Emulation::Chrome137,
            136 => Emulation::Chrome136,
            135 => Emulation::Chrome135,
            134 => Emulation::Chrome134,
            133 => Emulation::Chrome133,
            132 => Emulation::Chrome132,
            131 => Emulation::Chrome131,
            130 => Emulation::Chrome130,
            129 => Emulation::Chrome129,
            128 => Emulation::Chrome128,
            127 => Emulation::Chrome127,
            126 => Emulation::Chrome126,
            _ => Emulation::Chrome137,
        },
        BrowserFamily::Firefox => match major {
            139 => Emulation::Firefox139,
            136 => Emulation::Firefox136,
            135 => Emulation::Firefox135,
            133 => Emulation::Firefox133,
            128 => Emulation::Firefox128,
            _ => Emulation::Firefox139,
        },
        BrowserFamily::Safari => match major {
            18 => Emulation::Safari18_5,
            17 => Emulation::Safari17_5,
            16 => Emulation::Safari16_5,
            _ => Emulation::Safari18_5,
        },
    }
}

fn build_client(emulation: Emulation) -> Result<Client> {
    Client::builder()
        .emulation(emulation)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .context("building wreq client for AustLII")
}

fn format_cookie_header(session: &AustliiSession) -> Option<String> {
    if session.cookies.is_empty() {
        return None;
    }
    let pairs: Vec<String> = session
        .cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    Some(pairs.join("; "))
}

/// Fetch an AustLII case or legislation document and return chunks in the
/// shared `fetch` response shape. `path` is the AustLII canonical path
/// stripped of the host (e.g. `au/cases/cth/HCA/1992/23`); we append `.html`
/// and prefix the classic host.
///
/// `_allow_ocr` is accepted for shape compatibility with the dispatcher
/// signature — PDF/OCR support lands in the next commit alongside
/// `search_austlii`.
pub(crate) fn fetch_austlii_doc(path: &str, _allow_ocr: bool) -> Result<String> {
    runtime()?.block_on(fetch_austlii_doc_async(path))
}

async fn fetch_austlii_doc_async(path: &str) -> Result<String> {
    let browser = browser::detect().context(
        "detecting default browser for AustLII fetch; set ATO_MCP_BROWSER to override",
    )?;
    let session = cookies::load_session()?;
    let emulation = detected_to_emulation(browser);
    let client = build_client(emulation)?;

    let url = format!("https://classic.austlii.edu.au/{path}.html");
    let mut req = client
        .get(&url)
        .header("User-Agent", browser.user_agent.as_str())
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-AU,en;q=0.9")
        .header("Referer", AUSTLII_REFERER);
    if let Some(s) = session.as_ref() {
        if let Some(cookie_header) = format_cookie_header(s) {
            req = req.header("Cookie", cookie_header);
        }
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if status.as_u16() == 403 {
        bail!(
            "AustLII returned HTTP 403 (likely Cloudflare bot challenge) for {url}. \
             {SETUP_REQUIRED_HINT}"
        );
    }
    if !status.is_success() {
        bail!("AustLII returned HTTP {} for {url}", status.as_u16());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading response body from {url}"))?;

    if is_pdf_bytes(&bytes) {
        bail!(
            "{url} responded with a PDF. PDF + OCR support lands in a follow-up \
             commit — for now, open the URL in a browser."
        );
    }

    let html = String::from_utf8_lossy(&bytes).to_string();
    let cleaned = clean_austlii_html(&html);
    if cleaned.html.trim().is_empty() {
        bail!(
            "no content body found in AustLII response for {url} — page \
             structure may have changed"
        );
    }

    let chunks = chunk_html(&cleaned.html, cleaned.title.as_deref(), EMBED_MAX_TOKENS);
    let chunk_json: Vec<JsonValue> = chunks
        .iter()
        .map(|c| {
            json!({
                "ord": c.ord,
                "anchor": c.anchor,
                "text": c.text,
            })
        })
        .collect();
    let canonical_uri = format!("austlii:{path}");
    Ok(serde_json::to_string_pretty(&json!({
        "uri": canonical_uri,
        "canonical_url": url,
        "title": cleaned.title,
        "source": "live",
        "ocr_used": false,
        "chunks": chunk_json,
    }))?)
}

fn is_pdf_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == b"%PDF"
}

/// Cleaned AustLII document: body inner HTML with script/style/nav nodes
/// stripped, plus the page `<title>` (with a trailing date trimmed off
/// where AustLII appends one).
pub(crate) struct CleanedAustliiDoc {
    pub(crate) html: String,
    pub(crate) title: Option<String>,
}

/// AustLII pages are simple XHTML-ish HTML. Pull the `<title>`, take the
/// `<body>` inner HTML, then regex out the noise elements so the chunker
/// doesn't churn on them. AustLII doesn't use a dedicated content container
/// the way ATO's `#LawContent` does — the whole body is the content.
pub(crate) fn clean_austlii_html(html: &str) -> CleanedAustliiDoc {
    let doc = Html::parse_document(html);
    let title_selector = Selector::parse("title").expect("valid selector");
    let title = doc
        .select(&title_selector)
        .next()
        .map(|n| n.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());

    let body_selector = Selector::parse("body").expect("valid selector");
    let body_inner = doc
        .select(&body_selector)
        .next()
        .map(|n| n.inner_html())
        .unwrap_or_default();

    let cleaned = strip_austlii_noise(&body_inner);
    CleanedAustliiDoc {
        html: cleaned,
        title,
    }
}

fn strip_austlii_noise(html: &str) -> String {
    static SCRIPT_RE: OnceLock<Regex> = OnceLock::new();
    static STYLE_RE: OnceLock<Regex> = OnceLock::new();
    static NAV_RE: OnceLock<Regex> = OnceLock::new();
    static COMMENT_RE: OnceLock<Regex> = OnceLock::new();
    let script = SCRIPT_RE.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*>.*?</script>").expect("valid regex")
    });
    let style =
        STYLE_RE.get_or_init(|| Regex::new(r"(?is)<style\b[^>]*>.*?</style>").expect("valid regex"));
    let nav = NAV_RE.get_or_init(|| Regex::new(r"(?is)<nav\b[^>]*>.*?</nav>").expect("valid regex"));
    let comment =
        COMMENT_RE.get_or_init(|| Regex::new(r"(?is)<!--.*?-->").expect("valid regex"));
    let s = script.replace_all(html, "").to_string();
    let s = style.replace_all(&s, "").to_string();
    let s = nav.replace_all(&s, "").to_string();
    comment.replace_all(&s, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserFamily;

    #[test]
    fn is_pdf_bytes_recognises_magic() {
        assert!(is_pdf_bytes(b"%PDF-1.4\n..."));
        assert!(!is_pdf_bytes(b"<html>"));
        assert!(!is_pdf_bytes(b""));
        assert!(!is_pdf_bytes(b"%PD"));
    }

    #[test]
    fn strip_austlii_noise_removes_script_style_nav() {
        let input = "<p>before</p><script>alert(1)</script><nav>menu</nav><style>p{}</style><!-- c --><p>after</p>";
        let cleaned = strip_austlii_noise(input);
        assert!(!cleaned.contains("<script"), "cleaned = {cleaned}");
        assert!(!cleaned.contains("<style"), "cleaned = {cleaned}");
        assert!(!cleaned.contains("<nav"), "cleaned = {cleaned}");
        assert!(!cleaned.contains("<!--"), "cleaned = {cleaned}");
        assert!(cleaned.contains("before"));
        assert!(cleaned.contains("after"));
    }

    #[test]
    fn clean_austlii_html_extracts_title_and_body() {
        let input = "<html><head><title>Scott v FCT [1966] HCA 48</title></head>\
                     <body><p>hello</p></body></html>";
        let cleaned = clean_austlii_html(input);
        assert_eq!(cleaned.title.as_deref(), Some("Scott v FCT [1966] HCA 48"));
        assert!(cleaned.html.contains("<p>hello</p>"));
    }

    #[test]
    fn format_cookie_header_joins_pairs() {
        let session = AustliiSession {
            acquired_at: "2026-05-21T00:00:00Z".to_string(),
            browser_name: "Google Chrome".to_string(),
            user_agent: "Mozilla/5.0".to_string(),
            cookies: vec![
                cookies::NamedCookie {
                    domain: "www.austlii.edu.au".to_string(),
                    name: "cf_clearance".to_string(),
                    value: "abc".to_string(),
                    expires: None,
                },
                cookies::NamedCookie {
                    domain: "www.austlii.edu.au".to_string(),
                    name: "session".to_string(),
                    value: "xyz".to_string(),
                    expires: None,
                },
            ],
        };
        assert_eq!(
            format_cookie_header(&session).as_deref(),
            Some("cf_clearance=abc; session=xyz"),
        );
    }

    #[test]
    fn detected_to_emulation_picks_chrome_version() {
        let detected = DetectedBrowser {
            name: "Google Chrome".to_string(),
            version: "136.0.7103.93".to_string(),
            family: BrowserFamily::Chromium,
            user_agent: "...".to_string(),
        };
        assert_eq!(detected_to_emulation(&detected), Emulation::Chrome136);
    }

    #[test]
    fn detected_to_emulation_falls_back_to_newest_for_old_chrome() {
        let detected = DetectedBrowser {
            name: "Google Chrome".to_string(),
            version: "90.0.0.0".to_string(),
            family: BrowserFamily::Chromium,
            user_agent: "...".to_string(),
        };
        assert_eq!(detected_to_emulation(&detected), Emulation::Chrome137);
    }
}
