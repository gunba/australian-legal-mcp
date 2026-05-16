//! Source HTML cleaning, rendering to plaintext with markdown-style markers,
//! `<a href>` rewriting to `data-doc-id`, attribute stripping, named-anchor
//! normalisation, and ATO doc-link parsing.

use regex::Regex;
use std::collections::HashSet;

// [IB-06] Containers ATO has used over the years. First selector match wins;
// pick_container_html falls back to <main>/<body> if none match.
pub(crate) const ATO_CONTAINER_SELECTORS: &[&str] =
    &["#LawContent", "#lawContents", "#LawContents", "#contents"];
// Strip these wholesale before any text extraction. Mirrors extract.py:_strip_noise.
pub(crate) const ATO_NOISE_SELECTORS: &[&str] = &[
    "script",
    "style",
    "noscript",
    "template",
    "nav",
    "#LawMiniMenuHeader",
    ".minimenu",
    ".minimenu-bar",
];
// History-toggle UI labels — case-insensitive match on text-node content and
// img title/alt attributes. Mirrors extract.py:_HISTORY_UI_LABELS.
pub(crate) const ATO_HISTORY_UI_LABELS: &[&str] = &[
    "view history note",
    "hide history note",
    "view history reference",
    "hide history reference",
];

// ATO URL parsing — port of extract.py:_doc_id_from_ato_link and helpers.
// We accept either ato.gov.au hosts or any URL whose path contains one of the
// ATO doc path hints. Recognised query params (case-insensitive): docid, locid,
// PiT, db. Recognised db values: HISTFT (amendment-history view).
pub(crate) const ATO_DOC_PATH_HINTS: &[&str] = &[
    "/law/view/document",
    "/law/view/view.htm",
    "/law/view.htm",
    "/atolaw/view.htm",
    "/view.htm",
];
pub(crate) const ATO_KNOWN_VIEWS: &[&str] = &["HISTFT"];

pub(crate) struct CleanedAtoDoc {
    pub(crate) html: String,
    pub(crate) text: String,
    pub(crate) title: Option<String>,
}

pub(crate) fn clean_ato_html(html: &str) -> CleanedAtoDoc {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);

    // Browser tab title (for hint / display).
    let title_selector = Selector::parse("title").unwrap();
    let raw_title = doc
        .select(&title_selector)
        .next()
        .map(|n| n.text().collect::<String>());
    let title = raw_title
        .map(|t| crate::rules::collapse_ws(&t))
        .filter(|t| !t.is_empty());

    // Pick container — first match wins; fallback to <main> then <body>.
    let container_html = pick_container_html(&doc);
    let Some(container_html) = container_html else {
        return CleanedAtoDoc {
            html: String::new(),
            text: String::new(),
            title,
        };
    };

    // Re-parse the picked container so we can strip noise within just that subtree.
    let mut subdoc = Html::parse_fragment(&container_html);
    strip_noise(&mut subdoc);
    strip_history_ui_controls(&mut subdoc);
    let referenced_anchors = collect_referenced_anchors(&subdoc);

    let cleaned_html = subdoc.root_element().html();
    let cleaned_text = subtree_text(&subdoc, &referenced_anchors);
    CleanedAtoDoc {
        html: cleaned_html,
        text: cleaned_text,
        title,
    }
}

pub(crate) fn pick_container_html(doc: &scraper::Html) -> Option<String> {
    use scraper::Selector;
    for sel_str in ATO_CONTAINER_SELECTORS {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(node) = doc.select(&sel).next() {
                return Some(node.html());
            }
        }
    }
    for sel_str in &["main", "body"] {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(node) = doc.select(&sel).next() {
                return Some(node.html());
            }
        }
    }
    None
}

pub(crate) fn strip_noise(doc: &mut scraper::Html) {
    use ego_tree::NodeId;
    use scraper::Selector;
    let mut to_remove: Vec<NodeId> = Vec::new();
    for sel_str in ATO_NOISE_SELECTORS {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in doc.select(&sel) {
                to_remove.push(el.id());
            }
        }
    }
    for id in to_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

pub(crate) fn strip_history_ui_controls(doc: &mut scraper::Html) {
    use ego_tree::NodeId;
    use scraper::{Node as ScraperNode, Selector};

    // Pass 1: strip <img> whose title or alt matches a history-UI label.
    let img_sel = Selector::parse("img").unwrap();
    let mut img_remove: Vec<NodeId> = Vec::new();
    for el in doc.select(&img_sel) {
        let val = el.value();
        let title = val.attr("title").unwrap_or("").trim().to_lowercase();
        let alt = val.attr("alt").unwrap_or("").trim().to_lowercase();
        if ATO_HISTORY_UI_LABELS
            .iter()
            .any(|l| *l == title || *l == alt)
        {
            img_remove.push(el.id());
        }
    }
    for id in img_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }

    // Pass 2: strip text nodes whose content is exactly a history-UI label.
    let mut text_remove: Vec<NodeId> = Vec::new();
    for node_ref in doc.tree.nodes() {
        if let ScraperNode::Text(text) = node_ref.value() {
            let trimmed = text.trim().to_lowercase();
            if ATO_HISTORY_UI_LABELS.iter().any(|l| *l == trimmed) {
                text_remove.push(node_ref.id());
            }
        }
    }
    for id in text_remove {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

pub(crate) fn collect_referenced_anchors(doc: &scraper::Html) -> HashSet<String> {
    use scraper::Selector;
    let sel = Selector::parse("a[href]").unwrap();
    let mut refs = HashSet::new();
    for el in doc.select(&sel) {
        let href = el.value().attr("href").unwrap_or("");
        if let Some(name) = href.strip_prefix('#') {
            if !name.is_empty() {
                refs.insert(name.to_string());
            }
        }
    }
    refs
}

pub(crate) fn has_descendant_with_tag(
    node: ego_tree::NodeRef<scraper::Node>,
    tags: &[&str],
) -> bool {
    use scraper::Node as ScraperNode;
    for n in node.descendants() {
        if let ScraperNode::Element(el) = n.value() {
            if tags.contains(&el.name()) {
                return true;
            }
        }
    }
    false
}

/// Walk the cleaned tree and emit text with inline markdown markers, ported
/// from src/ato_mcp/indexer/chunk.py:_inline_text + html_to_text. Block-level
/// tags introduce paragraph breaks. Inline tags emit:
///   <a> with an ATO docid in href: "text [doc:X]" (with @PiT / view= when
///     present) — ported from chunk.py:_inline_text and
///     extract.py:_doc_id_from_ato_link.
///   <a name="X"> where X is referenced: "text [anchor:X]"
///   any element with id="X" referenced (fallback): "text [anchor:X]"
///   <span data-asset-ref="X">: "[asset:X]"
///   <img alt="...">: "[image: alt]" when alt is non-empty, else dropped
///   <strong>/<b> containing <em>/<i> (or vice versa): "***term***"
///   <strong>/<b>: **text**, <em>/<i>: *text*
///   <h1>-<h6>:    "# text" / "## text" / ... on their own line
///   <br>:         newline
pub(crate) fn subtree_text(doc: &scraper::Html, referenced_anchors: &HashSet<String>) -> String {
    let mut buf = String::new();
    for root_child in doc.tree.root().children() {
        render_node(root_child, &mut buf, referenced_anchors);
    }
    normalise_paragraph_breaks(&buf)
}

pub(crate) fn render_node(
    node: ego_tree::NodeRef<scraper::Node>,
    buf: &mut String,
    referenced: &HashSet<String>,
) {
    use scraper::Node as ScraperNode;

    const BLOCK_TAGS: &[&str] = &[
        "p",
        "div",
        "section",
        "article",
        "header",
        "footer",
        "main",
        "aside",
        "table",
        "tr",
        "thead",
        "tbody",
        "tfoot",
        "td",
        "th",
        "caption",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "hr",
        "pre",
        "blockquote",
    ];

    match node.value() {
        ScraperNode::Text(t) => {
            let raw: &str = &t.text;
            let mut last_ws = buf.chars().last().is_none_or(|c| c == '\n');
            for c in raw.chars() {
                if c.is_whitespace() {
                    if !last_ws {
                        buf.push(' ');
                        last_ws = true;
                    }
                } else {
                    buf.push(c);
                    last_ws = false;
                }
            }
        }
        ScraperNode::Element(el) => {
            let tag = el.name();

            match tag {
                "br" => {
                    buf.push('\n');
                    return;
                }
                "img" => {
                    let alt = el.attr("alt").unwrap_or("").trim();
                    if !alt.is_empty() {
                        buf.push_str("[image: ");
                        buf.push_str(alt);
                        buf.push(']');
                    }
                    return;
                }
                "span" => {
                    if let Some(asset_ref) = el.attr("data-asset-ref") {
                        buf.push_str("[asset:");
                        buf.push_str(asset_ref);
                        buf.push(']');
                        return;
                    }
                }
                _ => {}
            }

            if tag == "a" {
                let href = el.attr("href").unwrap_or("");
                let data_doc_id = el.attr("data-doc-id");
                let resolved = if let Some(id) = data_doc_id {
                    Some((
                        id.to_string(),
                        el.attr("data-pit").map(|s| s.to_string()),
                        el.attr("data-view").map(|s| s.to_string()),
                    ))
                } else if !href.is_empty() {
                    doc_id_from_ato_link(href)
                } else {
                    None
                };
                let inner = render_inner_string(node, referenced).trim().to_string();
                if let Some((doc_id, pit, view)) = resolved {
                    let mut marker = format!("[doc:{doc_id}");
                    if let Some(p) = pit.as_ref().filter(|s| !s.is_empty()) {
                        marker.push('@');
                        marker.push_str(p);
                    }
                    if let Some(v) = view.as_ref().filter(|s| !s.is_empty()) {
                        marker.push_str(" view=");
                        marker.push_str(v);
                    }
                    marker.push(']');
                    if !inner.is_empty() {
                        buf.push_str(&inner);
                        buf.push(' ');
                    }
                    buf.push_str(&marker);
                    return;
                }
                if let Some(name) = el.attr("name") {
                    if referenced.contains(name) {
                        if !inner.is_empty() {
                            buf.push_str(&inner);
                            buf.push(' ');
                        }
                        buf.push_str("[anchor:");
                        buf.push_str(name);
                        buf.push(']');
                        return;
                    }
                }
                if !inner.is_empty() {
                    buf.push_str(&inner);
                }
                return;
            }

            let is_def_term = match tag {
                "strong" | "b" => has_descendant_with_tag(node, &["em", "i"]),
                "em" | "i" => has_descendant_with_tag(node, &["strong", "b"]),
                _ => false,
            };
            if is_def_term {
                let term = render_inner_string(node, referenced).trim().to_string();
                if !term.is_empty() {
                    buf.push_str("***");
                    buf.push_str(&term);
                    buf.push_str("***");
                }
                if let Some(id) = el.attr("id") {
                    if referenced.contains(id) {
                        buf.push_str(" [anchor:");
                        buf.push_str(id);
                        buf.push(']');
                    }
                }
                return;
            }

            match tag {
                "strong" | "b" => {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push_str("**");
                        buf.push_str(&inner);
                        buf.push_str("**");
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                        }
                    }
                    return;
                }
                "em" | "i" => {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push('*');
                        buf.push_str(&inner);
                        buf.push('*');
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                        }
                    }
                    return;
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if !buf.ends_with('\n') && !buf.is_empty() {
                        buf.push('\n');
                    }
                    let level = tag[1..].parse::<usize>().unwrap_or(1).clamp(1, 6);
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        for _ in 0..level {
                            buf.push('#');
                        }
                        buf.push(' ');
                        buf.push_str(&inner);
                        if let Some(id) = el.attr("id") {
                            if referenced.contains(id) {
                                buf.push_str(" [anchor:");
                                buf.push_str(id);
                                buf.push(']');
                            }
                        }
                        buf.push('\n');
                    }
                    return;
                }
                _ if BLOCK_TAGS.contains(&tag) => {
                    if !buf.ends_with('\n') && !buf.is_empty() {
                        buf.push('\n');
                    }
                    for child in node.children() {
                        render_node(child, buf, referenced);
                    }
                    if let Some(id) = el.attr("id") {
                        if referenced.contains(id) {
                            if buf.ends_with('\n') {
                                buf.pop();
                            }
                            buf.push_str(" [anchor:");
                            buf.push_str(id);
                            buf.push(']');
                            buf.push('\n');
                        } else if !buf.ends_with('\n') {
                            buf.push('\n');
                        }
                    } else if !buf.ends_with('\n') {
                        buf.push('\n');
                    }
                    return;
                }
                _ => {}
            }

            if let Some(id) = el.attr("id") {
                if referenced.contains(id) {
                    let inner = render_inner_string(node, referenced).trim().to_string();
                    if !inner.is_empty() {
                        buf.push_str(&inner);
                        buf.push(' ');
                    }
                    buf.push_str("[anchor:");
                    buf.push_str(id);
                    buf.push(']');
                    return;
                }
            }

            for child in node.children() {
                render_node(child, buf, referenced);
            }
        }
        _ => {
            for child in node.children() {
                render_node(child, buf, referenced);
            }
        }
    }
}

pub(crate) fn render_inner_string(
    node: ego_tree::NodeRef<scraper::Node>,
    referenced: &HashSet<String>,
) -> String {
    let mut inner = String::new();
    for child in node.children() {
        render_node(child, &mut inner, referenced);
    }
    inner
}

pub(crate) fn doc_id_from_ato_link(
    target: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut t = target.trim();
    if t.starts_with('<') && t.ends_with('>') && t.len() >= 2 {
        t = &t[1..t.len() - 1];
    }
    if let Some(idx) = t.find(' ') {
        t = &t[..idx];
    }
    let parsed = if t.starts_with('/') {
        let base = url::Url::parse("https://www.ato.gov.au").ok()?;
        base.join(t).ok()?
    } else {
        url::Url::parse(t).ok()?
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let path_lower = parsed.path().to_ascii_lowercase();
    let is_ato_host = host.ends_with("ato.gov.au");
    let has_ato_path = ATO_DOC_PATH_HINTS
        .iter()
        .any(|hint| path_lower.contains(hint));
    if !(is_ato_host || has_ato_path) {
        return None;
    }
    let (mut raw, mut pit, mut view) = (None, None, None);
    for (k, v) in parsed.query_pairs() {
        let key_lc = k.to_ascii_lowercase();
        match key_lc.as_str() {
            "docid" | "locid" if raw.is_none() => {
                raw = Some(v.into_owned());
            }
            "pit" if pit.is_none() => {
                let s = v.trim().to_string();
                if !s.is_empty() {
                    pit = Some(s);
                }
            }
            "db" if view.is_none() => {
                let s = v.trim().to_ascii_uppercase();
                if ATO_KNOWN_VIEWS.iter().any(|kv| *kv == s) {
                    view = Some(s);
                }
            }
            _ => {}
        }
    }
    if raw.is_none() {
        if let Some(frag) = parsed.fragment() {
            if let Some(qpos) = frag.find('?') {
                let frag_query = &frag[qpos + 1..];
                for (k, v) in url::form_urlencoded::parse(frag_query.as_bytes()) {
                    let key_lc = k.to_ascii_lowercase();
                    match key_lc.as_str() {
                        "docid" | "locid" if raw.is_none() => {
                            raw = Some(v.into_owned());
                        }
                        "pit" if pit.is_none() => {
                            let s = v.trim().to_string();
                            if !s.is_empty() {
                                pit = Some(s);
                            }
                        }
                        "db" if view.is_none() => {
                            let s = v.trim().to_ascii_uppercase();
                            if ATO_KNOWN_VIEWS.iter().any(|kv| *kv == s) {
                                view = Some(s);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let raw = raw?;
    if raw.ends_with('?') {
        return None;
    }
    let doc_id = raw.trim().trim_matches('"').to_string();
    if doc_id.is_empty() || !doc_id.contains('/') {
        return None;
    }
    Some((doc_id, pit, view))
}

pub(crate) fn normalise_paragraph_breaks(s: &str) -> String {
    let mut out_lines: Vec<String> = Vec::new();
    let mut last_blank = false;
    for line in s.split('\n') {
        let collapsed = crate::rules::collapse_ws(line);
        if collapsed.is_empty() {
            if !last_blank && !out_lines.is_empty() {
                out_lines.push(String::new());
            }
            last_blank = true;
        } else {
            out_lines.push(collapsed);
            last_blank = false;
        }
    }
    while out_lines.last().is_some_and(|l| l.is_empty()) {
        out_lines.pop();
    }
    out_lines.join("\n")
}

pub(crate) fn extract_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    fn common_re(name: &str) -> Option<&'static Regex> {
        macro_rules! attr_re {
            ($cell:ident, $name:literal) => {{
                static $cell: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
                $cell.get_or_init(|| {
                    Regex::new(concat!(
                        r#"(?is)\b"#,
                        $name,
                        r#"\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*))"#
                    ))
                    .unwrap()
                })
            }};
        }
        match name.to_ascii_lowercase().as_str() {
            "alt" => Some(attr_re!(ATTR_ALT_RE, "alt")),
            "href" => Some(attr_re!(ATTR_HREF_RE, "href")),
            "id" => Some(attr_re!(ATTR_ID_RE, "id")),
            "name" => Some(attr_re!(ATTR_NAME_RE, "name")),
            "src" => Some(attr_re!(ATTR_SRC_RE, "src")),
            "title" => Some(attr_re!(ATTR_TITLE_RE, "title")),
            _ => None,
        }
    }
    fn capture_attr<'a>(re: &Regex, attrs: &'a str) -> Option<&'a str> {
        let caps = re.captures(attrs)?;
        caps.get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
    }
    if let Some(re) = common_re(name) {
        return capture_attr(re, attrs);
    }
    let pat = format!(
        r#"(?is)\b{}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]*))"#,
        regex::escape(name)
    );
    let re = Regex::new(&pat).ok()?;
    capture_attr(&re, attrs)
}

pub(crate) fn strip_attributes(html: &str) -> String {
    static STRIP_ATTR_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = STRIP_ATTR_RE.get_or_init(|| {
        Regex::new(
            r#"(?is)\s+(?:style|width|height|align|valign|bgcolor|name|data-icon|cite|on[a-zA-Z]+)\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#,
        )
        .unwrap()
    });
    re.replace_all(html, "").into_owned()
}

pub(crate) fn normalise_named_anchors(html: &str) -> String {
    static A_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NAME_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let a_re = A_RE.get_or_init(|| Regex::new(r#"(?is)<a\b([^>]*)>"#).unwrap());
    let name_re = NAME_RE
        .get_or_init(|| Regex::new(r#"(?is)\s+name\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap());
    a_re.replace_all(html, |caps: &regex::Captures| {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let name = extract_attr(attrs, "name");
        let id = extract_attr(attrs, "id");
        let mut new_attrs = attrs.to_string();
        new_attrs = name_re.replace_all(&new_attrs, "").into_owned();
        if let Some(n) = name {
            if id.is_none() {
                new_attrs.push_str(&format!(r#" id="{}""#, assets_html_escape(n)));
            }
        }
        format!("<a{new_attrs}>")
    })
    .into_owned()
}

pub(crate) fn rewrite_links_html(html: &str) -> String {
    let a_re = Regex::new(r#"(?is)<a\b([^>]*)>"#).unwrap();
    a_re.replace_all(html, |caps: &regex::Captures| {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let Some(href) = extract_attr(attrs, "href") else {
            return caps.get(0).unwrap().as_str().to_string();
        };
        if let Some((doc_id, pit, view)) = doc_id_from_ato_link(href) {
            let href_re = Regex::new(r#"(?is)\s+href\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap();
            let stripped = href_re.replace_all(attrs, "").into_owned();
            let mut new_attrs = stripped;
            new_attrs.push_str(&format!(
                r#" data-doc-id="{}""#,
                assets_html_escape(&doc_id)
            ));
            if let Some(p) = pit {
                new_attrs.push_str(&format!(r#" data-pit="{}""#, assets_html_escape(&p)));
            }
            if let Some(v) = view {
                new_attrs.push_str(&format!(r#" data-view="{}""#, assets_html_escape(&v)));
            }
            return format!("<a{new_attrs}>");
        }
        let safe = href.trim();
        if safe.is_empty()
            || Regex::new(r#"(?is)^\s*(?:javascript|data):"#)
                .unwrap()
                .is_match(safe)
        {
            let href_re = Regex::new(r#"(?is)\s+href\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap();
            let stripped = href_re.replace_all(attrs, "").into_owned();
            return format!("<a{stripped}>");
        }
        caps.get(0).unwrap().as_str().to_string()
    })
    .into_owned()
}

pub(crate) fn assets_html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}
