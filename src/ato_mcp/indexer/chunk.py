"""Block-aware chunking for cleaned ATO HTML.

Walks DOM into a flat list of atomic blocks (tables, paragraphs, blockquotes,
lists, etc.), renders each into plaintext with markdown markers for headings
and emphasis, then greedy-packs blocks into chunks bounded by the embedder's
token budget.

Headings (h1-h6), strong, em, blockquote, pre, dt+dd pairs, list items — all
rendered inline so the embedder + BM25 see them as part of chunk body. There
is no separate heading_path metadata; the heading hierarchy IS the chunk text.

Anchor markup (`<a name="X">` and elements with `[id="X"]`) is preserved as
inline `[anchor:X]` markers when X is referenced by some `<a href="#X">` in
the same doc. build.py uses these to resolve in-doc anchor lookups to chunk
ids.
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

# Embedder truncates inputs at 1024 tokens; chunk packing must not exceed this.
EMBED_MAX_TOKENS = 1024
DEFAULT_MAX_TOKENS = EMBED_MAX_TOKENS

# Bump when chunker output for the SAME source HTML would differ in a way
# that the incremental-reuse path needs to detect — e.g. new inline-marker
# format, new atomic-block rules, tighter oversize fallback. The build's
# [IB-21] wholesale-reuse branch refuses to carry chunks from prior pack
# records whose stored chunker_format_version doesn't match this constant,
# forcing a re-extract+re-chunk under the current rules.
CHUNKER_FORMAT_VERSION = 3


@dataclass
class Chunk:
    ord: int
    anchor: str | None
    text: str
    definition_text: str | None = None


@dataclass
class _Block:
    """One atomic unit of chunkable content."""

    text: str
    definition_text: str
    anchor: str | None  # first anchor name appearing in this block, if any
    is_oversize_table: bool  # True if a single <table> exceeds max_tokens
    table_node: Node | None  # only set when is_oversize_table


def approx_tokens(text: str) -> int:
    """Rough token count: whitespace split + a constant factor for subwords."""
    return max(1, int(len(text.split()) * 1.3))


def html_to_text(html: str) -> str:
    """Return the plain semantic text used for metadata and diagnostics."""
    if not html.strip():
        return ""
    tree = HTMLParser(html)
    root = tree.body or tree.root
    if root is None:
        return ""
    referenced: set[str] = set()
    blocks: list[_Block] = []
    _walk(root, blocks, referenced, root_title=None, definition_markers=False)
    return "\n\n".join(b.text for b in blocks if b.text)


def chunk_html(
    html: str,
    *,
    root_title: str | None = None,
    max_tokens: int = DEFAULT_MAX_TOKENS,
) -> list[Chunk]:
    """Return chunks for a cleaned HTML fragment.

    Walks DOM into atomic blocks, renders inline markdown for headings and
    emphasis, then greedy-packs blocks into chunks bounded by ``max_tokens``.
    """
    if not html.strip():
        return []

    tree = HTMLParser(html)
    root = tree.body or tree.root
    if root is None:
        return []

    referenced = _collect_referenced_anchors(tree)
    blocks: list[_Block] = []
    _walk(root, blocks, referenced, root_title=root_title, definition_markers=False)
    # Re-walk for definition_text variant. Cheap and avoids interleaving state.
    definition_blocks: list[_Block] = []
    _walk(root, definition_blocks, referenced, root_title=root_title, definition_markers=True)
    # Splice definition_text back into the primary blocks (1:1 ordering).
    for primary, defn in zip(blocks, definition_blocks):
        primary.definition_text = defn.text

    return _pack_chunks(blocks, max_tokens=max_tokens)


def _collect_referenced_anchors(tree: HTMLParser) -> set[str]:
    """Return the set of anchor names referenced by `<a href="#X">` in this doc."""
    refs: set[str] = set()
    for a in tree.css('a[href^="#"]'):
        href = a.attributes.get("href") or ""
        if href.startswith("#") and len(href) > 1:
            refs.add(href[1:])
    return refs


def _walk(
    parent: Node,
    blocks: list[_Block],
    referenced_anchors: set[str],
    *,
    root_title: str | None,
    definition_markers: bool,
) -> None:
    """Emit atomic _Block records into ``blocks`` for the parent's children."""
    inline_parts: list[str] = []
    inline_anchors: list[str] = []
    child = parent.child
    while child is not None:
        tag = (child.tag or "").lower()
        if tag == "-text":
            text = _RAW_TEXT_WS_RE.sub(" ", child.text() or "")
            inline_parts.append(text)
            child = child.next
            continue
        # Combine adjacent <dt> + <dd> into a single block.
        if tag == "dt":
            _flush_inline(inline_parts, inline_anchors, blocks)
            block = _render_dt_dd_pair(
                child, referenced_anchors, definition_markers=definition_markers,
            )
            if block:
                blocks.append(block)
            child = _advance_past_dt_dd(child)
            continue
        if tag in _HEADING_TAGS:
            # Inline-render heading as its own block. Skip when heading text
            # equals the doc's root title (front-matter echo).
            _flush_inline(inline_parts, inline_anchors, blocks)
            heading_text = _normalise_text(
                _inline_text(child, referenced_anchors, definition_markers=definition_markers)
            )
            if heading_text and not _is_root_title_echo(heading_text, root_title):
                level = int(tag[1])
                rendered = f"{'#' * level} {heading_text}"
                anchor = _heading_anchor(child)
                blocks.append(
                    _Block(
                        text=rendered,
                        definition_text=rendered,
                        anchor=anchor,
                        is_oversize_table=False,
                        table_node=None,
                    )
                )
            child = child.next
            continue
        if _is_atomic_block(child):
            _flush_inline(inline_parts, inline_anchors, blocks)
            block = _render_block(child, referenced_anchors, definition_markers=definition_markers)
            if block:
                blocks.append(block)
            child = child.next
            continue
        if _contains_structural_child(child):
            _flush_inline(inline_parts, inline_anchors, blocks)
            _walk(
                child,
                blocks,
                referenced_anchors,
                root_title=root_title,
                definition_markers=definition_markers,
            )
            child = child.next
            continue
        # Pure inline content — accumulate.
        inline_parts.append(_inline_text(child, referenced_anchors, definition_markers=definition_markers))
        anchor = _first_referenced_anchor(child, referenced_anchors)
        if anchor:
            inline_anchors.append(anchor)
        child = child.next
    _flush_inline(inline_parts, inline_anchors, blocks)


def _advance_past_dt_dd(dt: Node) -> Node | None:
    """Skip the dt and a following <dd> if present."""
    nxt = dt.next
    if nxt is not None and (nxt.tag or "").lower() == "dd":
        return nxt.next
    return nxt


def _render_dt_dd_pair(
    dt: Node,
    referenced_anchors: set[str],
    *,
    definition_markers: bool,
) -> _Block | None:
    """Combine <dt>...</dt><dd>...</dd> into a single **term**\\nbody block."""
    term = _normalise_text(
        _inline_text_children(dt, referenced_anchors, definition_markers=definition_markers)
    )
    body = ""
    nxt = dt.next
    if nxt is not None and (nxt.tag or "").lower() == "dd":
        body = _normalise_text(
            _inline_text_children(nxt, referenced_anchors, definition_markers=definition_markers)
        )
    if not term and not body:
        return None
    rendered = f"**{term}**" if term else ""
    if body:
        rendered = f"{rendered}\n{body}" if rendered else body
    anchor = _first_referenced_anchor(dt, referenced_anchors)
    if anchor is None and nxt is not None and (nxt.tag or "").lower() == "dd":
        anchor = _first_referenced_anchor(nxt, referenced_anchors)
    return _Block(
        text=rendered,
        definition_text=rendered,
        anchor=anchor,
        is_oversize_table=False,
        table_node=None,
    )


def _flush_inline(
    parts: list[str],
    anchors: list[str],
    blocks: list[_Block],
) -> None:
    text = _normalise_text("".join(parts))
    parts.clear()
    if text:
        blocks.append(
            _Block(
                text=text,
                definition_text=text,
                anchor=anchors[0] if anchors else None,
                is_oversize_table=False,
                table_node=None,
            )
        )
    anchors.clear()


def _contains_structural_child(node: Node) -> bool:
    return any(_child_is_structural(child) for child in node.iter(include_text=False))


def _is_atomic_block(node: Node) -> bool:
    tag = (node.tag or "").lower()
    if tag == "table":
        return True
    if tag in {"p", "pre", "blockquote", "li", "figcaption", "caption"}:
        return True
    # dt/dd handled by the dt+dd pair rule in _walk; if they appear standalone
    # treat them as atomic.
    if tag in {"dt", "dd"}:
        return True
    if tag not in _BLOCK_TAGS:
        return False
    if tag in _CONTAINER_BLOCK_TAGS:
        return not any(_child_is_structural(child) for child in node.iter(include_text=False))
    return True


def _child_is_structural(node: Node) -> bool:
    tag = (node.tag or "").lower()
    return tag in _HEADING_TAGS or tag in _BLOCK_TAGS


def _render_block(
    node: Node,
    referenced_anchors: set[str],
    *,
    definition_markers: bool,
) -> _Block | None:
    """Render an atomic block to its plaintext form with markdown markers."""
    tag = (node.tag or "").lower()
    if tag == "table":
        text = _table_text(node, referenced_anchors, definition_markers=definition_markers)
        anchor = _first_referenced_anchor(node, referenced_anchors)
        oversize = approx_tokens(text) > EMBED_MAX_TOKENS
        return _Block(
            text=text,
            definition_text=text,
            anchor=anchor,
            is_oversize_table=oversize,
            table_node=node if oversize else None,
        )
    if tag == "blockquote":
        inner = _normalise_text(
            _inline_text_children(node, referenced_anchors, definition_markers=definition_markers)
        )
        text = "\n".join(f"> {line}" for line in inner.splitlines() if line) if inner else ""
    elif tag == "pre":
        inner = (node.text() or "").strip()
        text = f"```\n{inner}\n```" if inner else ""
    elif tag == "li":
        inner = _normalise_text(
            _inline_text_children(node, referenced_anchors, definition_markers=definition_markers)
        )
        text = f"- {inner}" if inner else ""
    elif tag in {"ul", "ol"}:
        items = []
        for li in node.css("li"):
            item = _normalise_text(
                _inline_text_children(li, referenced_anchors, definition_markers=definition_markers)
            )
            if item:
                items.append(f"- {item}")
        text = "\n".join(items)
    else:
        text = _normalise_text(
            _inline_text(node, referenced_anchors, definition_markers=definition_markers)
        )
    if not text:
        return None
    anchor = _first_referenced_anchor(node, referenced_anchors)
    return _Block(
        text=text,
        definition_text=text,
        anchor=anchor,
        is_oversize_table=False,
        table_node=None,
    )


def _table_text(
    table: Node,
    referenced_anchors: set[str],
    *,
    definition_markers: bool,
) -> str:
    """Render a table as pipe-separated rows."""
    rows: list[str] = []
    for row in table.css("tr"):
        cells = [
            _normalise_text(_inline_text(cell, referenced_anchors, definition_markers=definition_markers))
            for cell in row.css("th,td")
        ]
        cells = [cell for cell in cells if cell]
        if cells:
            rows.append(" | ".join(cells))
    if rows:
        return "\n".join(rows)
    return _normalise_text(
        _inline_text(table, referenced_anchors, definition_markers=definition_markers)
    )


def _heading_anchor(node: Node) -> str | None:
    anchor = node.attributes.get("id")
    if anchor:
        return anchor
    for child in node.css("a"):
        anchor = child.attributes.get("id") or child.attributes.get("name")
        if anchor:
            return anchor
    return None


def _first_referenced_anchor(node: Node, referenced_anchors: set[str]) -> str | None:
    """Return the first anchor name within ``node`` that is referenced."""
    for el in node.iter(include_text=False):
        name = el.attributes.get("name")
        if name and name in referenced_anchors:
            return name
        nid = el.attributes.get("id")
        if nid and nid in referenced_anchors:
            return nid
    return None


def _is_root_title_echo(heading: str, root_title: str | None) -> bool:
    if not root_title:
        return False
    return _normalise_text(heading).casefold() == _normalise_text(root_title).casefold()


def _inline_text(
    node: Node,
    referenced_anchors: set[str],
    *,
    definition_markers: bool,
) -> str:
    tag = (node.tag or "").lower()
    if tag == "br":
        return "\n"
    if tag == "-text":
        return _RAW_TEXT_WS_RE.sub(" ", node.text() or "")
    if tag in {"strong", "b"}:
        if definition_markers and _is_definition_term_node(node):
            term = _normalise_text(node.text(deep=True, separator=" ", strip=True) or "")
            return f"***{term}***" if term else ""
        inner = _inline_text_children(node, referenced_anchors, definition_markers=definition_markers).strip()
        return f"**{inner}**" if inner else ""
    if tag in {"em", "i"}:
        if definition_markers and _is_definition_term_node(node):
            term = _normalise_text(node.text(deep=True, separator=" ", strip=True) or "")
            return f"***{term}***" if term else ""
        inner = _inline_text_children(node, referenced_anchors, definition_markers=definition_markers).strip()
        return f"*{inner}*" if inner else ""
    if tag == "a":
        # Cross-doc link → [doc:X] (with @PiT suffix when present).
        doc_id = node.attributes.get("data-doc-id")
        pit = node.attributes.get("data-pit")
        view = node.attributes.get("data-view")
        if not doc_id:
            href = node.attributes.get("href")
            if href:
                from .extract import _doc_id_from_ato_link

                resolved = _doc_id_from_ato_link(href)
                if resolved:
                    doc_id, pit, view = resolved
        if doc_id:
            inner = _inline_text_children(
                node, referenced_anchors, definition_markers=definition_markers
            )
            qualifiers = ""
            if pit:
                qualifiers += f"@{pit}"
            if view:
                qualifiers += f" view={view}"
            marker = f"[doc:{doc_id}{qualifiers}]"
            return f"{inner} {marker}" if inner else marker
        # In-doc anchor target.
        name = node.attributes.get("name")
        if name and name in referenced_anchors:
            inner = _inline_text_children(
                node, referenced_anchors, definition_markers=definition_markers
            )
            return f"{inner} [anchor:{name}]" if inner else f"[anchor:{name}]"
    # Element with [id] referenced by an in-doc href.
    nid = node.attributes.get("id")
    if nid and nid in referenced_anchors:
        inner = _inline_text_children(
            node, referenced_anchors, definition_markers=definition_markers
        )
        return f"{inner} [anchor:{nid}]" if inner else f"[anchor:{nid}]"
    if tag == "span":
        asset_ref = node.attributes.get("data-asset-ref")
        if asset_ref:
            return f"[asset:{asset_ref}]"
    if definition_markers and _is_definition_term_node(node):
        term = _normalise_text(
            _inline_text_children(node, referenced_anchors, definition_markers=False)
        )
        return f"***{term}***" if term else ""
    return _inline_text_children(node, referenced_anchors, definition_markers=definition_markers)


def _inline_text_children(
    node: Node,
    referenced_anchors: set[str],
    *,
    definition_markers: bool,
) -> str:
    parts: list[str] = []
    child = node.child
    while child is not None:
        parts.append(
            _inline_text(child, referenced_anchors, definition_markers=definition_markers)
        )
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


def _pack_chunks(blocks: list[_Block], *, max_tokens: int) -> list[Chunk]:
    """Greedy block-aware packing. Never splits inside an atomic block.

    A single block whose token count exceeds ``max_tokens`` is split via:
      - row-split for `<table>` (rows stay whole)
      - sentence-split for everything else
    Each split piece becomes its own chunk.
    """
    chunks: list[Chunk] = []
    ord_counter = 0
    current_text: list[str] = []
    current_def: list[str] = []
    # [IB-22] Track raw word count, not summed `approx_tokens(block.text)`. The
    # per-block `int(words * 1.3)` truncation rounds down per block; the sum
    # of rounded values is up to one token short per block versus
    # `approx_tokens` of the joined text. With enough small blocks the
    # produced chunk lands a handful of tokens over `max_tokens`. Word counts
    # are additive across whitespace joins (`len("\n\n".join(b).split()) ==
    # sum(len(b.split()))`), so accumulating words gives an exact projection.
    current_words = 0
    current_anchor: str | None = None

    def flush() -> None:
        nonlocal ord_counter, current_text, current_def, current_words, current_anchor
        if not current_text:
            return
        text = "\n\n".join(current_text).strip()
        defn = "\n\n".join(current_def).strip()
        chunks.append(
            Chunk(
                ord=ord_counter,
                anchor=current_anchor,
                text=text,
                definition_text=defn if defn and defn != text else None,
            )
        )
        ord_counter += 1
        current_text = []
        current_def = []
        current_words = 0
        current_anchor = None

    for block in blocks:
        block_words = len(block.text.split())
        block_tokens = max(1, int(block_words * 1.3))
        if block_tokens > max_tokens:
            flush()
            for piece_text, piece_def in _split_oversize_block(block, max_tokens=max_tokens):
                chunks.append(
                    Chunk(
                        ord=ord_counter,
                        anchor=block.anchor,
                        text=piece_text,
                        definition_text=piece_def if piece_def != piece_text else None,
                    )
                )
                ord_counter += 1
            continue
        projected_tokens = max(1, int((current_words + block_words) * 1.3))
        if projected_tokens > max_tokens and current_text:
            flush()
        current_text.append(block.text)
        current_def.append(block.definition_text)
        current_words += block_words
        if current_anchor is None and block.anchor is not None:
            current_anchor = block.anchor

    flush()
    return chunks


def _split_oversize_block(
    block: _Block, *, max_tokens: int
) -> Iterable[tuple[str, str]]:
    """Yield (text, definition_text) pairs for a block that exceeds max_tokens.

    Hard cap: every emitted piece is guaranteed to be ≤ max_tokens, regardless
    of how granular the source structure is. Order of fallbacks:
    1. tables → row split (rows stay whole)
    2. prose  → sentence split
    3. single sentence / row still too large → word-window split

    The final word-window catches pathological cases where a single sentence
    or row alone exceeds max_tokens (court-case extracts with no terminal
    punctuation, oversize column cells, etc.).
    """
    if block.is_oversize_table and block.table_node is not None:
        for piece, defn in _table_row_split(block.table_node, max_tokens=max_tokens):
            yield from _enforce_max_tokens(piece, defn, max_tokens=max_tokens)
        return
    # Prose fallback: sentence split, greedy-pack within max_tokens.
    sentences = _sentence_split(block.text)
    buf: list[str] = []
    buf_tokens = 0
    for sentence in sentences:
        sent_tokens = approx_tokens(sentence)
        if buf and buf_tokens + sent_tokens > max_tokens:
            piece = " ".join(buf)
            yield from _enforce_max_tokens(piece, piece, max_tokens=max_tokens)
            buf = [sentence]
            buf_tokens = sent_tokens
        else:
            buf.append(sentence)
            buf_tokens += sent_tokens
    if buf:
        piece = " ".join(buf)
        yield from _enforce_max_tokens(piece, piece, max_tokens=max_tokens)


def _enforce_max_tokens(
    text: str, definition_text: str, *, max_tokens: int
) -> Iterable[tuple[str, str]]:
    """Last-resort word-window split for pieces still over the embedder cap."""
    if approx_tokens(text) <= max_tokens:
        yield text, definition_text
        return
    words = text.split()
    # Tokens ≈ words × 1.3; conservative bound to ensure the split piece fits.
    target_words = max(1, int(max_tokens / 1.4))
    for i in range(0, len(words), target_words):
        piece = " ".join(words[i : i + target_words])
        yield piece, piece


def _table_row_split(
    table: Node, *, max_tokens: int
) -> Iterable[tuple[str, str]]:
    """Split a table between <tr> rows. Each piece contains whole rows."""
    rows = []
    for row in table.css("tr"):
        cells = [
            _normalise_text(_inline_text(cell, set(), definition_markers=False))
            for cell in row.css("th,td")
        ]
        cells = [cell for cell in cells if cell]
        if cells:
            rows.append(" | ".join(cells))
    buf: list[str] = []
    buf_tokens = 0
    for row in rows:
        row_tokens = approx_tokens(row)
        if buf and buf_tokens + row_tokens > max_tokens:
            piece = "\n".join(buf)
            yield piece, piece
            buf = [row]
            buf_tokens = row_tokens
        else:
            buf.append(row)
            buf_tokens += row_tokens
    if buf:
        piece = "\n".join(buf)
        yield piece, piece


def _sentence_split(text: str) -> list[str]:
    return [s.strip() for s in _SENT_RE.split(text) if s.strip()]
