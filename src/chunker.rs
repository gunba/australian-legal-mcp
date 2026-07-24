//! Block-aware chunker. Walks cleaned structural HTML into atomic blocks,
//! renders each to plaintext with markdown markers, then greedily packs blocks
//! into chunks bounded by `max_tokens` while preserving document order.

use crate::config::tokenizer_path;
use crate::html::{collect_referenced_anchors, push_anchor_marker, render_node};
use crate::{DOCUMENT_EMBEDDING_PREFIX, EMBEDDING_INPUT_MAX_TOKENS};
use legal_model::{is_canonical_public_component, AssetRef, DocumentId};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokenizers::Tokenizer;

// ----- Block-aware chunking -----
//
// Cleaned structural HTML becomes a flat list of atomic blocks, each rendered
// as plaintext with markdown markers and greedily packed within `max_tokens`.
// The public helpers expose HTML rendering, token estimation, and the stable
// intermediate block and chunk shapes used by the build pipeline.

// Checkpoints pin CHUNKER_FORMAT_VERSION; changing output shape
// forces an explicit fresh build instead of resuming stale chunk records.
pub(crate) const CHUNKER_FORMAT_VERSION: u32 = 9;
pub(crate) const EMBED_MAX_TOKENS: usize = EMBEDDING_INPUT_MAX_TOKENS;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Chunk {
    pub(crate) ord: i64,
    pub(crate) anchor: Option<String>,
    pub(crate) text: String,
    pub(crate) definition_text: Option<String>,
    #[serde(skip)]
    pub(crate) token_count: usize,
    #[serde(skip)]
    pub(crate) embedding_token_ids: Option<Vec<i64>>,
    /// Context repeated if exact tokenizer enforcement splits this chunk.
    #[serde(skip)]
    heading_context: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkBlock {
    pub(crate) text: String,
    pub(crate) definition_text: String,
    pub(crate) anchor: Option<String>,
    pub(crate) is_oversize_table: bool,
    /// Present only until pending headings are attached to substantive content.
    heading_level: Option<usize>,
    /// Heading text repeated when this block must be split.
    heading_context: Option<String>,
}

pub(crate) fn chunker_approx_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    std::cmp::max(1, ((words as f64) * 1.3) as usize)
}

/// Tighter whitespace normalisation than `normalise_paragraph_breaks`.
/// Collapses NBSP and horizontal-only runs to single spaces, collapses
/// ` *\n *` to `\n`, caps newline runs at two, normalises numeric-range
/// spacing, and tightens quoted text.
pub(crate) fn chunker_normalise_text(text: &str) -> String {
    static WS_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_PAD_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NEWLINE_RUN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static NUMERIC_RANGE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SPACED_QUOTE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let s = text.replace('\u{a0}', " ");
    // Collapse horizontal whitespace while preserving line boundaries.
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
    if let Some(anchor) = node
        .value()
        .attr("name")
        .or_else(|| node.value().attr("id"))
        .filter(|anchor| referenced.contains(*anchor))
    {
        return Some(anchor.to_string());
    }
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
            .map(|cell| {
                let mut text = chunker_render_inline(cell, referenced);
                if let Some(anchor) = cell
                    .value()
                    .attr("name")
                    .or_else(|| cell.value().attr("id"))
                    .filter(|anchor| referenced.contains(*anchor))
                {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    push_anchor_marker(&mut text, anchor);
                }
                chunker_normalise_text(&text)
            })
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
    Some(ChunkBlock {
        text: text.clone(),
        definition_text: text,
        anchor,
        is_oversize_table,
        heading_level: None,
        heading_context: None,
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
        heading_level: None,
        heading_context: None,
    })
}

/// Walk children of `parent` in document order and emit typed chunk blocks.
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
        // Preserve a text-only heading's level and order until the first
        // substantive block is available. Some official pages contain
        // invalid heading elements wrapped around structural content such as
        // a table of contents list. Walk that content as ordinary blocks;
        // flattening the entire subtree into repeated heading context can
        // consume the complete embedding budget before any body text.
        if HEADING_TAGS.contains(&tag) {
            chunker_flush_inline(&mut inline_parts, &mut inline_anchors, blocks);
            if chunker_has_structural_child(eref) {
                chunker_walk(eref, blocks, referenced, root_title);
                idx += 1;
                continue;
            }
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
                    heading_level: Some(level),
                    heading_context: None,
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
        // Pure inline element — render the element itself so wrapper-owned
        // document, asset, anchor, and emphasis markers are retained.
        let mut rendered = String::new();
        render_node(child, &mut rendered, referenced);
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
            heading_level: None,
            heading_context: None,
        });
    }
    inline_parts.clear();
    inline_anchors.clear();
}

fn chunker_attach_pending_headings(blocks: Vec<ChunkBlock>) -> Vec<ChunkBlock> {
    let mut output = Vec::with_capacity(blocks.len());
    let mut pending = Vec::new();

    for mut block in blocks {
        if block.heading_level.is_some() {
            pending.push(block);
            continue;
        }
        if pending.is_empty() {
            output.push(block);
            continue;
        }

        let context = pending
            .iter()
            .map(|heading| heading.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let definition_context = pending
            .iter()
            .map(|heading| heading.definition_text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let heading_anchor = pending.iter().find_map(|heading| heading.anchor.clone());
        block.text = format!("{context}\n\n{}", block.text);
        block.definition_text = format!("{definition_context}\n\n{}", block.definition_text);
        block.anchor = heading_anchor.or(block.anchor);
        block.heading_context = Some(context);
        output.push(block);
        pending.clear();
    }

    // A document ending in headings has no body to carry them. Preserve those
    // headings as ordinary blocks rather than dropping source text.
    for mut heading in pending {
        heading.heading_level = None;
        output.push(heading);
    }
    output
}

/// Split an oversize block into pieces that each fit within max_tokens.
/// Splitting follows this stable order:
///   1. oversize tables -> row split (rows stay whole).
///   2. prose -> sentence split, greedy-pack within budget.
///   3. word-window split as last-resort (single sentence/row over budget).
pub(crate) fn chunker_split_oversize_block(
    block: &ChunkBlock,
    max_tokens: usize,
) -> Vec<(String, String)> {
    let (body_text, body_definition, body_max_tokens) =
        if let Some(context) = block.heading_context.as_deref() {
            let prefix = format!("{context}\n\n");
            let body_text = block
                .text
                .strip_prefix(&prefix)
                .expect("heading context must prefix chunk block text");
            let body_definition = block
                .definition_text
                .strip_prefix(&prefix)
                .expect("heading context must prefix definition text");
            let context_words = context.split_whitespace().count();
            let max_total_words = (0..=max_tokens)
                .rev()
                .find(|words| ((*words as f64) * 1.3) as usize <= max_tokens)
                .unwrap_or(0);
            let body_words = max_total_words.saturating_sub(context_words).max(1);
            let body_max_tokens = std::cmp::max(1, ((body_words as f64) * 1.3) as usize);
            (body_text, body_definition, body_max_tokens)
        } else {
            (
                block.text.as_str(),
                block.definition_text.as_str(),
                max_tokens,
            )
        };

    let pieces = chunker_split_oversize_body(block, body_text, body_definition, body_max_tokens);
    if let Some(context) = block.heading_context.as_deref() {
        pieces
            .into_iter()
            .map(|(text, definition)| {
                (
                    format!("{context}\n\n{text}"),
                    format!("{context}\n\n{definition}"),
                )
            })
            .collect()
    } else {
        pieces
    }
}

fn chunker_split_oversize_body(
    block: &ChunkBlock,
    body_text: &str,
    body_definition: &str,
    max_tokens: usize,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if block.is_oversize_table {
        for (piece, defn) in chunker_table_text_split(body_text, max_tokens) {
            for p in chunker_enforce_max_tokens(&piece, &defn, max_tokens) {
                out.push(p);
            }
        }
        return out;
    }
    // Prose: sentence-split, greedy-pack.
    let sentences = chunker_sentence_split(body_text);
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
        let definition = if piece == body_text {
            body_definition
        } else {
            &piece
        };
        for p in chunker_enforce_max_tokens(&piece, definition, max_tokens) {
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
    let target_words = std::cmp::max(1, ((max_tokens as f64) / 1.4) as usize);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut remaining = text.trim();
    while !remaining.is_empty() {
        let words = chunker_word_spans(remaining);
        if words.len() <= target_words {
            out.push((remaining.to_string(), remaining.to_string()));
            break;
        }
        let candidate = words[target_words - 1].end;
        let split = chunker_typed_marker_spans(remaining)
            .into_iter()
            .find(|marker| marker.start < candidate && candidate < marker.end)
            .map_or(candidate, |marker| {
                if marker.start > 0 {
                    marker.start
                } else {
                    marker.end
                }
            });
        let piece = remaining[..split].trim_end().to_string();
        if piece.is_empty() {
            break;
        }
        out.push((piece.clone(), piece));
        remaining = remaining[split..].trim_start();
    }
    out
}

fn chunker_word_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    static WORD_RE: OnceLock<Regex> = OnceLock::new();
    WORD_RE
        .get_or_init(|| Regex::new(r"\S+").expect("valid word regex"))
        .find_iter(text)
        .map(|word| word.start()..word.end())
        .collect()
}

fn chunker_table_text_split(table_text: &str, max_tokens: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let mut buf_tokens: usize = 0;
    for row in table_text
        .lines()
        .map(str::trim)
        .filter(|row| !row.is_empty())
    {
        let row = row.to_string();
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
    // Split on whitespace that follows `.!?` and precedes an uppercase
    // letter or `(`. The regex crate lacks lookahead, so scan characters.
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

/// Greedily pack blocks into chunks bounded by max_tokens. Blocks exceeding
/// max_tokens are split via chunker_split_oversize_block (table rows,
/// sentences, or word-window
/// fallback) so every emitted chunk fits the budget.
pub(crate) fn chunker_pack(blocks: Vec<ChunkBlock>, max_tokens: usize) -> Vec<Chunk> {
    let blocks = chunker_attach_pending_headings(blocks);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut ord_counter: i64 = 0;
    let mut current_text: Vec<String> = Vec::new();
    let mut current_def: Vec<String> = Vec::new();
    let mut current_words: usize = 0;
    let mut current_anchor: Option<String> = None;
    let mut current_heading_context: Option<String> = None;

    let flush = |current_text: &mut Vec<String>,
                 current_def: &mut Vec<String>,
                 current_words: &mut usize,
                 current_anchor: &mut Option<String>,
                 current_heading_context: &mut Option<String>,
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
            token_count: 0,
            embedding_token_ids: None,
            heading_context: current_heading_context.take(),
        });
        *ord_counter += 1;
        current_text.clear();
        current_def.clear();
        *current_words = 0;
    };

    for block in blocks {
        if block.heading_context.is_some() && !current_text.is_empty() {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut current_heading_context,
                &mut ord_counter,
                &mut chunks,
            );
        }
        let block_words = block.text.split_whitespace().count();
        let block_tokens = std::cmp::max(1, ((block_words as f64) * 1.3) as usize);
        if block_tokens > max_tokens {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut current_heading_context,
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
                    token_count: 0,
                    embedding_token_ids: None,
                    heading_context: block.heading_context.clone(),
                });
                ord_counter += 1;
            }
            continue;
        }
        // Project token count from accumulated raw words, not summed
        // per-block integer token estimates, so truncation drift cannot build up.
        let projected_tokens =
            std::cmp::max(1, (((current_words + block_words) as f64) * 1.3) as usize);
        if projected_tokens > max_tokens && !current_text.is_empty() {
            flush(
                &mut current_text,
                &mut current_def,
                &mut current_words,
                &mut current_anchor,
                &mut current_heading_context,
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
        if current_heading_context.is_none() {
            current_heading_context = block.heading_context;
        }
    }
    flush(
        &mut current_text,
        &mut current_def,
        &mut current_words,
        &mut current_anchor,
        &mut current_heading_context,
        &mut ord_counter,
        &mut chunks,
    );
    chunks
}

#[cfg(test)]
fn chunker_enforce_final_token_limit<F>(
    chunks: Vec<Chunk>,
    max_tokens: usize,
    token_count: F,
) -> Vec<Chunk>
where
    F: Fn(&str) -> usize,
{
    chunker_enforce_final_token_limit_result(chunks, max_tokens, |text| Ok(token_count(text)))
        .expect("infallible token counter failed")
}

fn chunker_enforce_final_token_limit_result<F>(
    chunks: Vec<Chunk>,
    max_tokens: usize,
    token_count: F,
) -> anyhow::Result<Vec<Chunk>>
where
    F: Fn(&str) -> anyhow::Result<usize>,
{
    chunker_enforce_final_token_limit_prepared(chunks, max_tokens, |text| {
        token_count(text).map(|count| (count, None))
    })
}

fn chunker_enforce_final_token_limit_prepared<F>(
    chunks: Vec<Chunk>,
    max_tokens: usize,
    prepare_tokens: F,
) -> anyhow::Result<Vec<Chunk>>
where
    F: Fn(&str) -> anyhow::Result<(usize, Option<Vec<i64>>)>,
{
    let mut output = Vec::new();
    for mut chunk in chunks {
        let prefixed_tokens =
            |text: &str| prepare_tokens(&format!("{DOCUMENT_EMBEDDING_PREFIX}{text}"));
        let (chunk_token_count, chunk_token_ids) = prefixed_tokens(&chunk.text)?;
        if chunk_token_count <= max_tokens {
            chunk.token_count = chunk_token_count;
            chunk.embedding_token_ids = chunk_token_ids;
            output.push(chunk);
            continue;
        }

        let heading_context = chunk.heading_context.clone();
        let candidate_heading_prefix = heading_context
            .as_deref()
            .map(|context| format!("{context}\n\n"));
        let attached_body = candidate_heading_prefix.as_deref().map(|prefix| {
            chunk
                .text
                .strip_prefix(prefix)
                .expect("heading context must prefix chunk text")
        });
        // Repeating heading context is useful only when at least one body
        // character fits beside it. If that is mathematically impossible,
        // split the complete heading-plus-body stream instead. This keeps the
        // chunker total and preserves all source text for arbitrarily long
        // valid headings.
        let repeat_heading = match (candidate_heading_prefix.as_deref(), attached_body) {
            (Some(prefix), Some(body)) => match body.chars().next() {
                Some(first) => {
                    let first_end = first.len_utf8();
                    let first_fits = prefixed_tokens(&format!("{prefix}{}", &body[..first_end]))?.0
                        <= max_tokens;
                    let mut markers_fit = true;
                    for marker in chunker_typed_marker_spans(body) {
                        if prefixed_tokens(&format!("{prefix}{}", &body[marker]))?.0 > max_tokens {
                            markers_fit = false;
                            break;
                        }
                    }
                    first_fits && markers_fit
                }
                None => false,
            },
            _ => false,
        };
        let heading_prefix = repeat_heading
            .then(|| candidate_heading_prefix.clone())
            .flatten();
        let output_heading_context = repeat_heading.then(|| heading_context.clone()).flatten();
        let mut pieces = Vec::new();
        let mut remaining = if repeat_heading {
            attached_body.expect("repeated heading has an attached body")
        } else {
            chunk.text.as_str()
        };
        let with_heading = |text: &str| {
            heading_prefix
                .as_deref()
                .map_or_else(|| text.to_string(), |prefix| format!("{prefix}{text}"))
        };
        while !remaining.is_empty() {
            let remaining_text = with_heading(remaining);
            let (remaining_count, remaining_ids) = prefixed_tokens(&remaining_text)?;
            if remaining_count <= max_tokens {
                pieces.push((remaining_text, remaining_count, remaining_ids));
                break;
            }
            let boundaries = chunker_safe_split_boundaries(remaining);
            let mut low = 0usize;
            let mut high = boundaries.len();
            while low < high {
                let mid = low + (high - low) / 2;
                if prefixed_tokens(&with_heading(&remaining[..boundaries[mid]]))?.0 <= max_tokens {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low == 0 {
                if chunker_typed_marker_spans(remaining)
                    .first()
                    .is_some_and(|marker| marker.start == 0)
                {
                    anyhow::bail!("a typed marker exceeds the tokenizer limit");
                }
                anyhow::bail!("a single character exceeds the tokenizer limit");
            }
            let split = boundaries[low - 1];
            let piece = with_heading(&remaining[..split]);
            let (piece_count, piece_ids) = prefixed_tokens(&piece)?;
            pieces.push((piece, piece_count, piece_ids));
            remaining = &remaining[split..];
        }

        for (piece_index, (piece, token_count, token_ids)) in pieces.into_iter().enumerate() {
            output.push(Chunk {
                ord: output.len() as i64,
                anchor: chunk.anchor.clone(),
                text: piece,
                definition_text: (piece_index == 0)
                    .then(|| chunk.definition_text.clone())
                    .flatten(),
                token_count,
                embedding_token_ids: token_ids,
                heading_context: output_heading_context.clone(),
            });
        }
    }
    for (ord, chunk) in output.iter_mut().enumerate() {
        chunk.ord = ord as i64;
    }
    Ok(output)
}

fn chunker_typed_marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    const PREFIXES: &[&str] = &["[anchor:", "[asset:", "[doc:", "[fetch:"];
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let Some(relative_start) = text[cursor..].find('[') else {
            break;
        };
        let start = cursor + relative_start;
        let Some(prefix) = PREFIXES
            .iter()
            .find(|prefix| text[start..].starts_with(**prefix))
        else {
            cursor = start + 1;
            continue;
        };
        let value_start = start + prefix.len();
        let Some(relative_end) = text[value_start..].find(']') else {
            cursor = start + 1;
            continue;
        };
        let end = value_start + relative_end + 1;
        let value = &text[value_start..end - 1];
        let valid = match *prefix {
            "[anchor:" => is_canonical_public_component(value),
            "[asset:" => value.parse::<AssetRef>().is_ok(),
            "[doc:" if value.len() <= 512 => {
                let identity_end = value
                    .char_indices()
                    .find(|(_, character)| character.is_whitespace() || *character == '@')
                    .map_or(value.len(), |(index, _)| index);
                value[..identity_end].parse::<DocumentId>().is_ok()
            }
            "[fetch:" => crate::uri::parse_doc_uri(value).is_ok(),
            _ => false,
        };
        if valid && end - start <= 600 {
            spans.push(start..end);
        }
        cursor = end;
    }
    spans
}

fn chunker_safe_split_boundaries(text: &str) -> Vec<usize> {
    let protected = chunker_typed_marker_spans(text);
    let mut protected_index = 0usize;
    let mut output = Vec::new();
    for boundary in text
        .char_indices()
        .map(|(index, _)| index)
        .skip(1)
        .chain(std::iter::once(text.len()))
    {
        while protected
            .get(protected_index)
            .is_some_and(|span| span.end <= boundary)
        {
            protected_index += 1;
        }
        if protected
            .get(protected_index)
            .is_none_or(|span| !(span.start < boundary && boundary < span.end))
        {
            output.push(boundary);
        }
    }
    output
}

fn chunker_apply_live_tokenizer_limit(
    chunks: Vec<Chunk>,
    max_tokens: usize,
) -> anyhow::Result<Vec<Chunk>> {
    let Ok(path) = tokenizer_path() else {
        return Ok(chunks);
    };
    static TOKENIZERS: OnceLock<Mutex<HashMap<PathBuf, Arc<Tokenizer>>>> = OnceLock::new();
    let cache = TOKENIZERS.get_or_init(|| Mutex::new(HashMap::new()));
    let tokenizer = {
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow::anyhow!("tokenizer cache lock is poisoned"))?;
        if let Some(tokenizer) = cache.get(&path) {
            Arc::clone(tokenizer)
        } else {
            let mut tokenizer = Tokenizer::from_file(&path)
                .map_err(|error| anyhow::anyhow!("loading installed tokenizer: {error}"))?;
            tokenizer
                .with_truncation(None)
                .map_err(|error| anyhow::anyhow!("disabling tokenizer truncation: {error}"))?;
            tokenizer.with_padding(None);
            let tokenizer = Arc::new(tokenizer);
            cache.insert(path, Arc::clone(&tokenizer));
            tokenizer
        }
    };
    chunker_enforce_final_token_limit_result(chunks, max_tokens, |text| {
        Ok(tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("installed tokenizer failed: {error}"))?
            .get_ids()
            .len())
    })
}

pub(crate) fn chunk_html(
    html: &str,
    root_title: Option<&str>,
    max_tokens: usize,
) -> anyhow::Result<Vec<Chunk>> {
    let chunks = chunk_html_packed(html, root_title, max_tokens);
    chunker_apply_live_tokenizer_limit(chunks, max_tokens)
}

#[cfg(test)]
pub(crate) fn chunk_html_with_token_count<F>(
    html: &str,
    root_title: Option<&str>,
    max_tokens: usize,
    token_count: F,
) -> anyhow::Result<Vec<Chunk>>
where
    F: Fn(&str) -> anyhow::Result<usize>,
{
    let doc = scraper::Html::parse_fragment(html);
    chunk_fragment_with_token_count(&doc, root_title, max_tokens, token_count)
}

#[cfg(test)]
pub(crate) fn chunk_fragment_with_token_count<F>(
    doc: &scraper::Html,
    root_title: Option<&str>,
    max_tokens: usize,
    token_count: F,
) -> anyhow::Result<Vec<Chunk>>
where
    F: Fn(&str) -> anyhow::Result<usize>,
{
    let chunks = chunk_fragment_packed(doc, root_title, max_tokens);
    chunker_enforce_final_token_limit_result(chunks, max_tokens, token_count)
}

pub(crate) fn chunk_fragment_with_prepared_tokens<F>(
    doc: &scraper::Html,
    root_title: Option<&str>,
    max_tokens: usize,
    prepare_tokens: F,
) -> anyhow::Result<Vec<Chunk>>
where
    F: Fn(&str) -> anyhow::Result<(usize, Option<Vec<i64>>)>,
{
    let chunks = chunk_fragment_packed(doc, root_title, max_tokens);
    chunker_enforce_final_token_limit_prepared(chunks, max_tokens, prepare_tokens)
}

fn chunk_html_packed(html: &str, root_title: Option<&str>, max_tokens: usize) -> Vec<Chunk> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    let doc = scraper::Html::parse_fragment(html);
    chunk_fragment_packed(&doc, root_title, max_tokens)
}

fn chunk_fragment_packed(
    doc: &scraper::Html,
    root_title: Option<&str>,
    max_tokens: usize,
) -> Vec<Chunk> {
    let referenced = collect_referenced_anchors(doc);
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

// ----- Source metadata helpers -----

#[cfg(test)]
mod tests {
    use super::{
        chunk_fragment_with_prepared_tokens, chunk_html_packed, chunk_html_with_token_count,
        chunker_enforce_final_token_limit, chunker_normalise_text, Chunk, CHUNKER_FORMAT_VERSION,
    };

    #[test]
    fn heading_and_first_substantive_body_are_one_chunk() {
        let chunks = chunk_html_packed(
            "<h2 id='rule'>Application rule</h2><p>The rule applies to every applicant.</p>",
            None,
            64,
        );

        assert_eq!(CHUNKER_FORMAT_VERSION, 9);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].text,
            "## Application rule\n\nThe rule applies to every applicant."
        );
        assert_eq!(chunks[0].anchor.as_deref(), Some("rule"));
        assert_ne!(chunks[0].text, "## Application rule");
    }

    #[test]
    fn consecutive_headings_keep_levels_and_source_order() {
        let chunks = chunk_html_packed(
            "<section><h2>Part 2</h2><div><h3>Division 4</h3>\
             <article><h4>18 Eligibility</h4><p>A person is eligible when the criteria are met.</p>\
             </article></div></section>",
            None,
            64,
        );

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].text,
            "## Part 2\n\n### Division 4\n\n#### 18 Eligibility\n\n\
             A person is eligible when the criteria are met."
        );
    }

    #[test]
    fn same_level_headings_are_not_dropped_or_reordered() {
        let chunks = chunk_html_packed(
            "<h2>Schedule 1</h2><h2>Amendment 3</h2><p>Insert the following text.</p>",
            None,
            64,
        );

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].text,
            "## Schedule 1\n\n## Amendment 3\n\nInsert the following text."
        );
    }

    #[test]
    fn empty_blocks_do_not_consume_pending_headings() {
        let chunks = chunk_html_packed(
            "<h3>Operative provision</h3><p>  </p><div>\n</div><p>Substantive text.</p>",
            None,
            64,
        );

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].text,
            "### Operative provision\n\nSubstantive text."
        );
    }

    #[test]
    fn approximate_oversize_continuations_repeat_heading_context() {
        let context = "## Section 18";
        let body = "one two three four five six seven eight nine ten eleven twelve";
        let chunks = chunk_html_packed(&format!("<h2>Section 18</h2><p>{body}</p>"), None, 8);

        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.text.starts_with(&format!("{context}\n\n"))));
        assert!(chunks.iter().all(|chunk| chunk.text != context));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.strip_prefix(&format!("{context}\n\n")).unwrap())
                .collect::<Vec<_>>()
                .join(" "),
            body
        );
    }

    #[test]
    fn exact_tokenizer_continuations_repeat_context_in_text_and_embedding_input() {
        let html = scraper::Html::parse_fragment("<h2>Scope</h2><p>café naïve résumé wording</p>");
        let chunks = chunk_fragment_with_prepared_tokens(&html, None, 20, |text| {
            Ok((
                text.chars().count(),
                Some(text.bytes().map(i64::from).collect()),
            ))
        })
        .unwrap();
        let prefix = "## Scope\n\n";

        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| {
            chunk.text.starts_with(prefix)
                && chunk.token_count <= 20
                && chunk.embedding_token_ids.as_ref().unwrap()
                    == &chunk.text.bytes().map(i64::from).collect::<Vec<_>>()
        }));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.strip_prefix(prefix).unwrap())
                .collect::<String>(),
            "café naïve résumé wording"
        );
    }

    #[test]
    fn oversize_table_rows_repeat_heading_context() {
        let rows = (0..100)
            .map(|index| format!("<tr><td>row {index} alpha beta gamma</td></tr>"))
            .collect::<String>();
        let chunks = chunk_html_packed(
            &format!("<h3>Rates table</h3><table>{rows}</table>"),
            None,
            30,
        );
        let prefix = "### Rates table\n\n";

        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.text.starts_with(prefix) && chunk.text.contains("row ")));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.strip_prefix(prefix).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
            (0..100)
                .map(|index| format!("row {index} alpha beta gamma"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn documents_without_headings_keep_existing_packing() {
        let chunks = chunk_html_packed(
            "<p>alpha beta</p><p>gamma delta</p><p>epsilon zeta</p>",
            None,
            5,
        );

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha beta\n\ngamma delta", "epsilon zeta"]
        );
        assert!(chunks.iter().all(|chunk| chunk.heading_context.is_none()));
    }

    #[test]
    fn trailing_heading_is_preserved_when_no_body_follows() {
        let chunks = chunk_html_packed("<p>Body.</p><h2>Appendix</h2>", None, 64);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Body.\n\n## Appendix");
    }

    #[test]
    fn root_title_echo_does_not_create_heading_context() {
        let chunks = chunk_html_packed(
            "<h1>Example title</h1><p>Body.</p>",
            Some(" example TITLE "),
            64,
        );

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Body.");
        assert!(chunks[0].heading_context.is_none());
    }

    #[test]
    fn structural_content_nested_in_a_heading_is_not_repeated_as_context() {
        let items = (0..80)
            .map(|index| format!("<li><a href='#part-{index}'>Part {index}</a></li>"))
            .collect::<String>();
        let chunks = chunk_html_packed(
            &format!("<h3><ul>{items}</ul></h3><p>Operative body.</p>"),
            None,
            24,
        );
        let text = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(chunks.len() > 1);
        assert!(!text.contains("### Part"));
        assert_eq!(text.matches("Part 0").count(), 1);
        assert_eq!(text.matches("Part 79").count(), 1);
        assert_eq!(text.matches("Operative body.").count(), 1);
        assert!(chunks.iter().all(|chunk| chunk.heading_context.is_none()));
    }

    #[test]
    fn overlong_heading_context_falls_back_to_a_lossless_stream() {
        let expected = "## ABCDEFGHIJKL\n\nbody";
        let chunks =
            chunk_html_with_token_count("<h2>ABCDEFGHIJKL</h2><p>body</p>", None, 8, |text| {
                Ok(text.chars().count())
            })
            .unwrap();

        assert!(chunks.len() > 1);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            expected
        );
        assert!(chunks.iter().all(|chunk| chunk.token_count <= 8));
        assert!(chunks.iter().all(|chunk| chunk.heading_context.is_none()));
    }

    #[test]
    fn exact_tokenizer_split_keeps_typed_asset_markers_atomic() {
        let asset = "frl:C2004A05138/sha256-481f9ff2a748417d3d70eabbe16af8e25190263f069f56b4596904cbdd809c29";
        let marker = format!("[asset:{asset}]");
        let html = format!(
            "<p>{} <img data-asset-ref='{asset}' alt='formula'> {}</p>",
            "before ".repeat(12),
            "after ".repeat(12)
        );
        let expected = chunker_normalise_text(&format!(
            "{} [image: formula] {marker} {}",
            "before ".repeat(12),
            "after ".repeat(12)
        ));
        let chunks =
            chunk_html_with_token_count(&html, None, 160, |text| Ok(text.chars().count())).unwrap();
        let joined = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();

        assert!(chunks.len() > 1);
        assert_eq!(joined, expected);
        assert_eq!(joined.matches(&marker).count(), 1);
        assert!(chunks.iter().any(|chunk| chunk.text.contains(&marker)));
        assert!(chunks
            .iter()
            .all(|chunk| { !chunk.text.contains("[asset:") || chunk.text.contains(&marker) }));
        assert!(chunks.iter().all(|chunk| chunk.token_count <= 160));
    }

    #[test]
    fn approximate_split_keeps_qualified_document_markers_atomic() {
        let html = format!(
            "<p>{}<a data-doc-id='ato:PAC/X' data-view='HISTFT'>ref</a>{}</p>",
            "a ".repeat(363),
            " b".repeat(32)
        );
        let chunks = chunk_html_packed(&html, None, 512);
        let marker = "[doc:ato:PAC/X view=HISTFT]";
        let rendered = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(chunks.len() > 1);
        assert_eq!(rendered.matches(marker).count(), 1);
        assert!(!rendered.contains("[doc:ato:PAC/X view= HISTFT]"));
        assert!(chunks.iter().any(|chunk| chunk.text.contains(marker)));
    }

    #[test]
    fn direct_inline_wrappers_retain_typed_markers() {
        let chunks = chunk_html_packed(
            "<a data-doc-id='ato:PAC/X'>document</a>\
             <img data-asset-ref='frl:C1/image.png' alt='formula'>",
            None,
            64,
        );
        let rendered = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("document [doc:ato:PAC/X]"));
        assert!(rendered.contains("[image: formula] [asset:frl:C1/image.png]"));
    }

    #[test]
    fn oversize_table_split_retains_referenced_anchor_markers() {
        let rows = (0..30)
            .map(|index| {
                if index == 17 {
                    "<tr><td id='target'>target row alpha beta gamma</td></tr>".to_string()
                } else {
                    format!("<tr><td>row {index} alpha beta gamma</td></tr>")
                }
            })
            .collect::<String>();
        let chunks = chunk_html_packed(
            &format!("<p><a href='#target'>jump</a></p><table>{rows}</table>"),
            None,
            24,
        );
        let rendered = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(rendered.matches("[anchor:target]").count(), 1);
    }

    #[test]
    fn tokenizer_boundary_keeps_heading_attached_to_body() {
        let chunks =
            chunk_html_with_token_count("<h2>Rule</h2><p>1234567890</p>", None, 14, |text| {
                Ok(text.chars().count())
            })
            .unwrap();

        assert!(chunks
            .iter()
            .all(|chunk| chunk.text.starts_with("## Rule\n\n")));
        assert!(chunks.iter().all(|chunk| chunk.text != "## Rule"));
    }

    #[test]
    fn final_limit_uses_tokenizer_counts_including_the_embedding_prefix() {
        let chunks = vec![Chunk {
            ord: 9,
            anchor: Some("section".to_string()),
            text: "aa bb cc dd".to_string(),
            definition_text: Some("different".to_string()),
            token_count: 0,
            embedding_token_ids: None,
            heading_context: None,
        }];
        let chunks = chunker_enforce_final_token_limit(chunks, 5, |text| text.len());

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            "aa bb cc dd"
        );
        assert_eq!(
            chunks.iter().map(|chunk| chunk.ord).collect::<Vec<_>>(),
            (0..chunks.len() as i64).collect::<Vec<_>>()
        );
        assert!(chunks
            .iter()
            .all(|chunk| chunk.anchor.as_deref() == Some("section")));
        assert_eq!(chunks[0].definition_text.as_deref(), Some("different"));
        assert!(chunks
            .iter()
            .skip(1)
            .all(|chunk| chunk.definition_text.is_none()));
    }

    #[test]
    fn final_limit_keeps_a_chunk_at_the_exact_tokenizer_limit() {
        let chunks = vec![Chunk {
            ord: 0,
            anchor: None,
            text: "abc".to_string(),
            definition_text: None,
            token_count: 0,
            embedding_token_ids: None,
            heading_context: None,
        }];
        let chunks = chunker_enforce_final_token_limit(chunks, 3, |text| text.len());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "abc");
    }
}
