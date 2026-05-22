//! AustLII fetch support.
//!
//! AustLII is fronted by Cloudflare. Document URLs on
//! `classic.austlii.edu.au` return clean 200s with a browser-grade
//! User-Agent over standard TLS. AustLII's formerly published SINO
//! search CGI is no longer a supported retrieval path; `search_austlii`
//! therefore fails fast with an explicit unavailable message instead
//! of trying to tune cookies against a retired endpoint.
//!
//! See README.md "AustLII access" for the setup flow.

use crate::browser;
use crate::chunker::{chunk_html, EMBED_MAX_TOKENS};
use crate::cookies;
use crate::ocr;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::blocking::Client;
#[cfg(test)]
use scraper::ElementRef;
use scraper::{Html, Selector};
use serde_json::{json, Value as JsonValue};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

const FETCH_TIMEOUT_SECS: u64 = 30;
const AUSTLII_REFERER: &str = "https://classic.austlii.edu.au/";
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const ACCEPT_LANGUAGE: &str = "en-AU,en;q=0.9";
const FETCH_CHALLENGE_HINT: &str =
    "AustLII may be presenting a bot challenge for this document fetch.";
pub(crate) const AUSTLII_SEARCH_UNAVAILABLE: &str =
    "AustLII search is currently unavailable: AustLII's published SINO CGI endpoint \
     (/cgi-bin/sinosrch.cgi) now returns 'no longer available'. This is not a cookie \
     setup issue. Known AustLII documents can still be fetched with `fetch` using an \
     `austlii:<path>` URI.";
const PDF_TEXT_MIN_CHARS: usize = 100;
const OCR_WARNING: &str =
    "Text extracted via Tesseract OCR — may contain errors. Verify against the canonical source.";

fn build_client(user_agent: &str, timeout_secs: u64) -> Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .context("building HTTP client for AustLII")
}

/// Fetch an AustLII case or legislation document and return chunks in
/// the shared `fetch` response shape. `path` is the AustLII canonical
/// path stripped of the host (e.g. `au/cases/cth/HCA/1992/23`); we
/// append `.html` and prefix the classic host.
///
/// When the response is a scanned PDF and `allow_ocr` is true, the PDF
/// goes through Tesseract OCR via `crate::ocr`. With `allow_ocr=false`
/// (the default) we surface a clear error so callers can opt in
/// explicitly — OCR can take 10-30s and risks tripping the MCP
/// request timeout if the client isn't configured for it.
pub(crate) fn fetch_austlii_doc(path: &str, allow_ocr: bool) -> Result<String> {
    let session = cookies::load_session()?;
    let user_agent = match session.as_ref() {
        Some(session) => session.user_agent.as_str(),
        None => browser::detect()
            .context(
                "detecting default browser for AustLII fetch; set ATO_MCP_BROWSER to override",
            )?
            .user_agent
            .as_str(),
    };

    let url = format!("https://classic.austlii.edu.au/{path}.html");
    let bytes = fetch_austlii_bytes(&url, user_agent)?;

    if is_pdf_bytes(&bytes) {
        return handle_pdf_response(path, &url, &bytes, allow_ocr);
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

fn fetch_austlii_bytes(url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let curl_err = match fetch_austlii_bytes_with_curl(url, user_agent) {
        Ok(bytes) => return Ok(bytes),
        Err(err) => err,
    };
    fetch_austlii_bytes_with_reqwest(url, user_agent)
        .with_context(|| format!("curl fetch failed first: {curl_err}"))
}

fn fetch_austlii_bytes_with_curl(url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let output = Command::new("curl")
        .arg("-LfsS")
        .arg("--max-time")
        .arg(FETCH_TIMEOUT_SECS.to_string())
        .arg("-A")
        .arg(user_agent)
        .arg("-H")
        .arg(format!("Accept: {ACCEPT_HTML}"))
        .arg("-H")
        .arg(format!("Accept-Language: {ACCEPT_LANGUAGE}"))
        .arg("-H")
        .arg(format!("Referer: {AUSTLII_REFERER}"))
        .arg(url)
        .output()
        .context("running curl for AustLII fetch")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("curl exited with status {} for {url}: {stderr}", output.status);
    }
    Ok(output.stdout)
}

fn fetch_austlii_bytes_with_reqwest(url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let client = build_client(user_agent, FETCH_TIMEOUT_SECS)?;
    let req = client
        .get(url)
        .header("Accept", ACCEPT_HTML)
        .header("Accept-Language", ACCEPT_LANGUAGE)
        .header("Referer", AUSTLII_REFERER);
    let resp = req.send().with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if status.as_u16() == 403 {
        bail!(
            "AustLII returned HTTP 403 (likely Cloudflare bot challenge) for {url}. \
             {FETCH_CHALLENGE_HINT}"
        );
    }
    if !status.is_success() {
        bail!("AustLII returned HTTP {} for {url}", status.as_u16());
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading response body from {url}"))?;
    Ok(bytes.to_vec())
}

fn handle_pdf_response(path: &str, url: &str, bytes: &[u8], allow_ocr: bool) -> Result<String> {
    let embedded = extract_pdf_text(bytes).unwrap_or_default();
    let (text, ocr_used) = if embedded.trim().len() >= PDF_TEXT_MIN_CHARS {
        (embedded, false)
    } else if allow_ocr {
        let ocr_text = ocr::ocr_pdf(bytes)
            .with_context(|| format!("running OCR over PDF response from {url}"))?;
        (ocr_text, true)
    } else {
        bail!(
            "{url} is a scanned PDF with no embedded text. Retry with \
             allow_ocr=true to run Tesseract OCR. OCR can take 10-30s and \
             will exceed the MCP default 30s request timeout — set \
             `timeout: 120000` in your MCP client config first."
        );
    };
    let title = derive_pdf_title(path);
    let chunks = vec![json!({
        "ord": 0,
        "anchor": null,
        "text": text,
    })];
    let canonical_uri = format!("austlii:{path}");
    let mut response = serde_json::Map::new();
    response.insert("uri".to_string(), JsonValue::String(canonical_uri));
    response.insert(
        "canonical_url".to_string(),
        JsonValue::String(url.to_string()),
    );
    response.insert("title".to_string(), JsonValue::String(title));
    response.insert("source".to_string(), JsonValue::String("live".to_string()));
    response.insert("ocr_used".to_string(), JsonValue::Bool(ocr_used));
    response.insert("chunks".to_string(), JsonValue::Array(chunks));
    if ocr_used {
        response.insert(
            "ocr_warning".to_string(),
            JsonValue::String(OCR_WARNING.to_string()),
        );
    }
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

fn extract_pdf_text(bytes: &[u8]) -> Result<String> {
    pdf_extract::extract_text_from_mem(bytes).map_err(|e| anyhow!("pdf_extract failed: {e}"))
}

/// Derive a human-readable title from the AustLII path when the PDF
/// response carries no usable metadata. `au/cases/cth/HCA/1966/48` →
/// `"HCA 1966/48"`.
fn derive_pdf_title(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 5 && parts[1] == "cases" {
        return format!("{} {}/{}", parts[3], parts[4], parts.last().unwrap_or(&"?"));
    }
    if parts.len() >= 2 && parts[1] == "legis" {
        return path.to_string();
    }
    path.to_string()
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

/// AustLII pages are simple XHTML-ish HTML. Pull the `<title>`, take
/// the `<body>` inner HTML, then regex out the noise elements so the
/// chunker doesn't churn on them. AustLII doesn't use a dedicated
/// content container the way ATO's `#LawContent` does — the whole
/// body is the content.
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
    let script = SCRIPT_RE
        .get_or_init(|| Regex::new(r"(?is)<script\b[^>]*>.*?</script>").expect("valid regex"));
    let style = STYLE_RE
        .get_or_init(|| Regex::new(r"(?is)<style\b[^>]*>.*?</style>").expect("valid regex"));
    let nav =
        NAV_RE.get_or_init(|| Regex::new(r"(?is)<nav\b[^>]*>.*?</nav>").expect("valid regex"));
    let comment = COMMENT_RE.get_or_init(|| Regex::new(r"(?is)<!--.*?-->").expect("valid regex"));
    let s = script.replace_all(html, "").to_string();
    let s = style.replace_all(&s, "").to_string();
    let s = nav.replace_all(&s, "").to_string();
    comment.replace_all(&s, "").to_string()
}

/// Retained option shape for `search_austlii` callers. The live SINO search
/// backend is currently unavailable, so these options are accepted only to
/// preserve the CLI/MCP input contract while returning a deterministic
/// diagnostic.
#[derive(Debug, Default, Clone)]
pub(crate) struct SearchAustliiOptions {
    pub(crate) jurisdictions: Option<Vec<String>>,
    pub(crate) limit: Option<usize>,
    pub(crate) sort_by_date: bool,
}

/// Report AustLII live-search unavailability without attempting the retired
/// SINO endpoint.
pub(crate) fn search_austlii(query: &str, opts: SearchAustliiOptions) -> Result<String> {
    if query.trim().is_empty() {
        bail!("search_austlii: query string is empty");
    }
    let _ = (opts.jurisdictions, opts.limit, opts.sort_by_date);
    bail!("{AUSTLII_SEARCH_UNAVAILABLE}");
}

#[cfg(test)]
#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub(crate) title: String,
    pub(crate) fetch_uri: String,
    pub(crate) url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) neutral_citation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reported_citation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) jurisdiction: Option<String>,
}

#[cfg(test)]
fn parse_search_results(html: &str) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let item_selector =
        Selector::parse("li[data-count].multi").expect("valid SINO result selector");
    doc.select(&item_selector)
        .filter_map(parse_search_hit)
        .collect()
}

#[cfg(test)]
fn parse_search_hit(node: ElementRef<'_>) -> Option<SearchHit> {
    let a_selector = Selector::parse("a").expect("valid selector");
    let meta_selector = Selector::parse("p.meta").expect("valid selector");
    let link = node.select(&a_selector).next()?;
    let href_raw = link.value().attr("href")?;
    let title: String = link.text().collect::<String>().trim().to_string();
    if title.is_empty() {
        return None;
    }
    let absolute = absolutise_austlii_url(href_raw);
    let fetch_uri = austlii_url_to_uri(&absolute)?;

    let summary = node
        .select(&meta_selector)
        .next()
        .map(|n| n.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());

    let jurisdiction = jurisdiction_from_uri(&fetch_uri);
    let neutral_citation = extract_neutral_citation(&title);
    let reported_citation = extract_reported_citation(&title);

    Some(SearchHit {
        title,
        fetch_uri,
        url: absolute,
        neutral_citation,
        reported_citation,
        summary,
        jurisdiction,
    })
}

#[cfg(test)]
fn absolutise_austlii_url(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    if href.starts_with('/') {
        return format!("https://www.austlii.edu.au{href}");
    }
    format!("https://www.austlii.edu.au/{href}")
}

/// Translate an AustLII document URL into the corresponding `austlii:`
/// URI. Strips the host + `cgi-bin/viewdoc/` + `cgi-bin/sinodisp/` shims,
/// drops the trailing `.html`, and returns just the `au/...` or `nz/...`
/// canonical path.
#[cfg(test)]
fn austlii_url_to_uri(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed
        .host_str()
        .map(|h| h.contains("austlii.edu.au"))
        .unwrap_or(false)
    {
        return None;
    }
    let mut path = parsed.path().trim_start_matches('/').to_string();
    for prefix in ["cgi-bin/viewdoc/", "cgi-bin/sinodisp/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest.to_string();
        }
    }
    if let Some(rest) = path.strip_suffix(".html") {
        path = rest.to_string();
    }
    if !path.starts_with("au/") && !path.starts_with("nz/") {
        return None;
    }
    Some(format!("austlii:{path}"))
}

#[cfg(test)]
fn jurisdiction_from_uri(uri: &str) -> Option<String> {
    // austlii:au/cases/cth/HCA/... → "cth/HCA"
    let path = uri.strip_prefix("austlii:")?;
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 4 {
        Some(format!("{}/{}", parts[2], parts[3]))
    } else if parts.len() >= 3 {
        Some(parts[2].to_string())
    } else {
        None
    }
}

#[cfg(test)]
fn extract_neutral_citation(title: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\[(\d{4})\]\s+([A-Za-z][A-Za-z0-9]+)\s+(\d+)")
            .expect("valid neutral citation regex")
    });
    re.captures(title)
        .map(|c| format!("[{}] {} {}", &c[1], &c[2], &c[3]))
}

#[cfg(test)]
fn extract_reported_citation(title: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\((\d{4})\)\s+(\d+)\s+([A-Za-z]+)\s+(\d+)")
            .expect("valid reported citation regex")
    });
    re.captures(title)
        .map(|c| format!("({}) {} {} {}", &c[1], &c[2], &c[3], &c[4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_pdf_bytes_recognises_magic() {
        assert!(is_pdf_bytes(b"%PDF-1.4\n..."));
        assert!(!is_pdf_bytes(b"<html>"));
        assert!(!is_pdf_bytes(b""));
        assert!(!is_pdf_bytes(b"%PD"));
    }

    #[test]
    fn search_austlii_reports_endpoint_unavailable() {
        let err = search_austlii("income tax residency", SearchAustliiOptions::default())
            .expect_err("search should fail fast while SINO is unavailable");
        assert!(err.to_string().contains("not a cookie setup issue"));
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
    fn extract_neutral_citation_from_aglc4_title() {
        assert_eq!(
            extract_neutral_citation("Scott v FCT [1966] HCA 48"),
            Some("[1966] HCA 48".to_string())
        );
        assert_eq!(
            extract_neutral_citation("Re X [2024] FCAFC 36 (someone)"),
            Some("[2024] FCAFC 36".to_string())
        );
        assert_eq!(extract_neutral_citation("just a title"), None);
    }

    #[test]
    fn extract_reported_citation_from_title() {
        assert_eq!(
            extract_reported_citation("Scott v FCT (1966) 117 CLR 514"),
            Some("(1966) 117 CLR 514".to_string())
        );
        assert_eq!(
            extract_reported_citation("Mabo (1992) 175 CLR 1"),
            Some("(1992) 175 CLR 1".to_string())
        );
        assert_eq!(extract_reported_citation("[2024] HCA 1"), None);
    }

    #[test]
    fn austlii_url_to_uri_handles_canonical_forms() {
        assert_eq!(
            austlii_url_to_uri(
                "https://www.austlii.edu.au/cgi-bin/viewdoc/au/cases/cth/HCA/1992/23.html"
            ),
            Some("austlii:au/cases/cth/HCA/1992/23".to_string())
        );
        assert_eq!(
            austlii_url_to_uri("https://classic.austlii.edu.au/au/cases/cth/HCA/1992/23.html"),
            Some("austlii:au/cases/cth/HCA/1992/23".to_string())
        );
        assert_eq!(
            austlii_url_to_uri(
                "https://www.austlii.edu.au/cgi-bin/sinodisp/au/cases/cth/HCA/1992/23.html"
            ),
            Some("austlii:au/cases/cth/HCA/1992/23".to_string())
        );
        assert_eq!(
            austlii_url_to_uri("https://www.austlii.edu.au/au/legis/cth/consol_act/itaa1997240"),
            Some("austlii:au/legis/cth/consol_act/itaa1997240".to_string())
        );
        assert_eq!(
            austlii_url_to_uri("https://example.com/au/cases/cth/HCA/1992/23"),
            None
        );
    }

    #[test]
    fn absolutise_austlii_url_handles_relative() {
        assert_eq!(
            absolutise_austlii_url("/cgi-bin/viewdoc/au/cases/cth/HCA/1992/23.html"),
            "https://www.austlii.edu.au/cgi-bin/viewdoc/au/cases/cth/HCA/1992/23.html"
        );
        assert_eq!(
            absolutise_austlii_url("https://www.austlii.edu.au/foo.html"),
            "https://www.austlii.edu.au/foo.html"
        );
        assert_eq!(
            absolutise_austlii_url("//www.austlii.edu.au/foo.html"),
            "https://www.austlii.edu.au/foo.html"
        );
    }

    #[test]
    fn jurisdiction_from_uri_extracts_court_path() {
        assert_eq!(
            jurisdiction_from_uri("austlii:au/cases/cth/HCA/1992/23"),
            Some("cth/HCA".to_string())
        );
        assert_eq!(
            jurisdiction_from_uri("austlii:au/legis/cth/consol_act/itaa1997240"),
            Some("cth/consol_act".to_string())
        );
        assert_eq!(jurisdiction_from_uri("austlii:au"), None);
        assert_eq!(jurisdiction_from_uri("notausti"), None);
    }

    #[test]
    fn derive_pdf_title_handles_case_path() {
        assert_eq!(derive_pdf_title("au/cases/cth/HCA/1966/48"), "HCA 1966/48");
    }

    #[test]
    fn parse_search_results_reads_sino_result_html() {
        let html = r#"<html><body>
            <ul>
              <li data-count="1" class="multi">
                <a href="/cgi-bin/viewdoc/au/cases/cth/HCA/1992/23.html">Mabo v Queensland (No 2) [1992] HCA 23; (1992) 175 CLR 1</a>
                <p class="meta">High Court of Australia - 3 June 1992</p>
              </li>
              <li data-count="2" class="multi">
                <a href="https://www.austlii.edu.au/cgi-bin/viewdoc/au/cases/cth/HCA/1966/48.html">Scott v FCT [1966] HCA 48</a>
                <p class="meta">High Court of Australia - 24 August 1966</p>
              </li>
            </ul>
        </body></html>"#;
        let hits = parse_search_results(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            SearchHit {
                title: "Mabo v Queensland (No 2) [1992] HCA 23; (1992) 175 CLR 1".to_string(),
                fetch_uri: "austlii:au/cases/cth/HCA/1992/23".to_string(),
                url: "https://www.austlii.edu.au/cgi-bin/viewdoc/au/cases/cth/HCA/1992/23.html"
                    .to_string(),
                neutral_citation: Some("[1992] HCA 23".to_string()),
                reported_citation: Some("(1992) 175 CLR 1".to_string()),
                summary: Some("High Court of Australia - 3 June 1992".to_string()),
                jurisdiction: Some("cth/HCA".to_string()),
            }
        );
        assert_eq!(hits[1].fetch_uri, "austlii:au/cases/cth/HCA/1966/48");
        assert_eq!(hits[1].neutral_citation.as_deref(), Some("[1966] HCA 48"));
    }
}
