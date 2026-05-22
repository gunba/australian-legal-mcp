//! AustLII fetch and search support.
//!
//! AustLII is fronted by Cloudflare. Document URLs on
//! `classic.austlii.edu.au` return clean 200s with a browser-grade
//! User-Agent through curl's TLS stack. AustLII's formerly published
//! SINO search CGI is no longer a supported retrieval path, so
//! `search_austlii` searches AustLII's static title indexes through curl
//! with a temporary per-search cookie jar and returns exact `austlii:<path>`
//! fetch URIs.
//!
//! See README.md "AustLII access" for the setup flow.

use crate::browser;
use crate::chunker::{chunk_html, EMBED_MAX_TOKENS};
use crate::cookies;
use crate::ocr;
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::blocking::Client;
use scraper::ElementRef;
use scraper::{Html, Selector};
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

const FETCH_TIMEOUT_SECS: u64 = 30;
const SEARCH_TIMEOUT_SECS: u64 = 20;
const AUSTLII_REFERER: &str = "https://classic.austlii.edu.au/";
const BRAVE_SEARCH_REFERER: &str = "https://search.brave.com/";
const BRAVE_SEARCH_URL: &str = "https://search.brave.com/search";
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const ACCEPT_LANGUAGE: &str = "en-AU,en;q=0.9";
const FETCH_CHALLENGE_HINT: &str =
    "AustLII may be presenting a bot challenge for this document fetch.";
const AUSTLII_NATIVE_SEARCH_UNAVAILABLE: &str =
    "AustLII's native SINO CGI search endpoint (/cgi-bin/sinosrch.cgi) is unavailable";
const TITLE_INDEX_SEARCH_BACKEND: &str = "austlii_title_index";
const WEB_INDEX_SEARCH_BACKEND: &str = "brave_web";
const WEB_FALLBACK_ENV: &str = "ATO_MCP_AUSTLII_WEB_FALLBACK";
const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;
const PDF_TEXT_MIN_CHARS: usize = 100;
const OCR_WARNING: &str =
    "Text extracted via Tesseract OCR — may contain errors. Verify against the canonical source.";

#[derive(Debug, Clone, Copy)]
struct AustliiTitleIndex {
    label: &'static str,
    path: &'static str,
}

const TITLE_INDEXES: &[AustliiTitleIndex] = &[
    AustliiTitleIndex {
        label: "High Court of Australia",
        path: "au/cases/cth/HCA",
    },
    AustliiTitleIndex {
        label: "Federal Court of Australia",
        path: "au/cases/cth/FCA",
    },
    AustliiTitleIndex {
        label: "Full Federal Court of Australia",
        path: "au/cases/cth/FCAFC",
    },
    AustliiTitleIndex {
        label: "Administrative Appeals Tribunal of Australia",
        path: "au/cases/cth/AATA",
    },
    AustliiTitleIndex {
        label: "Commonwealth Consolidated Acts",
        path: "au/legis/cth/consol_act",
    },
    AustliiTitleIndex {
        label: "Commonwealth Consolidated Regulations",
        path: "au/legis/cth/consol_reg",
    },
];

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

    let url = austlii_fetch_url(path);
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

fn austlii_fetch_url(path: &str) -> String {
    if path.ends_with(".html") || path.ends_with(".pdf") || path.ends_with('/') {
        format!("https://classic.austlii.edu.au/{path}")
    } else if is_probable_legislation_root(path) {
        format!("https://classic.austlii.edu.au/{path}/")
    } else {
        format!("https://classic.austlii.edu.au/{path}.html")
    }
}

fn is_probable_legislation_root(path: &str) -> bool {
    if !path.contains("/legis/") {
        return false;
    }
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.is_empty()
        || last.ends_with(".html")
        || last.ends_with(".pdf")
        || matches!(last, "index" | "notes" | "longtitle")
    {
        return false;
    }
    !(last.starts_with('s') || last.starts_with("sch"))
}

fn fetch_austlii_bytes(url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let curl_err = match fetch_austlii_bytes_with_curl(url, user_agent, FETCH_TIMEOUT_SECS, None) {
        Ok(bytes) => return Ok(bytes),
        Err(err) => err,
    };
    fetch_austlii_bytes_with_reqwest(url, user_agent)
        .with_context(|| format!("curl fetch failed first: {curl_err}"))
}

fn fetch_austlii_bytes_with_curl(
    url: &str,
    user_agent: &str,
    timeout_secs: u64,
    cookie_jar: Option<&Path>,
) -> Result<Vec<u8>> {
    let mut command = Command::new("curl");
    command
        .arg("-LfsS")
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("-A")
        .arg(user_agent)
        .arg("-H")
        .arg(format!("Accept: {ACCEPT_HTML}"))
        .arg("-H")
        .arg(format!("Accept-Language: {ACCEPT_LANGUAGE}"))
        .arg("-H")
        .arg(format!("Referer: {AUSTLII_REFERER}"));
    if let Some(cookie_jar) = cookie_jar {
        command.arg("-b").arg(cookie_jar).arg("-c").arg(cookie_jar);
    }
    let output = command
        .arg(url)
        .output()
        .context("running curl for AustLII fetch")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "curl exited with status {} for {url}: {stderr}",
            output.status
        );
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

/// Options accepted by the CLI and MCP `search_austlii` surfaces. Native
/// SINO search is unavailable; the title-index backend uses these for result
/// limiting and post-filtering where possible.
#[derive(Debug, Default, Clone)]
pub(crate) struct SearchAustliiOptions {
    pub(crate) jurisdictions: Option<Vec<String>>,
    pub(crate) limit: Option<usize>,
    pub(crate) sort_by_date: bool,
}

/// Search AustLII's static title indexes. The returned hits are AustLII
/// document URLs normalised to exact `austlii:<path>` fetch URIs. Set
/// ATO_MCP_AUSTLII_WEB_FALLBACK=1 to append a public web-index fallback when
/// title indexes do not produce enough hits.
pub(crate) fn search_austlii(query: &str, opts: SearchAustliiOptions) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("search_austlii: query string is empty");
    }
    let limit = opts.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    let limit = limit.clamp(1, MAX_SEARCH_LIMIT);
    let jurisdictions = opts.jurisdictions.unwrap_or_default();
    let user_agent = browser::detect()
        .context("detecting default browser for AustLII search; set ATO_MCP_BROWSER to override")?
        .user_agent
        .as_str();

    let mut diagnostics = Vec::new();
    let mut hits = direct_neutral_citation_hits(query);
    if hits.len() < limit {
        let title_index =
            search_title_indexes(query, &jurisdictions, limit, user_agent, &mut diagnostics)?;
        hits.extend(title_index);
    }

    let web_fallback_enabled = env_flag_enabled(WEB_FALLBACK_ENV);
    let mut search_backend = TITLE_INDEX_SEARCH_BACKEND.to_string();
    if hits.len() < limit && web_fallback_enabled {
        let backend_query = build_web_index_query(query, &jurisdictions);
        match search_web_index(&backend_query, &jurisdictions, user_agent) {
            Ok(web_hits) => {
                search_backend = format!("{TITLE_INDEX_SEARCH_BACKEND}+{WEB_INDEX_SEARCH_BACKEND}");
                hits.extend(web_hits);
            }
            Err(err) => diagnostics.push(format!("web fallback failed: {err}")),
        }
    }

    let mut seen = HashSet::new();
    hits.retain(|hit| seen.insert(hit.fetch_uri.clone()));
    hits.truncate(limit);

    let mut response = serde_json::Map::new();
    response.insert(
        "source".to_string(),
        JsonValue::String("live_title_index".to_string()),
    );
    response.insert(
        "search_backend".to_string(),
        JsonValue::String(search_backend),
    );
    response.insert(
        "warning".to_string(),
        JsonValue::String(format!(
            "Results come from AustLII title indexes, not full-text native SINO search. {AUSTLII_NATIVE_SEARCH_UNAVAILABLE}. Fetch and verify each returned source."
        )),
    );
    if opts.sort_by_date {
        diagnostics.push("sort_by_date is not supported by AustLII title-index search".to_string());
    }
    if !diagnostics.is_empty() {
        response.insert("diagnostics".to_string(), json!(diagnostics));
    }
    response.insert("hits".to_string(), json!(hits));
    Ok(serde_json::to_string_pretty(&JsonValue::Object(response))?)
}

fn search_web_index(
    backend_query: &str,
    jurisdictions: &[String],
    user_agent: &str,
) -> Result<Vec<SearchHit>> {
    let search_url = build_brave_search_url(backend_query)?;
    let html = fetch_web_index_search(&search_url, user_agent)?;
    let mut hits = parse_brave_search_results(&html);
    if !jurisdictions.is_empty() {
        hits.retain(|hit| hit_matches_jurisdictions(hit, jurisdictions));
    }
    Ok(hits)
}

fn build_web_index_query(query: &str, jurisdictions: &[String]) -> String {
    let mut parts = if jurisdictions.is_empty() {
        vec![
            "site:classic.austlii.edu.au/au/cases".to_string(),
            "OR".to_string(),
            "site:classic.austlii.edu.au/au/legis".to_string(),
            query.trim().to_string(),
        ]
    } else {
        vec![
            "site:classic.austlii.edu.au/au".to_string(),
            query.trim().to_string(),
        ]
    };
    for jurisdiction in jurisdictions {
        let jurisdiction = jurisdiction.trim().trim_matches('/');
        if !jurisdiction.is_empty() {
            parts.push(jurisdiction.to_string());
        }
    }
    parts.join(" ")
}

fn search_title_indexes(
    query: &str,
    jurisdictions: &[String],
    desired_hits: usize,
    user_agent: &str,
    diagnostics: &mut Vec<String>,
) -> Result<Vec<SearchHit>> {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let letters = title_index_letters(query);
    if letters.is_empty() {
        return Ok(Vec::new());
    }

    let mut scored = Vec::new();
    let cookie_jar =
        tempfile::NamedTempFile::new().context("creating temporary AustLII cookie jar")?;
    let env = TitleIndexSearchEnv {
        letters: &letters,
        tokens: &tokens,
        desired_hits,
        user_agent,
        cookie_jar: cookie_jar.path(),
    };
    let (primary, secondary) = ordered_title_index_groups(&tokens, jurisdictions);
    for group in [primary, secondary] {
        search_title_index_group(&group, &env, diagnostics, &mut scored)?;
        if scored.len() >= desired_hits {
            break;
        }
    }
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.hit.title.cmp(&b.hit.title))
    });
    Ok(scored.into_iter().map(|s| s.hit).collect())
}

struct TitleIndexSearchEnv<'a> {
    letters: &'a [String],
    tokens: &'a [String],
    desired_hits: usize,
    user_agent: &'a str,
    cookie_jar: &'a Path,
}

fn search_title_index_group(
    indexes: &[AustliiTitleIndex],
    env: &TitleIndexSearchEnv<'_>,
    diagnostics: &mut Vec<String>,
    scored: &mut Vec<ScoredSearchHit>,
) -> Result<()> {
    for index in indexes {
        if scored.len() >= env.desired_hits {
            break;
        }
        let parent_url = title_index_parent_url(*index)?;
        if let Err(err) = fetch_title_index_bytes(&parent_url, env.user_agent, env.cookie_jar) {
            diagnostics.push(format!(
                "title index cookie prime failed for {}: {err}",
                index.path
            ));
        }
        for letter in env.letters {
            if scored.len() >= env.desired_hits {
                break;
            }
            let url = title_index_url(*index, letter)?;
            let html = match fetch_title_index_bytes(&url, env.user_agent, env.cookie_jar) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(err) => {
                    diagnostics.push(format!(
                        "title index fetch failed for {}: {err}",
                        index.path
                    ));
                    continue;
                }
            };
            scored.extend(parse_title_index_hits(*index, &url, &html, env.tokens));
        }
    }
    Ok(())
}

fn fetch_title_index_bytes(url: &str, user_agent: &str, cookie_jar: &Path) -> Result<Vec<u8>> {
    match fetch_austlii_bytes_with_curl(url, user_agent, SEARCH_TIMEOUT_SECS, Some(cookie_jar)) {
        Ok(bytes) => Ok(bytes),
        Err(first_err) => {
            fetch_austlii_bytes_with_curl(url, user_agent, SEARCH_TIMEOUT_SECS, Some(cookie_jar))
                .with_context(|| format!("first curl attempt failed: {first_err}"))
        }
    }
}

fn ordered_title_index_groups(
    tokens: &[String],
    jurisdictions: &[String],
) -> (Vec<AustliiTitleIndex>, Vec<AustliiTitleIndex>) {
    let indexes = filtered_title_indexes(jurisdictions);
    let (legislation, cases): (Vec<_>, Vec<_>) =
        indexes.into_iter().partition(is_legislation_index);
    if query_likely_legislation(tokens) {
        (legislation, Vec::new())
    } else {
        (cases, legislation)
    }
}

fn is_legislation_index(index: &AustliiTitleIndex) -> bool {
    index.path.contains("/legis/")
}

fn query_likely_legislation(tokens: &[String]) -> bool {
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "act" | "acts" | "regulation" | "regulations" | "rules" | "legislation"
        )
    })
}

fn title_index_url(index: AustliiTitleIndex, letter: &str) -> Result<String> {
    Ok(url::Url::parse(&format!(
        "https://classic.austlii.edu.au/{}/toc-{letter}.html",
        index.path
    ))
    .context("building AustLII title-index URL")?
    .to_string())
}

fn title_index_parent_url(index: AustliiTitleIndex) -> Result<String> {
    Ok(url::Url::parse(&format!(
        "https://classic.austlii.edu.au/{}/index.html",
        index.path
    ))
    .context("building AustLII title-index parent URL")?
    .to_string())
}

#[derive(Debug)]
struct ScoredSearchHit {
    score: usize,
    hit: SearchHit,
}

fn parse_title_index_hits(
    index: AustliiTitleIndex,
    index_url: &str,
    html: &str,
    tokens: &[String],
) -> Vec<ScoredSearchHit> {
    let doc = Html::parse_document(html);
    let a_selector = Selector::parse("li a[href]").expect("valid title index selector");
    let base = match url::Url::parse(index_url) {
        Ok(url) => url,
        Err(_) => return Vec::new(),
    };
    doc.select(&a_selector)
        .filter_map(|link| {
            let title = element_text(link);
            let score = title_match_score(&title, tokens)?;
            let href = link.value().attr("href")?;
            let absolute = title_index_result_url(index, &base, href)?;
            let fetch_uri = austlii_url_to_uri(&absolute)?;
            let url = canonical_url_from_fetch_uri(&fetch_uri)?;
            let summary = Some(format!("AustLII title index: {}", index.label));
            Some(ScoredSearchHit {
                score,
                hit: search_hit(title, fetch_uri, url, summary),
            })
        })
        .collect()
}

fn title_index_result_url(index: AustliiTitleIndex, base: &url::Url, href: &str) -> Option<String> {
    let absolute = base.join(href).ok()?;
    if index.path.contains("/legis/")
        && !href.ends_with('/')
        && !href.ends_with(".html")
        && !href.ends_with(".pdf")
    {
        return Some(format!("{}/", absolute.as_str().trim_end_matches('/')));
    }
    Some(absolute.to_string())
}

fn title_match_score(title: &str, tokens: &[String]) -> Option<usize> {
    let title = title.to_ascii_lowercase();
    let mut matched = 0;
    for token in tokens {
        if title.contains(token) {
            matched += 1;
        }
    }
    if matched == 0 {
        return None;
    }
    let completeness_bonus = if matched == tokens.len() {
        tokens.len()
    } else {
        0
    };
    Some(matched + completeness_bonus)
}

fn filtered_title_indexes(jurisdictions: &[String]) -> Vec<AustliiTitleIndex> {
    if jurisdictions.is_empty() {
        return TITLE_INDEXES.to_vec();
    }
    TITLE_INDEXES
        .iter()
        .copied()
        .filter(|index| {
            jurisdictions
                .iter()
                .any(|jurisdiction| title_index_matches_jurisdiction(*index, jurisdiction))
        })
        .collect()
}

fn title_index_matches_jurisdiction(index: AustliiTitleIndex, jurisdiction: &str) -> bool {
    let wanted = jurisdiction
        .trim()
        .trim_matches('/')
        .to_ascii_lowercase()
        .replace('\\', "/");
    if wanted.is_empty() {
        return false;
    }
    let path = index.path.to_ascii_lowercase();
    path.ends_with(&format!("/{wanted}")) || path.contains(&format!("/{wanted}/"))
}

fn title_index_letters(query: &str) -> Vec<String> {
    let mut letters = Vec::new();
    for token in query_tokens(query) {
        let Some(ch) = token.chars().find(|ch| ch.is_ascii_alphabetic()) else {
            continue;
        };
        let letter = ch.to_ascii_uppercase().to_string();
        if !letters.contains(&letter) {
            letters.push(letter);
        }
        if !letters.is_empty() {
            break;
        }
    }
    letters
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() > 1)
        .map(str::to_ascii_lowercase)
        .filter(|token| !TITLE_SEARCH_STOP_WORDS.contains(&token.as_str()))
        .collect()
}

const TITLE_SEARCH_STOP_WORDS: &[&str] =
    &["and", "for", "in", "no", "of", "or", "the", "to", "v", "vs"];

fn direct_neutral_citation_hits(query: &str) -> Vec<SearchHit> {
    let Some((year, court, number)) = parse_neutral_citation_parts(query) else {
        return Vec::new();
    };
    let Some(path_prefix) = court_path_prefix(&court) else {
        return Vec::new();
    };
    let fetch_uri = format!("austlii:{path_prefix}/{year}/{number}");
    let Some(url) = canonical_url_from_fetch_uri(&fetch_uri) else {
        return Vec::new();
    };
    let title = format!("[{year}] {court} {number}");
    vec![search_hit(
        title,
        fetch_uri,
        url,
        Some("Derived from neutral citation.".to_string()),
    )]
}

fn parse_neutral_citation_parts(query: &str) -> Option<(String, String, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\[(\d{4})\]\s+([A-Za-z][A-Za-z0-9]+)\s+(\d+)")
            .expect("valid neutral citation regex")
    });
    let captures = re.captures(query)?;
    Some((
        captures.get(1)?.as_str().to_string(),
        captures.get(2)?.as_str().to_ascii_uppercase(),
        captures.get(3)?.as_str().to_string(),
    ))
}

fn court_path_prefix(court: &str) -> Option<&'static str> {
    match court {
        "HCA" => Some("au/cases/cth/HCA"),
        "FCA" => Some("au/cases/cth/FCA"),
        "FCAFC" => Some("au/cases/cth/FCAFC"),
        "AATA" => Some("au/cases/cth/AATA"),
        "ART" => Some("au/cases/cth/ART"),
        "FCCA" => Some("au/cases/cth/FCCA"),
        _ => None,
    }
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn build_brave_search_url(query: &str) -> Result<String> {
    let mut url = url::Url::parse(BRAVE_SEARCH_URL).context("parsing Brave search URL")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("source", "web");
    Ok(url.to_string())
}

fn fetch_web_index_search(url: &str, user_agent: &str) -> Result<String> {
    let curl_err = match fetch_web_index_search_with_curl(url, user_agent) {
        Ok(html) => return Ok(html),
        Err(err) => err,
    };
    fetch_web_index_search_with_reqwest(url, user_agent)
        .with_context(|| format!("curl search failed first: {curl_err}"))
}

fn fetch_web_index_search_with_curl(url: &str, user_agent: &str) -> Result<String> {
    let output = Command::new("curl")
        .arg("-LfsS")
        .arg("--compressed")
        .arg("--max-time")
        .arg(SEARCH_TIMEOUT_SECS.to_string())
        .arg("-A")
        .arg(user_agent)
        .arg("-H")
        .arg(format!("Accept: {ACCEPT_HTML}"))
        .arg("-H")
        .arg(format!("Accept-Language: {ACCEPT_LANGUAGE}"))
        .arg("-H")
        .arg(format!("Referer: {BRAVE_SEARCH_REFERER}"))
        .arg(url)
        .output()
        .context("running curl for AustLII web-index search")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "curl exited with status {} for {url}: {stderr}",
            output.status
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn fetch_web_index_search_with_reqwest(url: &str, user_agent: &str) -> Result<String> {
    let client = build_client(user_agent, SEARCH_TIMEOUT_SECS)?;
    let resp = client
        .get(url)
        .header("Accept", ACCEPT_HTML)
        .header("Accept-Language", ACCEPT_LANGUAGE)
        .header("Referer", BRAVE_SEARCH_REFERER)
        .send()
        .with_context(|| format!("fetching web-index search results from {url}"))?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!(
            "web-index search provider returned HTTP {} for {url}; \
             AustLII native SINO search is also unavailable",
            status.as_u16()
        );
    }
    if !status.is_success() {
        bail!(
            "web-index search provider returned HTTP {} for {url}",
            status.as_u16()
        );
    }
    resp.text()
        .with_context(|| format!("reading web-index search response from {url}"))
}

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

fn parse_brave_search_results(html: &str) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let snippet_selector =
        Selector::parse(r#"div.snippet[data-type="web"]"#).expect("valid Brave result selector");
    let mut hits: Vec<SearchHit> = doc
        .select(&snippet_selector)
        .filter_map(parse_brave_search_hit)
        .collect();
    if hits.is_empty() {
        hits = parse_austlii_links(&doc);
    }
    hits
}

fn parse_brave_search_hit(node: ElementRef<'_>) -> Option<SearchHit> {
    let a_selector = Selector::parse("a[href]").expect("valid selector");
    let title_selector =
        Selector::parse(".search-snippet-title").expect("valid Brave title selector");
    let summary_selector =
        Selector::parse(".generic-snippet .content").expect("valid Brave summary selector");

    let link = node.select(&a_selector).find(|a| {
        a.value()
            .attr("href")
            .and_then(austlii_url_to_uri)
            .is_some()
    })?;
    let href = link.value().attr("href")?;
    let fetch_uri = austlii_url_to_uri(href)?;
    let url = canonical_url_from_fetch_uri(&fetch_uri)?;

    let title = link
        .select(&title_selector)
        .next()
        .map(element_text)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fetch_uri.clone());
    let summary = node
        .select(&summary_selector)
        .next()
        .map(element_text)
        .filter(|s| !s.is_empty());

    Some(search_hit(title, fetch_uri, url, summary))
}

fn parse_austlii_links(doc: &Html) -> Vec<SearchHit> {
    let a_selector = Selector::parse("a[href]").expect("valid selector");
    doc.select(&a_selector)
        .filter_map(|link| {
            let href = link.value().attr("href")?;
            let fetch_uri = austlii_url_to_uri(href)?;
            let url = canonical_url_from_fetch_uri(&fetch_uri)?;
            let title = element_text(link);
            let title = if title.is_empty() {
                fetch_uri.clone()
            } else {
                title
            };
            Some(search_hit(title, fetch_uri, url, None))
        })
        .collect()
}

fn search_hit(title: String, fetch_uri: String, url: String, summary: Option<String>) -> SearchHit {
    let jurisdiction = jurisdiction_from_uri(&fetch_uri);
    let neutral_citation = extract_neutral_citation(&title);
    let reported_citation = extract_reported_citation(&title);
    SearchHit {
        title,
        fetch_uri,
        url,
        neutral_citation,
        reported_citation,
        summary,
        jurisdiction,
    }
}

fn element_text(node: ElementRef<'_>) -> String {
    let joined = node.text().collect::<Vec<_>>().join(" ");
    normalize_ws(&joined)
}

fn normalize_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_url_from_fetch_uri(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("austlii:")?;
    Some(austlii_fetch_url(path))
}

fn hit_matches_jurisdictions(hit: &SearchHit, jurisdictions: &[String]) -> bool {
    let uri = hit.fetch_uri.to_ascii_lowercase();
    let jurisdiction = hit
        .jurisdiction
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    jurisdictions.iter().any(|raw| {
        let needle = raw
            .trim()
            .trim_matches('/')
            .to_ascii_lowercase()
            .replace('\\', "/");
        if needle.is_empty() {
            return false;
        }
        jurisdiction == needle
            || jurisdiction.starts_with(&format!("{needle}/"))
            || uri.contains(&format!("/{needle}/"))
            || uri.ends_with(&format!("/{needle}"))
    })
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
    for prefix in ["cgi-bin/viewdoc/", "cgi-bin/sinodisp/", "cgi-bin/viewdb/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            path = rest.to_string();
        }
    }
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    if path.ends_with('/') {
        path.push_str("index");
    }
    if let Some(rest) = path.strip_suffix(".html") {
        path = rest.to_string();
    }
    if !path.starts_with("au/") && !path.starts_with("nz/") {
        return None;
    }
    Some(format!("austlii:{path}"))
}

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

fn extract_neutral_citation(title: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\[(\d{4})\]\s+([A-Za-z][A-Za-z0-9]+)\s+(\d+)")
            .expect("valid neutral citation regex")
    });
    re.captures(title)
        .map(|c| format!("[{}] {} {}", &c[1], &c[2], &c[3]))
}

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
    fn parse_brave_search_results_reads_austlii_hits() {
        let html = r#"<html><body>
          <div class="snippet" data-type="web">
            <a href="https://classic.austlii.edu.au/au/cases/cth/HCA/1992/23.html">
              <div class="search-snippet-title">Mabo v Queensland (No 2) [1992] HCA 23; (1992) 175 CLR 1</div>
            </a>
            <div class="generic-snippet"><div class="content">High Court of Australia result snippet.</div></div>
          </div>
          <div class="snippet" data-type="web">
            <a href="https://example.com/not-austlii">Ignore me</a>
          </div>
        </body></html>"#;
        let hits = parse_brave_search_results(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fetch_uri, "austlii:au/cases/cth/HCA/1992/23");
        assert_eq!(
            hits[0].url,
            "https://classic.austlii.edu.au/au/cases/cth/HCA/1992/23.html"
        );
        assert_eq!(hits[0].neutral_citation.as_deref(), Some("[1992] HCA 23"));
        assert_eq!(
            hits[0].reported_citation.as_deref(),
            Some("(1992) 175 CLR 1")
        );
        assert_eq!(
            hits[0].summary.as_deref(),
            Some("High Court of Australia result snippet.")
        );
    }

    #[test]
    fn build_web_index_query_prioritises_cases_and_legislation() {
        assert_eq!(
            build_web_index_query("privacy act", &[]),
            "site:classic.austlii.edu.au/au/cases OR site:classic.austlii.edu.au/au/legis privacy act"
        );
        assert_eq!(
            build_web_index_query("mabo", &["cth/HCA".to_string()]),
            "site:classic.austlii.edu.au/au mabo cth/HCA"
        );
    }

    #[test]
    fn parse_title_index_hits_normalises_legislation_directories() {
        let index = AustliiTitleIndex {
            label: "Commonwealth Consolidated Acts",
            path: "au/legis/cth/consol_act",
        };
        let html = r#"<html><body><ul>
            <li><a href="pa1988108">PRIVACY ACT 1988</a></li>
        </ul></body></html>"#;
        let hits = parse_title_index_hits(
            index,
            "https://classic.austlii.edu.au/au/legis/cth/consol_act/toc-P.html",
            html,
            &query_tokens("Privacy Act"),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].hit.fetch_uri,
            "austlii:au/legis/cth/consol_act/pa1988108/index"
        );
        assert_eq!(
            hits[0].hit.url,
            "https://classic.austlii.edu.au/au/legis/cth/consol_act/pa1988108/index.html"
        );
    }

    #[test]
    fn direct_neutral_citation_hits_builds_fetch_uri() {
        let hits = direct_neutral_citation_hits("Mabo [1992] HCA 23");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fetch_uri, "austlii:au/cases/cth/HCA/1992/23");
    }

    #[test]
    fn title_index_letters_use_first_significant_token() {
        assert_eq!(title_index_letters("Privacy Act"), vec!["P".to_string()]);
        assert_eq!(title_index_letters("the Mabo case"), vec!["M".to_string()]);
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
            austlii_url_to_uri(
                "https://www.austlii.edu.au/cgi-bin/viewdb/au/legis/cth/consol_act/pa1988108/"
            ),
            Some("austlii:au/legis/cth/consol_act/pa1988108/index".to_string())
        );
        assert_eq!(
            austlii_url_to_uri("https://classic.austlii.edu.au/au/journals/JlATax/2021/4.pdf"),
            Some("austlii:au/journals/JlATax/2021/4.pdf".to_string())
        );
        assert_eq!(
            austlii_url_to_uri("https://example.com/au/cases/cth/HCA/1992/23"),
            None
        );
    }

    #[test]
    fn austlii_fetch_url_preserves_known_extensions() {
        assert_eq!(
            austlii_fetch_url("au/cases/cth/HCA/1992/23"),
            "https://classic.austlii.edu.au/au/cases/cth/HCA/1992/23.html"
        );
        assert_eq!(
            austlii_fetch_url("au/journals/JlATax/2021/4.pdf"),
            "https://classic.austlii.edu.au/au/journals/JlATax/2021/4.pdf"
        );
        assert_eq!(
            austlii_fetch_url("au/legis/cth/consol_act/pa1988108"),
            "https://classic.austlii.edu.au/au/legis/cth/consol_act/pa1988108/"
        );
        assert_eq!(
            austlii_fetch_url("au/legis/cth/consol_act/pa1988108/s6"),
            "https://classic.austlii.edu.au/au/legis/cth/consol_act/pa1988108/s6.html"
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
