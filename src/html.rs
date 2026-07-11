//! Source HTML cleaning, rendering to plaintext with markdown-style markers,
//! `<a href>` rewriting to `data-doc-id`, attribute stripping, named-anchor
//! normalisation, and ATO doc-link parsing.

use legal_model::{DocumentId, SourceId};
use regex::Regex;
use std::collections::HashSet;

// Containers ATO has used over the years. First selector match wins;
// pick_container_html falls back to <main>/<body> if none match.
pub(crate) const ATO_CONTAINER_SELECTORS: &[&str] =
    &["#LawContent", "#lawContents", "#LawContents", "#contents"];
// Strip these source-page elements wholesale before text extraction.
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
// ATO history-toggle labels matched case-insensitively in text nodes and image
// title/alt attributes.
pub(crate) const ATO_HISTORY_UI_LABELS: &[&str] = &[
    "view history note",
    "hide history note",
    "view history reference",
    "hide history reference",
];

// ATO URL parsing accepts ato.gov.au hosts and URLs whose paths contain one of the
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

    // Page title (for hint / display).
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

/// Walk the cleaned tree and emit text with inline markdown markers.
/// Block-level tags introduce paragraph breaks. Inline tags emit:
///   <a> with an ATO docid in href: "text [doc:X]" (with @PiT / view= when
///     present).
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
                    doc_id_from_ato_link(href).and_then(|(native_id, pit, view)| {
                        ato_document_ref(&native_id).map(|document| (document, pit, view))
                    })
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

pub(crate) fn is_ato_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("ato.gov.au")
        || host
            .to_ascii_lowercase()
            .strip_suffix(".ato.gov.au")
            .is_some_and(|prefix| !prefix.is_empty())
}

fn has_valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

fn collect_ato_query(
    query: &str,
    raw: &mut Option<String>,
    pit: &mut Option<String>,
    view: &mut Option<String>,
) -> Option<()> {
    if query.is_empty() || !has_valid_percent_encoding(query) {
        return None;
    }
    let mut saw_raw = false;
    let mut saw_pit = false;
    let mut saw_view = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.to_ascii_lowercase().as_str() {
            "docid" | "locid" => {
                if saw_raw || raw.is_some() {
                    return None;
                }
                saw_raw = true;
                *raw = Some(value.into_owned());
            }
            "pit" => {
                if saw_pit || pit.is_some() {
                    return None;
                }
                saw_pit = true;
                let value = value.trim();
                if !value.is_empty() {
                    *pit = Some(value.to_string());
                }
            }
            "db" => {
                if saw_view || view.is_some() {
                    return None;
                }
                saw_view = true;
                let value = value.trim().to_ascii_uppercase();
                if ATO_KNOWN_VIEWS.iter().any(|known| *known == value) {
                    *view = Some(value);
                }
            }
            _ => {}
        }
    }
    Some(())
}

pub(crate) fn doc_id_from_ato_link(
    target: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut target = target.trim();
    if target.starts_with('<') && target.ends_with('>') && target.len() >= 2 {
        target = &target[1..target.len() - 1];
    }
    if let Some(index) = target.find(' ') {
        target = &target[..index];
    }
    if !has_valid_percent_encoding(target) {
        return None;
    }
    let parsed = if target.starts_with('/') {
        url::Url::parse("https://www.ato.gov.au")
            .ok()?
            .join(target)
            .ok()?
    } else {
        url::Url::parse(target).ok()?
    };
    if parsed.scheme() != "https" || !is_ato_hostname(parsed.host_str()?) {
        return None;
    }
    let path = parsed.path().to_ascii_lowercase();
    if !ATO_DOC_PATH_HINTS.iter().any(|hint| path.contains(hint)) {
        return None;
    }

    let (mut raw, mut pit, mut view) = (None, None, None);
    if let Some(query) = parsed.query() {
        collect_ato_query(query, &mut raw, &mut pit, &mut view)?;
    }
    if raw.is_none() {
        if let Some(fragment) = parsed.fragment() {
            if let Some((_, query)) = fragment.split_once('?') {
                collect_ato_query(query, &mut raw, &mut pit, &mut view)?;
            }
        }
    }
    let doc_id = raw?.trim().trim_matches('"').to_string();
    if doc_id.is_empty()
        || !doc_id.contains('/')
        || doc_id.contains('\\')
        || doc_id
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || doc_id.chars().any(char::is_control)
    {
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
            let Some(document_ref) = ato_document_ref(&doc_id) else {
                return caps.get(0).unwrap().as_str().to_string();
            };
            let href_re = Regex::new(r#"(?is)\s+href\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]*)"#).unwrap();
            let stripped = href_re.replace_all(attrs, "").into_owned();
            let mut new_attrs = stripped;
            new_attrs.push_str(&format!(
                r#" data-doc-id="{}""#,
                assets_html_escape(&document_ref)
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

fn ato_document_ref(native_id: &str) -> Option<String> {
    let source = SourceId::new("ato").ok()?;
    DocumentId::new(source, canonical_ato_native_id(native_id))
        .ok()
        .map(|document| document.public_ref())
}

pub(crate) fn canonical_ato_native_id(native_id: &str) -> String {
    let mut parts = native_id.split('/').map(str::to_owned).collect::<Vec<_>>();
    if let Some(family) = parts.first_mut() {
        family.make_ascii_uppercase();
    }
    if let Some(series) = parts.get_mut(1) {
        series.make_ascii_uppercase();
    }
    if parts
        .first()
        .is_some_and(|family| matches!(family.as_str(), "CLR" | "OPS"))
    {
        for part in parts.iter_mut().skip(2) {
            part.make_ascii_uppercase();
        }
    }
    parts.join("/")
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

#[cfg(test)]
mod security_tests {
    use super::*;

    #[test]
    fn ato_identity_normalizes_source_series_without_changing_section_case() {
        assert_eq!(
            canonical_ato_native_id("clr/cr20269/nat/ato/00001"),
            "CLR/CR20269/NAT/ATO/00001"
        );
        assert_eq!(
            canonical_ato_native_id("pac/19970038/83A-45(5)(a)"),
            "PAC/19970038/83A-45(5)(a)"
        );
    }

    #[test]
    fn ato_hostname_matching_has_label_boundaries() {
        for host in ["ato.gov.au", "www.ato.gov.au", "law.ato.gov.au."] {
            assert!(is_ato_hostname(host), "rejected {host}");
        }
        for host in ["evilato.gov.au", "ato.gov.au.evil.test", "gov.au"] {
            assert!(!is_ato_hostname(host), "accepted {host}");
        }
    }

    #[test]
    fn ato_links_require_https_exact_host_path_and_unique_fields() {
        let good = "https://www.ato.gov.au/law/view/document?docid=JUD/X/Y&PiT=20250101";
        assert_eq!(
            doc_id_from_ato_link(good),
            Some(("JUD/X/Y".to_string(), Some("20250101".to_string()), None))
        );
        for bad in [
            "https://evilato.gov.au/law/view/document?docid=JUD/X/Y",
            "http://www.ato.gov.au/law/view/document?docid=JUD/X/Y",
            "https://www.ato.gov.au/not-law?docid=JUD/X/Y",
            "https://www.ato.gov.au/law/view/document?docid=JUD/X/Y&locid=JUD/A/B",
            "https://www.ato.gov.au/law/view/document?docid=JUD/X/Y&pit=1&pit=2",
            "https://www.ato.gov.au/law/view/document?docid=JUD/%2e%2e/Y",
            "https://www.ato.gov.au/law/view/document?docid=JUD/X/Y%GG",
        ] {
            assert!(doc_id_from_ato_link(bad).is_none(), "accepted {bad}");
        }
    }
}
