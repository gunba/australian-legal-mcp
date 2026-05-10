"""Doc-navigation extraction.

Walks the cleaned HTML for a single doc and classifies every `<a href>` as:

- ``in_doc``: an anchor reference (#X) whose target ``<a name="X">`` or
  ``[id="X"]`` lives elsewhere in the same doc. Surfaces as paragraph
  navigation, ToC entries, footnote refs.
- ``sister``: an external doc link (different doc_id, no PiT) — typically
  errata, addenda, or related rulings.
- ``history``: an external doc link to the same or another doc with a
  ``PiT=<YYYYMMDDHHMMSS>`` timestamp — a historical (point-in-time) version.

Label resolution priority:
1. If the anchor sits inside a `<tr>` whose other cells contain plain text:
   use those sibling cells' text concatenated.
2. Otherwise: use the anchor's own visible text.
3. For history links: append/derive a date string from the PiT timestamp.

This module deliberately does NOT depend on element class, id, or heading
text. ATO is inconsistent on those; the URL shape and DOM ancestry are the
stable signals.

build.py post-processes the returned ``AnchorRef`` list:
- ``in_doc`` refs: look up ``target_anchor`` in the chunk text for
  ``[anchor:<target_anchor>]`` markers (emitted by the chunker) to find the
  target chunk_id.
- ``sister``/``history`` refs: ``target_doc_id`` (and ``target_pit``) are
  recorded as-is.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

from selectolax.parser import HTMLParser, Node

from .extract import _doc_id_from_ato_link


@dataclass(frozen=True)
class AnchorRef:
    kind: str  # 'in_doc' | 'sister' | 'history'
    label: str
    # in_doc: anchor name (resolved to chunk_id by build.py)
    target_anchor: str | None = None
    # sister/history: target doc_id (with @PiT suffix already applied for history)
    target_doc_id: str | None = None
    target_pit: str | None = None


def extract_anchors(html: str, *, source_doc_id: str) -> list[AnchorRef]:
    """Return the list of navigation anchors for a single doc.

    ``source_doc_id`` is the doc_id of the doc being extracted; used to
    distinguish self-references (history) from sister docs.
    """
    if not html.strip():
        return []
    tree = HTMLParser(html)
    targets = _collect_anchor_targets(tree)
    refs: list[AnchorRef] = []
    seen: set[tuple[str, str, str | None]] = set()  # (kind, key, label) for dedup
    for a in tree.css("a[href]"):
        href = a.attributes.get("href") or ""
        if href.startswith("#"):
            target = href[1:]
            if not target or target not in targets:
                continue
            label = _resolve_label(a)
            key = ("in_doc", target, label)
            if key in seen:
                continue
            seen.add(key)
            refs.append(
                AnchorRef(kind="in_doc", label=label, target_anchor=target)
            )
            continue
        resolved = _doc_id_from_ato_link(href)
        if not resolved:
            continue
        target_doc_id, pit = resolved
        if pit:
            # Historical version (same or other doc + PiT timestamp).
            label = _resolve_label(a, default_date=_pit_to_date(pit))
            versioned_id = f"{target_doc_id}@{pit}"
            key = ("history", versioned_id, label)
            if key in seen:
                continue
            seen.add(key)
            refs.append(
                AnchorRef(
                    kind="history",
                    label=label,
                    target_doc_id=versioned_id,
                    target_pit=pit,
                )
            )
            continue
        if target_doc_id == source_doc_id:
            # Self-link without PiT — not a useful navigation entry.
            continue
        label = _resolve_label(a)
        key = ("sister", target_doc_id, label)
        if key in seen:
            continue
        seen.add(key)
        refs.append(
            AnchorRef(
                kind="sister",
                label=label,
                target_doc_id=target_doc_id,
            )
        )
    return refs


def _collect_anchor_targets(tree: HTMLParser) -> set[str]:
    """Return every `<a name="X">` and `[id="X"]` target name in the doc."""
    targets: set[str] = set()
    for a in tree.css("a[name]"):
        name = a.attributes.get("name")
        if name:
            targets.add(name)
    for el in tree.css("[id]"):
        nid = el.attributes.get("id")
        if nid:
            targets.add(nid)
    return targets


def _resolve_label(a: Node, *, default_date: str | None = None) -> str:
    """Best-effort label for an anchor.

    Priority: sibling cells in the same <tr>, then the anchor's own text,
    then the optional default_date (for history links with empty text).
    """
    own = (a.text(strip=True) or "").strip()
    sibling_text = _sibling_cells_text(a)
    parts: list[str] = []
    if sibling_text:
        parts.append(sibling_text)
    if own and own not in parts:
        parts.append(own)
    label = " ".join(parts).strip()
    if default_date:
        if label:
            label = f"{label} ({default_date})"
        else:
            label = default_date
    return label or "(unnamed)"


def _sibling_cells_text(a: Node) -> str:
    """If the anchor lives in a <tr>, concatenate the OTHER cells' text."""
    row = _ancestor(a, {"tr"})
    if row is None:
        return ""
    own_cell = _ancestor(a, {"td", "th"})
    parts: list[str] = []
    for cell in row.css("td, th"):
        if cell is own_cell:
            continue
        text = (cell.text(strip=True) or "").strip()
        if text:
            parts.append(text)
    return " ".join(parts).strip()


def _ancestor(node: Node, tags: set[str]) -> Node | None:
    current: Node | None = node.parent
    while current is not None:
        if (current.tag or "").lower() in tags:
            return current
        current = current.parent
    return None


def _pit_to_date(pit: str) -> str:
    """Convert a PiT timestamp (YYYYMMDDHHMMSS) to an ISO date YYYY-MM-DD.

    Returns the raw value if it doesn't match the expected shape.
    """
    s = pit.strip()
    if len(s) >= 8 and s[:8].isdigit():
        return f"{s[:4]}-{s[4:6]}-{s[6:8]}"
    return s


def anchor_target_to_chunk(
    anchor: str, chunk_texts: Iterable[tuple[int, str]]
) -> int | None:
    """Find the chunk_id whose text contains ``[anchor:<anchor>]``.

    ``chunk_texts`` is an iterable of (chunk_id, text) for one doc.
    """
    marker = f"[anchor:{anchor}]"
    for chunk_id, text in chunk_texts:
        if marker in text:
            return chunk_id
    return None
