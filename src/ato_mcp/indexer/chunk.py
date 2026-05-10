"""Heading-aware chunking for cleaned ATO HTML.

The MCP document surface is cleaned source HTML, but semantic search should not
embed Markdown, URLs, or host-rendering artefacts. This module converts the
cleaned HTML fragment into compact plain text chunks for FTS/vector search while
preserving heading paths and source anchors.
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Iterable

from selectolax.parser import HTMLParser, Node

_HEADING_TAGS = {"h1", "h2", "h3", "h4", "h5", "h6"}
_BLOCK_TAGS = {
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
}
_CONTAINER_BLOCK_TAGS = {
    "article",
    "aside",
    "details",
    "div",
    "dl",
    "figure",
    "footer",
    "header",
    "main",
    "ol",
    "section",
    "ul",
}
_URL_HEADING_RE = re.compile(r"^/law/view/document\?docid=", re.IGNORECASE)
# Horizontal-only whitespace: keep \n/\r so <br>-introduced line breaks survive
# normalisation and reach the chunk plaintext.
_WS_RE = re.compile(r"[ \t\f\v]+")
# Raw text nodes carry source-formatting whitespace (line breaks + indent).
# Only <br> should introduce a structural newline, so flatten everything inside
# a text node to a single space at the leaf.
_RAW_TEXT_WS_RE = re.compile(r"\s+")
_NEWLINE_PAD_RE = re.compile(r" *\n *")
_NEWLINE_RUN_RE = re.compile(r"\n{3,}")
_NUMERIC_RANGE_RE = re.compile(r"(?<=\d)\s+-\s+(?=\d)")
_SPACED_QUOTE_RE = re.compile(r'"\s+([^"\n]*?)\s+"')
_SENT_RE = re.compile(r"(?<=[.!?])\s+(?=[A-Z(])")

DEFAULT_MAX_TOKENS = 900
DEFAULT_OVERLAP_TOKENS = 120
TITLE_SEP = " — "
PATH_SEP = " › "


@dataclass
class Chunk:
    ord: int
    heading_path: str
    anchor: str | None
    text: str
    definition_text: str | None = None


@dataclass
class _Section:
    heading_path: str
    anchor: str | None
    blocks: list[str]
    definition_blocks: list[str]


@dataclass
class _WalkState:
    root_title: str | None
    heading_stack: list[str]
    heading_levels: list[int]
    anchor: str | None
    blocks: list[str]
    definition_blocks: list[str]
    sections: list[_Section]


def approx_tokens(text: str) -> int:
    """Rough token count: whitespace split + a constant factor for subwords."""
    return max(1, int(len(text.split()) * 1.3))


def html_to_text(html: str) -> str:
    """Return the plain semantic text used for metadata and diagnostics."""
    if not html.strip():
        return ""
    tree = HTMLParser(html)
    root = tree.body or tree.root
    blocks: list[str] = []
    _collect_visible_blocks(root, blocks)
    return "\n\n".join(blocks)


def chunk_html(
    html: str,
    *,
    root_title: str | None = None,
    max_tokens: int = DEFAULT_MAX_TOKENS,
    overlap_tokens: int = DEFAULT_OVERLAP_TOKENS,
) -> list[Chunk]:
    """Return heading-aware plain-text chunks for a cleaned HTML fragment."""
    if not html.strip():
        return []

    tree = HTMLParser(html)
    root = tree.body or tree.root
    state = _WalkState(
        root_title=root_title,
        heading_stack=[root_title] if root_title else [],
        heading_levels=[0] if root_title else [],
        anchor=None,
        blocks=[],
        definition_blocks=[],
        sections=[],
    )
    _walk_children(root, state)
    _flush_section(state)

    chunks: list[Chunk] = []
    ord_counter = 0
    for section in state.sections:
        body = "\n\n".join(section.blocks).strip()
        definition_body = "\n\n".join(section.definition_blocks).strip()
        if not body:
            continue
        if approx_tokens(body) <= max_tokens:
            chunks.append(
                Chunk(
                    ord=ord_counter,
                    heading_path=section.heading_path,
                    anchor=section.anchor,
                    text=body,
                    definition_text=definition_body or None,
                )
            )
            ord_counter += 1
            continue

        parts = _split_long(body, max_tokens=max_tokens)
        definition_parts = _split_long(definition_body, max_tokens=max_tokens) if definition_body else []
        prev_tail = ""
        prev_def_tail = ""
        for idx, part in enumerate(parts):
            text = (prev_tail + "\n\n" + part).strip() if prev_tail else part
            definition_part = definition_parts[idx] if idx < len(definition_parts) else part
            definition_text = (
                (prev_def_tail + "\n\n" + definition_part).strip()
                if prev_def_tail
                else definition_part
            )
            chunks.append(
                Chunk(
                    ord=ord_counter,
                    heading_path=section.heading_path,
                    anchor=section.anchor,
                    text=text,
                    definition_text=definition_text or None,
                )
            )
            ord_counter += 1
            prev_tail = _tail_overlap(part, overlap_tokens)
            prev_def_tail = _tail_overlap(definition_part, overlap_tokens)
    return chunks


def _walk_children(parent: Node, state: _WalkState) -> None:
    inline_parts: list[str] = []
    inline_definition_parts: list[str] = []
    child = parent.child
    while child is not None:
        tag = (child.tag or "").lower()
        if tag == "-text":
            text = _RAW_TEXT_WS_RE.sub(" ", child.text() or "")
            inline_parts.append(text)
            inline_definition_parts.append(text)
            child = child.next
            continue
        if tag in _HEADING_TAGS:
            _flush_inline_block(inline_parts, inline_definition_parts, state)
            _flush_section(state)
            _push_heading(child, state)
            child = child.next
            continue
        if _is_atomic_block(child):
            _flush_inline_block(inline_parts, inline_definition_parts, state)
            _append_block(child, state)
            child = child.next
            continue
        if _contains_structural_child(child):
            _flush_inline_block(inline_parts, inline_definition_parts, state)
            _walk_children(child, state)
            child = child.next
            continue
        inline_parts.append(_inline_text(child, definition_markers=False))
        inline_definition_parts.append(_inline_text(child, definition_markers=True))
        child = child.next
    _flush_inline_block(inline_parts, inline_definition_parts, state)


def _flush_inline_block(
    parts: list[str],
    definition_parts: list[str],
    state: _WalkState,
) -> None:
    text = _normalise_text("".join(parts))
    definition_text = _normalise_text("".join(definition_parts))
    parts.clear()
    definition_parts.clear()
    if text:
        state.blocks.append(text)
        state.definition_blocks.append(definition_text or text)


def _contains_structural_child(node: Node) -> bool:
    return any(_child_is_structural(child) for child in node.iter(include_text=False))


def _collect_visible_blocks(parent: Node, blocks: list[str]) -> None:
    inline_parts: list[str] = []
    child = parent.child
    while child is not None:
        tag = (child.tag or "").lower()
        if tag == "-text":
            inline_parts.append(_RAW_TEXT_WS_RE.sub(" ", child.text() or ""))
            child = child.next
            continue
        if tag in _HEADING_TAGS or _is_atomic_block(child):
            _flush_visible_inline_block(inline_parts, blocks)
            text = _block_text(child, definition_markers=False)
            if text:
                blocks.append(text)
            child = child.next
            continue
        if _contains_structural_child(child):
            _flush_visible_inline_block(inline_parts, blocks)
            _collect_visible_blocks(child, blocks)
            child = child.next
            continue
        inline_parts.append(_inline_text(child, definition_markers=False))
        child = child.next
    _flush_visible_inline_block(inline_parts, blocks)


def _flush_visible_inline_block(parts: list[str], blocks: list[str]) -> None:
    text = _normalise_text("".join(parts))
    parts.clear()
    if text:
        blocks.append(text)


def _is_atomic_block(node: Node) -> bool:
    tag = (node.tag or "").lower()
    if tag == "table":
        return True
    if tag in {"p", "pre", "blockquote", "dt", "dd", "li", "figcaption", "caption"}:
        return True
    if tag not in _BLOCK_TAGS:
        return False
    if tag in _CONTAINER_BLOCK_TAGS:
        return not any(_child_is_structural(child) for child in node.iter(include_text=False))
    return True


def _child_is_structural(node: Node) -> bool:
    tag = (node.tag or "").lower()
    return tag in _HEADING_TAGS or tag in _BLOCK_TAGS


def _append_block(node: Node, state: _WalkState) -> None:
    text = _block_text(node, definition_markers=False)
    definition_text = _block_text(node, definition_markers=True)
    if text:
        state.blocks.append(text)
        state.definition_blocks.append(definition_text or text)


def _push_heading(node: Node, state: _WalkState) -> None:
    tag = (node.tag or "").lower()
    level = int(tag[1])
    heading = _normalise_text(_inline_text(node, definition_markers=False))
    while state.heading_levels and state.heading_levels[-1] >= level:
        state.heading_stack.pop()
        state.heading_levels.pop()
    if not (
        state.root_title
        and _norm_heading(heading) == _norm_heading(state.root_title)
    ):
        state.heading_stack.append(heading)
        state.heading_levels.append(level)
    state.anchor = _heading_anchor(node)


def _heading_anchor(node: Node) -> str | None:
    anchor = node.attributes.get("id")
    if anchor:
        return anchor
    for child in node.css("a"):
        anchor = child.attributes.get("id")
        if anchor:
            return anchor
    return None


def _flush_section(state: _WalkState) -> None:
    if not state.blocks:
        return
    heading_path = _path_trail(state.heading_stack)
    if state.root_title:
        heading_path = strip_title_prefix(heading_path)
    state.sections.append(
        _Section(
            heading_path=heading_path,
            anchor=state.anchor,
            blocks=state.blocks,
            definition_blocks=state.definition_blocks,
        )
    )
    state.blocks = []
    state.definition_blocks = []


def _block_text(node: Node, *, definition_markers: bool) -> str:
    tag = (node.tag or "").lower()
    if tag == "table":
        return _table_text(node, definition_markers=definition_markers)
    if tag in {"ul", "ol"}:
        items = [
            _normalise_text(_inline_text(li, definition_markers=definition_markers))
            for li in node.css("li")
        ]
        return "\n".join(f"- {item}" for item in items if item)
    return _normalise_text(_inline_text(node, definition_markers=definition_markers))


def _table_text(table: Node, *, definition_markers: bool) -> str:
    rows: list[str] = []
    for row in table.css("tr"):
        cells = [
            _normalise_text(_inline_text(cell, definition_markers=definition_markers))
            for cell in row.css("th,td")
        ]
        cells = [cell for cell in cells if cell]
        if cells:
            rows.append(" | ".join(cells))
    if rows:
        return "\n".join(rows)
    return _normalise_text(_inline_text(table, definition_markers=definition_markers))


def _inline_text(node: Node, *, definition_markers: bool) -> str:
    tag = (node.tag or "").lower()
    if tag == "br":
        return "\n"
    if tag == "-text":
        # Raw text nodes carry formatting whitespace (newlines + indentation
        # from the source markup). Collapse all whitespace so the only \n
        # that reaches downstream is the one emitted by <br>.
        raw = node.text() or ""
        return _RAW_TEXT_WS_RE.sub(" ", raw)
    # Annotate ATO cross-references and asset references inline so agents
    # working from chunk plaintext can chain to the referenced doc / call
    # get_asset without re-searching. Bracketed form survives BM25 windowing
    # and is additive for old consumers.
    if tag == "a":
        doc_id = node.attributes.get("data-doc-id")
        if not doc_id:
            href = node.attributes.get("href")
            if href:
                # extract.py converts internal ATO links to data-doc-id at
                # extract time, but URL-shape drift would otherwise silently
                # drop cross-references from chunk plaintext. One source of
                # truth for the parse — import the extractor's helper.
                from .extract import _doc_id_from_ato_link

                doc_id = _doc_id_from_ato_link(href)
        if doc_id:
            inner = _inline_text_children(node, definition_markers=definition_markers)
            return f"{inner} [doc:{doc_id}]" if inner else f"[doc:{doc_id}]"
    if tag == "span":
        asset_ref = node.attributes.get("data-asset-ref")
        if asset_ref:
            return f"[asset:{asset_ref}]"
    if definition_markers and _is_definition_term_node(node):
        term = _normalise_text(_inline_text_children(node, definition_markers=False))
        return f"***{term}***" if term else ""
    return _inline_text_children(node, definition_markers=definition_markers)


def _inline_text_children(node: Node, *, definition_markers: bool) -> str:
    parts: list[str] = []
    child = node.child
    while child is not None:
        parts.append(_inline_text(child, definition_markers=definition_markers))
        child = child.next
    return "".join(part for part in parts if part)


def _is_definition_term_node(node: Node) -> bool:
    tag = (node.tag or "").lower()
    if tag in {"strong", "b"}:
        return any((child.tag or "").lower() in {"em", "i"} for child in node.css("em,i"))
    if tag in {"em", "i"}:
        return any((child.tag or "").lower() in {"strong", "b"} for child in node.css("strong,b"))
    return False


def _normalise_text(text: str) -> str:
    text = text.replace("\xa0", " ")
    text = _WS_RE.sub(" ", text)
    text = _NEWLINE_PAD_RE.sub("\n", text)
    text = _NEWLINE_RUN_RE.sub("\n\n", text)
    text = text.strip()
    text = _NUMERIC_RANGE_RE.sub("-", text)
    text = _SPACED_QUOTE_RE.sub(r'"\1"', text)
    return text


def _norm_heading(text: str) -> str:
    return _normalise_text(text).casefold()


def strip_title_prefix(heading_path: str) -> str:
    """Drop the document title's front-matter echo from a heading path."""
    if not heading_path:
        return ""
    parts = heading_path.split(PATH_SEP)
    while parts and _URL_HEADING_RE.match(parts[0].strip()):
        parts = parts[1:]
    if not parts:
        return ""
    root = parts[0]
    parts = parts[1:]
    components = {_norm_heading(p) for p in root.split(TITLE_SEP) if p.strip()}
    components.add(_norm_heading(root))
    while parts and _norm_heading(parts[0]) in components:
        parts = parts[1:]
    return PATH_SEP.join(parts)


def _path_trail(stack: list[str]) -> str:
    return PATH_SEP.join(s for s in stack if s)


def _split_long(text: str, max_tokens: int) -> list[str]:
    paragraphs = [p.strip() for p in re.split(r"\n\s*\n", text) if p.strip()]
    out: list[str] = []
    buf: list[str] = []
    buf_tokens = 0
    for paragraph in paragraphs:
        paragraph_tokens = approx_tokens(paragraph)
        if paragraph_tokens > max_tokens:
            for sentence in _sentence_split(paragraph):
                for piece in _word_windows(sentence, max_tokens=max_tokens):
                    piece_tokens = approx_tokens(piece)
                    if buf_tokens + piece_tokens > max_tokens and buf:
                        out.append("\n\n".join(buf))
                        buf, buf_tokens = [], 0
                    buf.append(piece)
                    buf_tokens += piece_tokens
            continue
        if buf_tokens + paragraph_tokens > max_tokens and buf:
            out.append("\n\n".join(buf))
            buf, buf_tokens = [], 0
        buf.append(paragraph)
        buf_tokens += paragraph_tokens
    if buf:
        out.append("\n\n".join(buf))
    return out or [text]


def _sentence_split(text: str) -> list[str]:
    return [s.strip() for s in _SENT_RE.split(text) if s.strip()]


def _word_windows(text: str, max_tokens: int) -> list[str]:
    if approx_tokens(text) <= max_tokens:
        return [text]
    words = text.split()
    target_words = max(1, int(max_tokens / 1.3))
    return [
        " ".join(words[i : i + target_words])
        for i in range(0, len(words), target_words)
    ]


def _tail_overlap(text: str, overlap_tokens: int) -> str:
    words = text.split()
    target = max(1, int(overlap_tokens / 1.3))
    if len(words) <= target:
        return text
    return " ".join(words[-target:])


def chunk_texts(chunks: Iterable[Chunk]) -> list[str]:
    return [chunk.text for chunk in chunks]
