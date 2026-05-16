//! Build-time content extractors: definitions, doc-navigation anchors,
//! leading-heading titles + EM front matter, currency/withdrawal markers,
//! image assets, and doc_id-derived metadata.

use crate::html::{
    assets_html_escape, doc_id_from_ato_link, extract_attr,
};
use crate::pit_to_date;
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

// ----- Definition extraction (port of src/ato_mcp/indexer/definitions.py) -----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DefinitionChunk {
    pub(crate) ord: i64,
    pub(crate) anchor: Option<String>,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Definition {
    pub(crate) definition_id: String,
    pub(crate) term: String,
    pub(crate) norm_term: String,
    pub(crate) doc_id: String,
    pub(crate) source_title: String,
    pub(crate) source_type: String,
    pub(crate) scope: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) ord: i64,
    pub(crate) body: String,
}

pub(crate) fn normalize_definition_term(term: &str) -> String {
    let t: String = term.replace("\\*", "*").replace("\\&", "&");
    let t = t.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ':' | '*'));
    let mut out = String::with_capacity(t.len());
    let mut last_ws = false;
    for c in t.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.to_lowercase()
}

pub(crate) fn defs_clean_term(term: &str) -> String {
    let s = term.replace('\n', " ");
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.trim_matches(|c: char| matches!(c, ' ' | ':' | '*'))
        .to_string()
}

pub(crate) fn defs_clean_body(body: &str) -> String {
    let trimmed = body.trim();
    // Collapse runs of 3+ newlines to two.
    let re = Regex::new(r"\n{3,}").unwrap();
    re.replace_all(trimmed, "\n\n").to_string()
}

pub(crate) fn defs_definition_id(doc_id: &str, ord: i64, term: &str, body: &str, offset: usize) -> String {
    let mut h = Sha256::new();
    h.update(doc_id.as_bytes());
    h.update(b"\0");
    h.update(ord.to_string().as_bytes());
    h.update(b"\0");
    h.update(offset.to_string().as_bytes());
    h.update(b"\0");
    h.update(normalize_definition_term(term).as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex[..20].to_string()
}

pub(crate) fn defs_scope_from_title(title: &str, source_type: &str) -> Option<String> {
    if title.contains(" s ") {
        Some(title.to_string())
    } else if !source_type.is_empty() {
        Some(source_type.to_string())
    } else {
        None
    }
}

pub(crate) fn extract_definitions(
    doc_id: &str,
    source_title: &str,
    source_type: &str,
    chunks: &[DefinitionChunk],
) -> Vec<Definition> {
    // Match `***term***` markers — same regex as definitions.py:_TERM_RE.
    let term_re = Regex::new(r"\*\*\*\s*([^*\n][^*]{0,180}?)\s*\*\*\*").unwrap();
    let cue_re = Regex::new(
        r"(?im)^\s*(?:,?\s*of\b|,?\s*in relation\b|:|means\b|includes\b|has\b|is\b|\(Repealed\b)",
    )
    .unwrap();

    let mut out: Vec<Definition> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();

    for chunk in chunks {
        let matches: Vec<regex::Match> = term_re.find_iter(&chunk.text).collect();
        // Capture groups for each match — need them to extract the term text.
        let captures: Vec<regex::Captures> = term_re.captures_iter(&chunk.text).collect();
        if matches.is_empty() {
            continue;
        }
        for (idx, m) in matches.iter().enumerate() {
            let term_raw = captures[idx].get(1).map(|c| c.as_str()).unwrap_or("");
            let term = defs_clean_term(term_raw);
            if term.is_empty() {
                continue;
            }
            let next_start = matches
                .get(idx + 1)
                .map(|m| m.start())
                .unwrap_or(chunk.text.len());
            let body_start = m.end();
            let body_slice = &chunk.text[body_start..next_start];
            let mut body = defs_clean_body(body_slice);
            // Handle "***term*** or ***other***" / "***term*** and ***other***" pattern:
            // body collapses to "or"/"and"; the real definition follows the next term marker.
            let body_lc = body.to_lowercase();
            if (body_lc == "or" || body_lc == "and") && idx + 1 < matches.len() {
                let next_m = &matches[idx + 1];
                let next_next_start = matches
                    .get(idx + 2)
                    .map(|m| m.start())
                    .unwrap_or(chunk.text.len());
                body = defs_clean_body(&chunk.text[next_m.end()..next_next_start]);
            }
            if body.len() < 4 || cue_re.find(&body).is_none() {
                continue;
            }
            let norm = normalize_definition_term(&term);
            let key = (norm.clone(), doc_id.to_string(), body.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            out.push(Definition {
                definition_id: defs_definition_id(doc_id, chunk.ord, &term, &body, m.start()),
                term: term.clone(),
                norm_term: norm,
                doc_id: doc_id.to_string(),
                source_title: source_title.to_string(),
                source_type: source_type.to_string(),
                scope: defs_scope_from_title(source_title, source_type),
                anchor: chunk.anchor.clone(),
                ord: chunk.ord,
                body,
            });
        }
    }
    out
}

// ----- Doc-navigation anchors (port of src/ato_mcp/indexer/anchors.py) -----
//
// Walks cleaned HTML for a single doc and classifies every <a href> into
// one of three kinds, mirroring the Python module: in_doc (#X target inside
// this doc), sister (cross-doc link, no PiT), history (cross-doc with PiT
// timestamp pointing at a historical version we don't store).

pub(crate) const ANCHORS_SENTINEL_PITS: &[&str] = &["99991231235958", "10010101000001"];

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AnchorRef {
    pub(crate) kind: String,
    pub(crate) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_doc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_pit: Option<String>,
}

pub(crate) fn anchors_collect_targets(doc: &scraper::Html) -> std::collections::HashSet<String> {
    use scraper::Selector;
    let mut targets = std::collections::HashSet::new();
    let a_name = Selector::parse("a[name]").unwrap();
    for el in doc.select(&a_name) {
        if let Some(name) = el.value().attr("name") {
            if !name.is_empty() {
                targets.insert(name.to_string());
            }
        }
    }
    let with_id = Selector::parse("[id]").unwrap();
    for el in doc.select(&with_id) {
        if let Some(nid) = el.value().attr("id") {
            if !nid.is_empty() {
                targets.insert(nid.to_string());
            }
        }
    }
    targets
}

pub(crate) fn anchors_find_ancestor<'a>(
    node: scraper::ElementRef<'a>,
    tags: &[&str],
) -> Option<scraper::ElementRef<'a>> {
    let mut current = node.parent();
    while let Some(p) = current {
        if let Some(el) = scraper::ElementRef::wrap(p) {
            if tags.contains(&el.value().name()) {
                return Some(el);
            }
        }
        current = p.parent();
    }
    None
}

pub(crate) fn anchors_node_text(node: scraper::ElementRef) -> String {
    let mut out = String::new();
    for s in node.text() {
        out.push_str(s);
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut last_ws = true;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_ws {
                collapsed.push(' ');
                last_ws = true;
            }
        } else {
            collapsed.push(c);
            last_ws = false;
        }
    }
    collapsed.trim().to_string()
}

pub(crate) fn anchors_sibling_cells_text(a: scraper::ElementRef) -> String {
    let row = match anchors_find_ancestor(a, &["tr"]) {
        Some(r) => r,
        None => return String::new(),
    };
    let own_cell = anchors_find_ancestor(a, &["td", "th"]);
    let cell_sel = scraper::Selector::parse("td, th").unwrap();
    let mut parts: Vec<String> = Vec::new();
    for cell in row.select(&cell_sel) {
        if let Some(own) = own_cell {
            if cell.id() == own.id() {
                continue;
            }
        }
        let text = anchors_node_text(cell);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(" ").trim().to_string()
}

pub(crate) fn anchors_resolve_label(a: scraper::ElementRef, default_date: Option<&str>) -> String {
    let own = anchors_node_text(a);
    let sibling = anchors_sibling_cells_text(a);
    let mut parts: Vec<String> = Vec::new();
    if !sibling.is_empty() {
        parts.push(sibling);
    }
    if !own.is_empty() && !parts.iter().any(|p| p == &own) {
        parts.push(own);
    }
    let mut label = parts.join(" ").trim().to_string();
    if let Some(date) = default_date {
        label = if label.is_empty() {
            date.to_string()
        } else {
            format!("{label} ({date})")
        };
    }
    if label.is_empty() {
        "(unnamed)".to_string()
    } else {
        label
    }
}

pub(crate) fn extract_anchors(html: &str, source_doc_id: &str) -> Vec<AnchorRef> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = scraper::Html::parse_document(html);
    let targets = anchors_collect_targets(&doc);
    let mut refs: Vec<AnchorRef> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, Option<String>, String)> =
        std::collections::HashSet::new();

    let a_sel = scraper::Selector::parse("a[href]").unwrap();
    for a in doc.select(&a_sel) {
        let href = a.value().attr("href").unwrap_or("");
        if let Some(target) = href.strip_prefix('#') {
            if target.is_empty() || !targets.contains(target) {
                continue;
            }
            let label = anchors_resolve_label(a, None);
            let key = (
                "in_doc".to_string(),
                target.to_string(),
                None,
                label.clone(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            refs.push(AnchorRef {
                kind: "in_doc".to_string(),
                label,
                target_anchor: Some(target.to_string()),
                target_doc_id: None,
                target_pit: None,
            });
            continue;
        }
        let resolved = doc_id_from_ato_link(href);
        let Some((target_doc_id, mut pit, _view)) = resolved else {
            continue;
        };
        if let Some(p) = pit.as_ref() {
            if ANCHORS_SENTINEL_PITS.iter().any(|s| *s == p) {
                pit = None;
            }
        }
        if let Some(p) = pit {
            let date = pit_to_date(&p).unwrap_or_else(|| p.trim().to_string());
            let label = anchors_resolve_label(a, Some(&date));
            let key = (
                "history".to_string(),
                target_doc_id.clone(),
                Some(p.clone()),
                label.clone(),
            );
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            refs.push(AnchorRef {
                kind: "history".to_string(),
                label,
                target_anchor: None,
                target_doc_id: Some(target_doc_id),
                target_pit: Some(p),
            });
            continue;
        }
        if target_doc_id == source_doc_id {
            continue;
        }
        let label = anchors_resolve_label(a, None);
        let key = (
            "sister".to_string(),
            target_doc_id.clone(),
            None,
            label.clone(),
        );
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        refs.push(AnchorRef {
            kind: "sister".to_string(),
            label,
            target_anchor: None,
            target_doc_id: Some(target_doc_id),
            target_pit: None,
        });
    }
    refs
}

// ----- Title composition + EM front matter + anchor collection -----
// Ports of src/ato_mcp/indexer/extract.py:
//   _collect_anchors, _leading_headings, _compose_title,
//   _collect_em_front_matter

pub(crate) fn extract_leading_headings(container_html: &str) -> Vec<String> {
    use scraper::Selector;
    let frag = scraper::Html::parse_fragment(container_html);
    let heading_tags = ["h1", "h2", "h3", "h4", "h5", "h6"];
    let nested_heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").unwrap();

    let mut out: Vec<String> = Vec::new();
    let mut dived = false;
    // Walk direct children of the fragment root (which is a wrapper).
    // scraper's parse_fragment wraps in a synthetic root; we need to find
    // the "real" first-level container's children.
    let root = frag.root_element();
    let direct_children: Vec<_> = root
        .children()
        .filter_map(scraper::ElementRef::wrap)
        .collect();
    // If the root has a single element child, treat that as the container.
    let walk_children: Vec<scraper::ElementRef> = if direct_children.len() == 1 {
        direct_children[0]
            .children()
            .filter_map(scraper::ElementRef::wrap)
            .collect()
    } else {
        direct_children
    };
    for child in walk_children {
        let tag = child.value().name();
        if heading_tags.contains(&tag) {
            let text = anchors_node_text(child);
            if !text.is_empty() {
                out.push(text);
            }
            continue;
        }
        if dived {
            break;
        }
        // Wrapper that only carries headings? Dive once.
        let nested: Vec<_> = child.select(&nested_heading_sel).collect();
        let non_heading_len = anchors_node_text(child).len();
        if !nested.is_empty() && non_heading_len <= 800 {
            for h in nested {
                let t = anchors_node_text(h);
                if !t.is_empty() {
                    out.push(t);
                }
            }
            dived = true;
            continue;
        }
        if !anchors_node_text(child).is_empty() {
            break;
        }
    }
    out.into_iter().take(4).collect()
}

// [IB-07] Titles are composed from leading headings with adjacent prefix
// overlap suppression, then fall back to source title/doc_id in the build path.
pub(crate) fn extract_compose_title(headings: &[String]) -> Option<String> {
    let cleaned: Vec<String> = headings
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() == 1 {
        return Some(cleaned[0].clone());
    }
    let mut out: Vec<String> = Vec::new();
    for h in cleaned {
        if let Some(last) = out.last() {
            let h_lc = h.to_lowercase();
            let last_lc = last.to_lowercase();
            if last_lc.starts_with(&h_lc) || h_lc.starts_with(&last_lc) {
                continue;
            }
        }
        out.push(h);
    }
    Some(out.join(" — "))
}

pub(crate) fn extract_em_front_matter(container_html: &str) -> (Vec<String>, Option<String>) {
    use scraper::Selector;
    let frag = scraper::Html::parse_fragment(container_html);
    let lawfront_sel = Selector::parse("#Lawfront").unwrap();
    let Some(front) = frag.select(&lawfront_sel).next() else {
        return (Vec::new(), None);
    };
    let strong_sel = Selector::parse("strong").unwrap();
    let mut refs: Vec<String> = Vec::new();
    let mut phrase: Option<String> = None;
    for child in front.children().filter_map(scraper::ElementRef::wrap) {
        let tag = child.value().name();
        match tag {
            "div" => {
                let classes: Vec<&str> = child
                    .value()
                    .attr("class")
                    .unwrap_or("")
                    .split_whitespace()
                    .collect();
                if classes.contains(&"ref") {
                    if let Some(s) = child.select(&strong_sel).next() {
                        let t = anchors_node_text(s);
                        if !t.is_empty() {
                            refs.push(t);
                        }
                    }
                }
            }
            "p" if phrase.is_none() => {
                if let Some(s) = child.select(&strong_sel).next() {
                    let t = anchors_node_text(s);
                    if t.to_lowercase().starts_with("explanatory ") {
                        phrase = Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    (refs, phrase)
}

// ----- Currency / withdrawal extraction (port of extract.py:extract_currency) -----
//
// Best-effort currency / supersession extraction from raw page HTML, mirroring
// src/ato_mcp/indexer/extract.py:extract_currency and its helpers. Each
// CurrencyInfo field is filled independently — alert panel beats body prose
// beats timeline table beats title-suffix sentinel.

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CurrencyInfo {
    pub(crate) withdrawn_date: Option<String>,
    pub(crate) superseded_by: Option<String>,
    pub(crate) replaces: Option<String>,
}

pub(crate) const CURRENCY_TITLE_SUFFIX_SENTINEL: &str = "0001-01-01";

pub(crate) fn currency_months() -> &'static std::collections::HashMap<&'static str, u32> {
    static MAP: std::sync::OnceLock<std::collections::HashMap<&'static str, u32>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for (i, name) in [
            "january",
            "february",
            "march",
            "april",
            "may",
            "june",
            "july",
            "august",
            "september",
            "october",
            "november",
            "december",
        ]
        .iter()
        .enumerate()
        {
            m.insert(*name, (i + 1) as u32);
        }
        m
    })
}

pub(crate) fn currency_normalise_date(raw: &str) -> Option<String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // "31 October 2025"
    let prose = Regex::new(r"^(\d{1,2})\s+([A-Za-z]+)\s+(\d{4})$").unwrap();
    if let Some(c) = prose.captures(&s) {
        let day: u32 = c.get(1)?.as_str().parse().ok()?;
        let month_name = c.get(2)?.as_str().to_lowercase();
        let year: u32 = c.get(3)?.as_str().parse().ok()?;
        let month = *currency_months().get(month_name.as_str())?;
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    }
    // "31/10/2025"
    let dmy = Regex::new(r"^(\d{1,2})/(\d{1,2})/(\d{4})$").unwrap();
    if let Some(c) = dmy.captures(&s) {
        let day: u32 = c.get(1)?.as_str().parse().ok()?;
        let month: u32 = c.get(2)?.as_str().parse().ok()?;
        let year: u32 = c.get(3)?.as_str().parse().ok()?;
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    }
    // "2025-10-31"
    let iso = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").unwrap();
    if iso.is_match(&s) {
        return Some(s);
    }
    None
}

pub(crate) fn currency_normalise_citation(raw: &str) -> Option<String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) const CURRENCY_RULING_SERIES: &str =
    "SMSFRB|SMSFR|SMSFD|GSTR|GSTD|FBTR|WETR|WETD|LCR|SGR|FTR|PCG|LCG|PRR|CLR|COG|TXD|TPA|FBT|GII|CR|PR|TR|TD|MT|TA|LI|LG|WT|IT";

pub(crate) fn currency_citation_pattern() -> String {
    format!(
        r"(?:{}|ATO\s+ID|PS\s+LA|SMSFRB)\s+\d{{1,4}}/D?\d+[A-Z0-9]*",
        CURRENCY_RULING_SERIES
    )
}

pub(crate) fn currency_date_prose_pattern() -> &'static str {
    r"\d{1,2}\s+(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{4}|\d{1,2}/\d{1,2}/\d{4}|\d{4}-\d{2}-\d{2}"
}

pub(crate) fn currency_re_withdrawn_prose() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let date = currency_date_prose_pattern();
        let prefix = r"\b(?:was|is|were|are|been|being|has\s+been|have\s+been)?\s*withdrawn(?:\s+(?:with\s+effect)?\s*(?:from|on|as\s+of))?\s+";
        Regex::new(&format!(r"(?i){prefix}(?P<date>{date})")).unwrap()
    })
}

pub(crate) fn currency_re_withdrawn_by_prose() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let date = currency_date_prose_pattern();
        let prefix = r"\b(?:was|is|were|are|been|being|has\s+been|have\s+been)?\s*withdrawn(?:\s+(?:with\s+effect)?\s*(?:from|on|as\s+of))?\s+";
        let cite = currency_citation_pattern();
        Regex::new(&format!(
            r"(?i){prefix}(?P<date>{date})\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
        )).unwrap()
    })
}

pub(crate) fn currency_re_replacement_verb() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(replaces|replaced\s+by|supersed(?:e|es|ed|ing)|in\s+lieu\s+of)\b").unwrap()
    })
}

pub(crate) fn currency_re_self_anchor() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\bthis\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\b").unwrap()
    })
}

pub(crate) fn currency_re_sentence_split() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| Regex::new(r"[.;\n]+").unwrap())
}

pub(crate) fn currency_re_replaces_prose() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let cite = currency_citation_pattern();
        Regex::new(&format!(
            r"(?i)\b(?:this\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\s+)?replaces\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
        )).unwrap()
    })
}

pub(crate) fn currency_re_superseded_by_prose() -> &'static Regex {
    static R: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let cite = currency_citation_pattern();
        Regex::new(&format!(
            r"(?i)\b(?:replaced|superseded)\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+(?P<cite>{cite})"
        )).unwrap()
    })
}

pub(crate) fn currency_withdrawal_fragment_is_self(fragment: &str, withdrawn_start: usize) -> bool {
    let rep = currency_re_replacement_verb();
    if !rep.is_match(fragment) {
        return true;
    }
    let anchor = currency_re_self_anchor();
    let Some(am) = anchor.find(fragment) else {
        return false;
    };
    let between_start = am.end();
    if between_start > withdrawn_start {
        return false;
    }
    let between = &fragment[between_start..withdrawn_start];
    !rep.is_match(between)
}

pub(crate) fn currency_extract_self_withdrawn_date(text: &str) -> Option<String> {
    let split = currency_re_sentence_split();
    let withdrawn = currency_re_withdrawn_prose();
    for fragment in split.split(text) {
        let Some(m) = withdrawn.captures(fragment) else {
            continue;
        };
        if !currency_withdrawal_fragment_is_self(fragment, m.get(0)?.start()) {
            continue;
        }
        let date = m.name("date")?.as_str();
        if let Some(iso) = currency_normalise_date(date) {
            return Some(iso);
        }
    }
    None
}

pub(crate) fn currency_extract_self_withdrawn_by(text: &str) -> Option<String> {
    let split = currency_re_sentence_split();
    let withdrawn_by = currency_re_withdrawn_by_prose();
    for fragment in split.split(text) {
        let Some(m) = withdrawn_by.captures(fragment) else {
            continue;
        };
        if !currency_withdrawal_fragment_is_self(fragment, m.get(0)?.start()) {
            continue;
        }
        let cite = m.name("cite")?.as_str();
        if let Some(c) = currency_normalise_citation(cite) {
            return Some(c);
        }
    }
    None
}

pub(crate) fn currency_alert_text(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("div.alert").unwrap();
    let parts: Vec<String> = doc
        .select(&sel)
        .map(|el| {
            let raw = el.text().collect::<String>();
            raw.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(" \n ")
}

pub(crate) fn currency_body_text(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    for sel_str in &["#LawBody", "#LawContent"] {
        let sel = scraper::Selector::parse(sel_str).unwrap();
        if let Some(el) = doc.select(&sel).next() {
            return anchors_node_text(el);
        }
    }
    if let Ok(body_sel) = scraper::Selector::parse("body") {
        if let Some(el) = doc.select(&body_sel).next() {
            return anchors_node_text(el);
        }
    }
    String::new()
}

pub(crate) fn currency_date_from_history_table(html: &str) -> Option<String> {
    let doc = scraper::Html::parse_document(html);
    let timeline_sel = scraper::Selector::parse("a[name='LawTimeLine']").unwrap();
    let timeline = doc.select(&timeline_sel).next()?;
    // Walk up to enclosing panel or table — at most 8 hops.
    let mut current = timeline.parent();
    let mut panel: Option<scraper::ElementRef> = None;
    for _ in 0..8 {
        let Some(p) = current else { break };
        if let Some(el) = scraper::ElementRef::wrap(p) {
            let tag = el.value().name();
            let classes: Vec<&str> = el
                .value()
                .attr("class")
                .unwrap_or("")
                .split_whitespace()
                .collect();
            if tag == "table" || classes.contains(&"panel") {
                panel = Some(el);
                break;
            }
        }
        current = p.parent();
    }
    let panel = panel?;
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("td").unwrap();
    let mut latest: Option<String> = None;
    for row in panel.select(&row_sel) {
        let cells: Vec<scraper::ElementRef> = row.select(&cell_sel).collect();
        if cells.len() < 2 {
            continue;
        }
        let mut date_cell: Option<String> = None;
        let mut label_cell: Option<String> = None;
        for cell in &cells {
            let cls = cell.value().attr("class").unwrap_or("").to_lowercase();
            let text = anchors_node_text(*cell);
            if cls.contains("date") && date_cell.is_none() {
                date_cell = Some(text);
            } else if date_cell.is_some() && label_cell.is_none() {
                label_cell = Some(text.to_lowercase());
            }
        }
        let Some(date) = date_cell else { continue };
        let label = label_cell.unwrap_or_else(|| {
            let last = cells.last().unwrap();
            anchors_node_text(*last).to_lowercase()
        });
        if !label.contains("withdraw") {
            continue;
        }
        if let Some(iso) = currency_normalise_date(&date) {
            latest = Some(iso);
        }
    }
    latest
}

pub(crate) fn currency_scan_text(text: &str) -> (Option<String>, Option<String>, Option<String>) {
    if text.is_empty() {
        return (None, None, None);
    }
    let withdrawn_date = currency_extract_self_withdrawn_date(text);
    let mut superseded_by = currency_extract_self_withdrawn_by(text);
    if superseded_by.is_none() {
        let sup = currency_re_superseded_by_prose();
        if let Some(m) = sup.captures(text) {
            if let Some(cite) = m.name("cite") {
                superseded_by = currency_normalise_citation(cite.as_str());
            }
        }
    }
    let mut replaces: Option<String> = None;
    let rep = currency_re_replaces_prose();
    if let Some(m) = rep.captures(text) {
        if let Some(cite) = m.name("cite") {
            replaces = currency_normalise_citation(cite.as_str());
        }
    }
    (withdrawn_date, superseded_by, replaces)
}

pub(crate) fn currency_has_withdrawn_title_suffix(html: &str) -> bool {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("h1, h2, h3").unwrap();
    for el in doc.select(&sel) {
        let text = anchors_node_text(el).to_lowercase();
        if text.contains("(withdrawn)") {
            return true;
        }
    }
    false
}

pub(crate) fn extract_currency(html: &str) -> CurrencyInfo {
    if html.trim().is_empty() {
        return CurrencyInfo::default();
    }
    let alert_text = currency_alert_text(html);
    let body_text = currency_body_text(html);
    let (a_w, a_s, a_r) = currency_scan_text(&alert_text);
    let (p_w, p_s, p_r) = currency_scan_text(&body_text);

    let mut withdrawn_date = a_w;
    if withdrawn_date.is_none() && p_w.is_some() {
        withdrawn_date = p_w;
    }
    if withdrawn_date.is_none() {
        withdrawn_date = currency_date_from_history_table(html);
    }
    if withdrawn_date.is_none() && currency_has_withdrawn_title_suffix(html) {
        withdrawn_date = Some(CURRENCY_TITLE_SUFFIX_SENTINEL.to_string());
    }
    let mut superseded_by = a_s;
    if superseded_by.is_none() && p_s.is_some() {
        superseded_by = p_s;
    }
    let mut replaces = a_r;
    if replaces.is_none() && p_r.is_some() {
        replaces = p_r;
    }
    CurrencyInfo {
        withdrawn_date,
        superseded_by,
        replaces,
    }
}

// ----- Image asset extraction (port of extract.py:_rewrite_images_html) -----
//
// Walks <img> tags in cleaned HTML, reads referenced files (src resolved
// against source_path's parent), SHA256-hashes + base64-encodes them, emits
// ExtractedAsset records and rewrites the HTML so each <img> becomes a
// <span data-asset-ref="..." data-media-type="...">[image: alt]</span>.
// Mirrors src/ato_mcp/indexer/extract.py:_rewrite_images_html.

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExtractedAsset {
    pub(crate) asset_ref: String,
    pub(crate) source_path: String,
    pub(crate) relative_path: String,
    pub(crate) media_type: Option<String>,
    pub(crate) alt: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) sha256: String,
    pub(crate) size: u64,
    pub(crate) data_b64: String,
}

pub(crate) fn assets_url_encode_doc_id(doc_id: &str) -> String {
    let mut out = String::with_capacity(doc_id.len() * 3);
    for byte in doc_id.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn assets_asset_ref(doc_id: &str, ordinal: u32) -> String {
    format!(
        "ato-image://{}/{}",
        assets_url_encode_doc_id(doc_id),
        ordinal
    )
}

pub(crate) fn assets_guess_media_type(src: &str) -> Option<String> {
    let path = src.split('?').next().unwrap_or(src);
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())?;
    match ext.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "svg" => Some("image/svg+xml".to_string()),
        "webp" => Some("image/webp".to_string()),
        "bmp" => Some("image/bmp".to_string()),
        "ico" => Some("image/vnd.microsoft.icon".to_string()),
        _ => None,
    }
}

pub(crate) fn assets_extension_from_media_type(mt: &Option<String>) -> &'static str {
    match mt.as_deref() {
        Some("image/png") => ".png",
        Some("image/jpeg") => ".jpg",
        Some("image/gif") => ".gif",
        Some("image/svg+xml") => ".svg",
        Some("image/webp") => ".webp",
        Some("image/bmp") => ".bmp",
        Some("image/vnd.microsoft.icon") => ".ico",
        _ => ".bin",
    }
}

pub(crate) fn assets_relative_path(data: &[u8], src: &str, media_type: &Option<String>) -> (String, String) {
    let mut h = Sha256::new();
    h.update(data);
    let sha_full = h.finalize();
    let sha = sha_full
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = src.split('?').next().unwrap_or(src);
    let mut suffix = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_lowercase()))
        .unwrap_or_default();
    if suffix.is_empty() || suffix.len() > 10 {
        suffix = assets_extension_from_media_type(media_type).to_string();
    }
    (format!("assets/{}/{}{}", &sha[..2], sha, suffix), sha)
}

pub(crate) fn assets_resolve_path(source_path: Option<&Path>, src: &str) -> Option<PathBuf> {
    let sp = source_path?;
    if src.is_empty() {
        return None;
    }
    // Skip URLs with scheme or absolute paths.
    if src.starts_with('/') || src.contains("://") {
        return None;
    }
    sp.parent().map(|p| p.join(src))
}

pub(crate) fn assets_text_norm(s: Option<&str>) -> Option<String> {
    let raw = s.unwrap_or("");
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// Walk HTML for <img> tags, extract assets from referenced files, and
/// produce (rewritten_html, assets) where every <img> becomes a <span>
/// carrying the asset_ref + alt-text marker.
pub(crate) fn rewrite_images_html(
    html: &str,
    doc_id: Option<&str>,
    source_path: Option<&Path>,
) -> (String, Vec<ExtractedAsset>) {
    let img_re = Regex::new(r#"(?is)<img\b([^>]*)>"#).unwrap();
    let mut assets: Vec<ExtractedAsset> = Vec::new();
    let mut image_ord: u32 = 0;
    let rewritten = img_re
        .replace_all(html, |caps: &regex::Captures| {
            let attrs_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let alt = assets_text_norm(extract_attr(attrs_str, "alt"));
            let title = assets_text_norm(extract_attr(attrs_str, "title"));
            let label = alt.clone().or_else(|| title.clone()).unwrap_or_default();
            if label.to_lowercase() == "exclamation" {
                return String::new();
            }
            let src = assets_text_norm(extract_attr(attrs_str, "src")).unwrap_or_default();
            let mut data: Option<Vec<u8>> = None;
            if let Some(p) = assets_resolve_path(source_path, &src) {
                if p.exists() {
                    if let Ok(bytes) = fs::read(&p) {
                        data = Some(bytes);
                    }
                }
            }
            let media_type = assets_guess_media_type(&src);
            let mut asset_ref: Option<String> = None;
            if let (Some(d), Some(did)) = (data.as_ref(), doc_id) {
                let r = assets_asset_ref(did, image_ord);
                let (relpath, sha) = assets_relative_path(d, &src, &media_type);
                assets.push(ExtractedAsset {
                    asset_ref: r.clone(),
                    source_path: src.clone(),
                    relative_path: relpath,
                    media_type: media_type.clone(),
                    alt: alt.clone(),
                    title: title.clone(),
                    sha256: sha,
                    size: d.len() as u64,
                    data_b64: base64::engine::general_purpose::STANDARD.encode(d),
                });
                asset_ref = Some(r);
                image_ord += 1;
            }
            if asset_ref.is_none() && label.is_empty() {
                return String::new();
            }
            let mut attrs: Vec<String> = Vec::new();
            if let Some(r) = &asset_ref {
                attrs.push(format!(r#"data-asset-ref="{}""#, assets_html_escape(r)));
            }
            if let Some(mt) = &media_type {
                if asset_ref.is_some() {
                    attrs.push(format!(r#"data-media-type="{}""#, assets_html_escape(mt)));
                }
            }
            let text = if !label.is_empty() {
                format!("[image: {label}]")
            } else {
                "[image]".to_string()
            };
            let attrs_joined = attrs.join(" ");
            let space = if attrs_joined.is_empty() { "" } else { " " };
            format!(
                "<span{space}{attrs}>{text}</span>",
                attrs = attrs_joined,
                text = assets_html_escape(&text)
            )
        })
        .into_owned();
    (rewritten, assets)
}


pub(crate) fn metadata_extract_docid_path(canonical_id: &str) -> Option<String> {
    let parsed = url::Url::parse(canonical_id)
        .ok()
        .or_else(|| url::Url::parse(&format!("https://placeholder/{canonical_id}")).ok())?;
    for (k, v) in parsed.query_pairs() {
        if k.eq_ignore_ascii_case("docid") {
            let s = v.into_owned();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

// [IB-18] doc_id preserves the ATO docid query path verbatim; malformed or
// missing URLs fall back to the canonical_id so every source row has a key.
pub(crate) fn metadata_doc_id_for(canonical_id: &str) -> String {
    metadata_extract_docid_path(canonical_id).unwrap_or_else(|| canonical_id.to_string())
}

pub(crate) fn metadata_parse_docid(canonical_id: &str) -> Option<String> {
    let docid = metadata_extract_docid_path(canonical_id)?;
    docid
        .split('/')
        .find(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
}

pub(crate) fn metadata_extract_pub_date(text: &str) -> Option<String> {
    let date_re = Regex::new(
        r"(?i)\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b",
    )
    .unwrap();
    let head = text.chars().take(2000).collect::<String>();
    let m = date_re.captures(&head)?;
    let day: u32 = m.get(1)?.as_str().parse().ok()?;
    let month_name = m.get(2)?.as_str().to_lowercase();
    let year: u32 = m.get(3)?.as_str().parse().ok()?;
    let month = currency_months().get(month_name.as_str()).copied()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

pub(crate) fn metadata_content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

