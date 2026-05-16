//! Block-aware chunker. Walks cleaned ATO HTML into atomic blocks, renders
//! each to plaintext with markdown markers, then greedy-packs blocks into
//! chunks bounded by `max_tokens`. Mirrors src/ato_mcp/indexer/chunk.py.

use crate::html::{collect_referenced_anchors, render_node};
use regex::Regex;
use serde::Serialize;

// ----- Chunker (port of src/ato_mcp/indexer/chunk.py) -----
//
// Block-aware chunking for cleaned ATO HTML. Walks the DOM into a flat list
// of atomic blocks, renders each into plaintext with markdown markers, then
// greedy-packs blocks into chunks bounded by max_tokens. Mirrors chunk.py's
// public API (chunk_html, html_to_text, approx_tokens) and intermediate
// shape (_Block, Chunk).

// [IB-21] Checkpoints pin CHUNKER_FORMAT_VERSION; changing output shape
// forces an explicit fresh build instead of resuming stale chunk records.
pub(crate) const CHUNKER_FORMAT_VERSION: u32 = 3;
pub(crate) const EMBED_MAX_TOKENS: usize = 1024;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Chunk {
    pub(crate) ord: i64,
    pub(crate) anchor: Option<String>,
    pub(crate) text: String,
    pub(crate) definition_text: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkBlock {
    pub(crate) text: String,
    pub(crate) definition_text: String,
    pub(crate) anchor: Option<String>,
    pub(crate) is_oversize_table: bool,
    /// Set when the block is an oversize table — needed by chunker_split
    /// to walk rows in table-row-split mode.
    pub(crate) table_html: Option<String>,
}

pub(crate) fn chunker_approx_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    std::cmp::max(1, ((words as f64) * 1.3) as usize)
}

/// Tighter whitespace normalisation than `normalise_paragraph_breaks`:
/// matches chunk.py:_normalise_text. Collapses NBSP and horizontal-only
/// runs to single spaces, collapses ` *\n *` to `\n`, caps newline runs at
/// two, normalises numeric-range spacing, and tightens quoted text.
pub(crate) fn chunker_normalise_text(text: &str) -> String {
    static WS_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_PAD_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_RUN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NUMERIC_RANGE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SPACED_QUOTE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let s = text.replace('\u{a0}', " ");
    // _WS_RE: horizontal whitespace [ \t\f\v]+
    let ws = WS_RE.get_or_init(|| Regex::new(r"[ \t\x0c\x0b]+").unwrap());
    let s = ws.replace_all(&s, " ").into_owned();
    let newline_pad = NEWLINE_PAD_RE.get_or_init(|| Regex::new(r" *\n *").unwrap());
    let s = newline_pad.replace_all(&s, "\n").into_owned();
    let newline_run = NEWLINE_RUN_RE.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let s = newline_run.replace_all(&s, "\n\n").into_owned();
    let s = s.trim().to_string();
    let numeric_range =
        NUMERIC_RANGE_RE.get_or_init(|| Regex::new(r"(?P<a>\d)\s+-\s+(?P<b>\d)").unwrap());
    let s = numeric_range.replace_all(&s, "$a-$b").into_owned();
    let spaced_quote = SPACED_QUOTE_RE.get_or_init(|| Regex::new(r#""\s+([^"\n]*?)\s+""#).unwrap());
    spaced_quote.replace_all(&s, r#""$1""#).into_owned()
}

pub(crate) fn chunker_heading_anchor(node: scraper::ElementRef) -> Option<String> {
    if let Some(id) = node.value().attr("id") {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    let a_sel = scraper::Selector::parse("a").unwrap();
    for a in node.select(&a_sel) {
        let val = a.value();
        if let Some(name) = val.attr("id").or_else(|| val.attr("name")) {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub(crate) fn chunker_first_referenced_anchor(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> Option<String> {
    for el in node.descendants() {
        if let Some(eref) = scraper::ElementRef::wrap(el) {
            let val = eref.value();
            if let Some(name) = val.attr("name") {
                if referenced.contains(name) {
                    return Some(name.to_string());
                }
            }
            if let Some(nid) = val.attr("id") {
                if referenced.contains(nid) {
                    return Some(nid.to_string());
                }
            }
        }
    }
    None
}

pub(crate) fn chunker_is_root_title_echo(heading: &str, root_title: Option<&str>) -> bool {
    let Some(rt) = root_title else { return false };
    chunker_normalise_text(heading).to_lowercase() == chunker_normalise_text(rt).to_lowercase()
}

/// Render a single subtree to inline text using the existing render_node
/// machinery (which already produces [doc:X], [anchor:X], [asset:X],
/// **/*/# markers). Used by the chunker for block rendering.
pub(crate) fn chunker_render_inline(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> String {
    let mut buf = String::new();
    for child in node.children() {
        render_node(child, &mut buf, referenced);
    }
    buf
}

pub(crate) fn chunker_is_atomic_block(tag: &str, has_structural_child: bool) -> bool {
    const PURE_ATOMIC: &[&str] = &[
        "table",
        "p",
        "pre",
        "blockquote",
        "li",
        "figcaption",
        "caption",
        "dt",
        "dd",
    ];
    const CONTAINER_BLOCKS: &[&str] = &[
        "article", "aside", "details", "div", "dl", "figure", "footer", "header", "main", "ol",
        "section", "ul",
    ];
    const BLOCK_TAGS: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "caption",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "header",
        "li",
        "main",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    if PURE_ATOMIC.contains(&tag) {
        return true;
    }
    if !BLOCK_TAGS.contains(&tag) {
        return false;
    }
    if CONTAINER_BLOCKS.contains(&tag) {
        return !has_structural_child;
    }
    true
}

pub(crate) fn chunker_child_is_structural(tag: &str) -> bool {
    const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
    const BLOCK_TAGS: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "caption",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "header",
        "li",
        "main",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ];
    HEADING_TAGS.contains(&tag) || BLOCK_TAGS.contains(&tag)
}

pub(crate) fn chunker_has_structural_child(node: scraper::ElementRef) -> bool {
    for child in node.children() {
        if let Some(eref) = scraper::ElementRef::wrap(child) {
            if chunker_child_is_structural(eref.value().name()) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn chunker_render_table_text(
    table: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> String {
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("th, td").unwrap();
    let mut rows: Vec<String> = Vec::new();
    for row in table.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|cell| chunker_normalise_text(&chunker_render_inline(cell, referenced)))
            .filter(|c| !c.is_empty())
            .collect();
        if !cells.is_empty() {
            rows.push(cells.join(" | "));
        }
    }
    if !rows.is_empty() {
        rows.join("\n")
    } else {
        chunker_normalise_text(&chunker_render_inline(table, referenced))
    }
}

pub(crate) fn chunker_render_block(
    node: scraper::ElementRef,
    referenced: &std::collections::HashSet<String>,
) -> Option<ChunkBlock> {
    let tag = node.value().name();
    let text = match tag {
        "table" => chunker_render_table_text(node, referenced),
        "blockquote" => {
            let inner = chunker_normalise_text(&chunker_render_inline(node, referenced));
            if inner.is_empty() {
                String::new()
            } else {
                inner
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| format!("> {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "pre" => {
            // Use raw text() rather than rendered (no markers inside <pre>).
            let inner = node.text().collect::<String>();
            let inner = inner.trim();
            if inner.is_empty() {
                String::new()
            } else {
                format!("```\n{inner}\n```")
            }
        }
        "li" => {
            let inner = chunker_normalise_text(&chunker_render_inline(node, referenced));
            if inner.is_empty() {
                String::new()
            } else {
                format!("- {inner}")
            }
        }
        "ul" | "ol" => {
            let li_sel = scraper::Selector::parse("li").unwrap();
            let items: Vec<String> = node
                .select(&li_sel)
                .map(|li| {
                    let t = chunker_normalise_text(&chunker_render_inline(li, referenced));
                    if t.is_empty() {
                        String::new()
                    } else {
                        format!("- {t}")
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            items.join("\n")
        }
        _ => chunker_normalise_text(&chunker_render_inline(node, referenced)),
    };
    if text.is_empty() {
        return None;
    }
    let anchor = chunker_first_referenced_anchor(node, referenced);
    let is_oversize_table = tag == "table" && chunker_approx_tokens(&text) > EMBED_MAX_TOKENS;
    let table_html = if is_oversize_table {
        Some(node.html())
    } else {
        None
    };
    Some(ChunkBlock {
        text: text.clone(),
        definition_text: text,
        anchor,
        is_oversize_table,
        table_html,
    })
}

pub(crate) fn chunker_render_dt_dd_pair(
    dt: scraper::ElementRef,
    dd: Option<scraper::ElementRef>,
    referenced: &std::collections::HashSet<String>,
) -> Option<ChunkBlock> {
    let term = chunker_normalise_text(&chunker_render_inline(dt, referenced));
    let body = match dd {
        Some(d) => chunker_normalise_text(&chunker_render_inline(d, referenced)),
        None => String::new(),
    };
    if term.is_empty() && body.is_empty() {
        return None;
    }
    let mut rendered = if term.is_empty() {
        String::new()
    } else {
        format!("**{term}**")
    };
    if !body.is_empty() {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&body);
    }
    let mut anchor = chunker_first_referenced_anchor(dt, referenced);
    if anchor.is_none() {
        if let Some(d) = dd {
            anchor = chunker_first_referenced_anchor(d, referenced);
        }
    }
    Some(ChunkBlock {
        text: rendered.clone(),
        definition_text: rendered,
        anchor,
        is_oversize_table: false,
        table_html: None,
    })
}

/// Walk children of `parent` and emit ChunkBlocks. Mirrors chunk.py:_walk.
pub(crate) fn chunker_walk(
    parent: scraper::ElementRef,
    blocks: &mut Vec<ChunkBlock>,
    referenced: &std::collections::HashSet<String>,
    root_title: Option<&str>,
) {
    const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
    let mut inline_parts: Vec<String> = Vec::new();
    let mut inline_anchors: Vec<String> = Vec::new();

    let children: Vec<_> = parent.children().collect();
    let mut idx = 0;
    while idx < children.len() {
        let child = children[idx];
        let Some(eref) = scraper::ElementRef::wrap(child) else {
            // Text node — accumulate to inline buffer using render_node.
            let mut tmp = String::new();
            render_node(child, &mut tmp, referenced);
            if !tmp.is_empty() {
                inline_parts.push(tmp);
            }
            idx += 1;
            continue;
        };
        let tag = eref.value().name();

        // dt/dd pair: combine adjacent dt + dd.
        if tag == "dt" {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            let dd = children
                .get(idx + 1)
                .and_then(|n| scraper::ElementRef::wrap(*n))
                .filter(|e| e.value().name() == "dd");
            if let Some(block) = chunker_render_dt_dd_pair(eref, dd, referenced) {
                blocks.push(block);
            }
            idx += if dd.is_some() { 2 } else { 1 };
            continue;
        }
        // Headings render as their own block with markdown level marker.
        if HEADING_TAGS.contains(&tag) {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            let inner = chunker_render_inline(eref, referenced);
            let heading_text = chunker_normalise_text(&inner);
            if !heading_text.is_empty() && !chunker_is_root_title_echo(&heading_text, root_title) {
                let level: usize = tag[1..].parse().unwrap_or(1).clamp(1, 6);
                let rendered = format!("{} {}", "#".repeat(level), heading_text);
                let anchor = chunker_heading_anchor(eref);
                blocks.push(ChunkBlock {
                    text: rendered.clone(),
                    definition_text: rendered,
                    anchor,
                    is_oversize_table: false,
                    table_html: None,
                });
            }
            idx += 1;
            continue;
        }
        let has_struct = chunker_has_structural_child(eref);
        if chunker_is_atomic_block(tag, has_struct) {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            if let Some(block) = chunker_render_block(eref, referenced) {
                blocks.push(block);
            }
            idx += 1;
            continue;
        }
        if has_struct {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            chunker_walk(eref, blocks, referenced, root_title);
            idx += 1;
            continue;
        }
        // Pure inline element — accumulate.
        let rendered = chunker_render_inline(eref, referenced);
        if !rendered.is_empty() {
            inline_parts.push(rendered);
        }
        if let Some(a) = chunker_first_referenced_anchor(eref, referenced) {
            inline_anchors.push(a);
        }
        idx += 1;
    }
    chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
}

pub(crate) fn chunker_flush_inline(
    inline_parts: &mut Vec<String>,
    inline_anchors: &mut Vec<String>,
    blocks: &mut Vec<ChunkBlock>,
) {
    let joined = inline_parts.join("");
    let text = chunker_normalise_text(&joined);
    if !text.is_empty() {
        let anchor = inline_anchors.first().cloned();
        blocks.push(ChunkBlock {
            text: text.clone(),
            definition_text: text,
            anchor,
            is_oversize_table: false,
            table_html: None,
        });
    }
    inline_parts.clear();
    inline_anchors.clear();
}

/// Split an oversize block into pieces that each fit within max_tokens.
/// Mirrors chunk.py:_split_oversize_block. Order:
///   1. oversize tables -> row split (rows stay whole).
///   2. prose -> sentence split, greedy-pack within budget.
///   3. word-window split as last-resort (single sentence/row over budget).
pub(crate) fn chunker_split_oversize_block(block: &ChunkBlock, max_tokens: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if block.is_oversize_table {
        if let Some(html) = block.table_html.as_deref() {
            for (piece, defn) in chunker_table_row_split(html, max_tokens) {
                for p in chunker_enforce_max_tokens(&piece, &defn, max_tokens) {
                    out.push(p);
                }
            }
            return out;
        }
    }
    // Prose: sentence-split, greedy-pack.
    let sentences = chunker_sentence_split(&block.text);
    let mut buf: Vec<String> = Vec::new();
    let mut buf_tokens: usize = 0;
    for s in sentences {
        let st = chunker_approx_tokens(&s);
        if !buf.is_empty() && buf_tokens + st > max_tokens {
            let piece = buf.join(" ");
            for p in chunker_enforce_max_tokens(&piece, &piece, max_tokens) {
                out.push(p);
            }
            buf = vec![s];
            buf_tokens = st;
        } else {
            buf.push(s);
            buf_tokens += st;
        }
    }
    if !buf.is_empty() {
        let piece = buf.join(" ");
        for p in chunker_enforce_max_tokens(&piece, &piece, max_tokens) {
            out.push(p);
        }
    }
    out
}

pub(crate) fn chunker_enforce_max_tokens(
    text: &str,
    definition_text: &str,
    max_tokens: usize,
) -> Vec<(String, String)> {
    if chunker_approx_tokens(text) <= max_tokens {
        return vec![(text.to_string(), definition_text.to_string())];
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let target_words = std::cmp::max(1, ((max_tokens as f64) / 1.4) as usize);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let end = std::cmp::min(i + target_words, words.len());
        let piece = words[i..end].join(" ");
        out.push((piece.clone(), piece));
        i = end;
    }
    out
}

pub(crate) fn chunker_table_row_split(table_html: &str, max_tokens: usize) -> Vec<(String, String)> {
    let frag = scraper::Html::parse_fragment(table_html);
    let row_sel = scraper::Selector::parse("tr").unwrap();
    let cell_sel = scraper::Selector::parse("th, td").unwrap();
    let referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows: Vec<String> = Vec::new();
    for row in frag.select(&row_sel) {
        let cells: Vec<String> = row
            .select(&cell_sel)
            .map(|c| chunker_normalise_text(&chunker_render_inline(c, &referenced)))
            .filter(|s| !s.is_empty())
            .collect();
        if !cells.is_empty() {
            rows.push(cells.join(" | "));
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut buf_tokens: usize = 0;
    for row in rows {
        let row_tokens = chunker_approx_tokens(&row);
        if !buf.is_empty() && buf_tokens + row_tokens > max_tokens {
            let piece = buf.join("\n");
            out.push((piece.clone(), piece));
            buf = vec![row];
            buf_tokens = row_tokens;
        } else {
            buf.push(row);
            buf_tokens += row_tokens;
        }
    }
    if !buf.is_empty() {
        let piece = buf.join("\n");
        out.push((piece.clone(), piece));
    }
    out
}

pub(crate) fn chunker_sentence_split(text: &str) -> Vec<String> {
    // Mirrors Python's _SENT_RE: split on whitespace that follows `.!?` and
    // precedes an uppercase letter or `(`. Rust's regex crate doesn't
    // support lookahead, so walk char-by-char.
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        current.push(c);
        if matches!(c, '.' | '!' | '?') {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && (chars[j].is_ascii_uppercase() || chars[j] == '(') {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    sentences.push(trimmed);
                }
                current.clear();
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        sentences.push(trimmed);
    }
    sentences
}

/// Greedy-pack blocks into chunks bounded by max_tokens. Mirrors
/// chunk.py:_pack_chunks. Blocks exceeding max_tokens are split via
/// chunker_split_oversize_block (table rows, sentences, or word-window
/// fallback) so every emitted chunk fits the budget.
pub(crate) fn chunker_pack(blocks: Vec<ChunkBlock>, max_tokens: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut ord_counter: i64 = 0;
    let mut current_text: Vec<String> = Vec::new();
    let mut current_def: Vec<String> = Vec::new();
    let mut current_words: usize = 0;
    let mut current_anchor: Option<String> = None;

    let flush = |current_text: &mut Vec<String>,
                 current_def: &mut Vec<String>,
                 current_words: &mut usize,
                 current_anchor: &mut Option<String>,
                 ord_counter: &mut i64,
                 chunks: &mut Vec<Chunk>| {
        if current_text.is_empty() {
            return;
        }
        let text = current_text.join("\n\n").trim().to_string();
        let defn = current_def.join("\n\n").trim().to_string();
        chunks.push(Chunk {
            ord: *ord_counter,
            anchor: current_anchor.take(),
            text: text.clone(),
            definition_text: if defn != text && !defn.is_empty() {
                Some(defn)
            } else {
                None
            },
        });
        *ord_counter += 1;
        current_text.clear();
        current_def.clear();
        *current_words = 0;
    };

    for block in blocks {
        let block_words = block.text.split_whitespace().count();
        let block_tokens = std::cmp::max(1, ((block_words as f64) * 1.3) as usize);
        if block_tokens > max_tokens {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut ord_counter,
                &mut chunks,
            );
            // Split oversize block into pieces that fit max_tokens.
            for (text, defn) in chunker_split_oversize_block(&block, max_tokens) {
                chunks.push(Chunk {
                    ord: ord_counter,
                    anchor: block.anchor.clone(),
                    text: text.clone(),
                    definition_text: if defn != text { Some(defn) } else { None },
                });
                ord_counter += 1;
            }
            continue;
        }
        // [IB-22] Project token count from accumulated raw words, not summed
        // per-block integer token estimates, so truncation drift cannot build up.
        let projected_tokens =
            std::cmp::max(1, (((current_words + block_words) as f64) * 1.3) as usize);
        if projected_tokens > max_tokens && !current_text.is_empty() {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut ord_counter,
                &mut chunks,
            );
        }
        current_text.push(block.text.clone());
        current_def.push(block.definition_text);
        current_words += block_words;
        if current_anchor.is_none() && block.anchor.is_some() {
            current_anchor = block.anchor;
        }
    }
    flush(
        &mut current_text,
        &mut current_def,
        &mut current_words,
        &mut current_anchor,
        &mut ord_counter,
        &mut chunks,
    );
    chunks
}

pub(crate) fn chunk_html(html: &str, root_title: Option<&str>, max_tokens: usize) -> Vec<Chunk> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = scraper::Html::parse_fragment(html);
    let referenced = collect_referenced_anchors(&doc);
    let root = doc.root_element();
    let mut blocks: Vec<ChunkBlock> = Vec::new();
    // Find the first <body> or fall back to root. parse_fragment wraps
    // content in <html><body>, but we want to walk just the body's children.
    let body_sel = scraper::Selector::parse("body").unwrap();
    let walk_root = doc.select(&body_sel).next().unwrap_or(root);
    chunker_walk(walk_root, &mut blocks, &referenced, root_title);
    chunker_pack(blocks, max_tokens)
}

// ----- end chunker -----

// ----- Metadata helpers (port of src/ato_mcp/indexer/metadata.py) -----
