"""Definition extraction for compact statutory definition lookup."""
from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass


_TERM_RE = re.compile(r"\*\*\*\s*([^*\n][^*]{0,180}?)\s*\*\*\*", re.MULTILINE)
_WS_RE = re.compile(r"\s+")
_CUE_RE = re.compile(
    r"^\s*(?:,?\s*of\b|,?\s*in relation\b|:|means\b|includes\b|has\b|is\b|\(Repealed\b)",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class Definition:
    definition_id: str
    term: str
    norm_term: str
    doc_id: str
    source_title: str
    source_type: str
    scope: str | None
    heading_path: str
    anchor: str | None
    ord: int
    body: str


@dataclass(frozen=True)
class DefinitionChunk:
    ord: int
    heading_path: str
    anchor: str | None
    text: str


def normalize_term(term: str) -> str:
    term = term.replace("\\*", "*").replace("\\&", "&")
    term = term.strip(" \t\r\n:*")
    term = _WS_RE.sub(" ", term)
    return term.casefold()


def _clean_term(term: str) -> str:
    return _WS_RE.sub(" ", term.replace("\n", " ")).strip(" :*")


def _clean_body(body: str) -> str:
    body = body.strip()
    body = re.sub(r"\n{3,}", "\n\n", body)
    return body


def _definition_id(doc_id: str, ord: int, term: str, body: str) -> str:
    h = hashlib.sha256()
    h.update(doc_id.encode("utf-8"))
    h.update(b"\0")
    h.update(str(ord).encode("ascii"))
    h.update(b"\0")
    h.update(normalize_term(term).encode("utf-8"))
    h.update(b"\0")
    h.update(body[:256].encode("utf-8"))
    return h.hexdigest()[:20]


def _scope_from_title(title: str, source_type: str, heading_path: str) -> str | None:
    if " s " in title:
        return title
    if heading_path:
        return heading_path
    return source_type or None


def extract_definitions(
    *,
    doc_id: str,
    source_title: str,
    source_type: str,
    chunks: list[DefinitionChunk],
) -> list[Definition]:
    """Extract simple markdown definition entries.

    The ATO corpus consistently marks defined terms as bold-italic
    ``***term***``. This extractor does not infer legal meaning; it only cuts
    the text from one marked term to the next when the following text looks
    like a definition clause.
    """

    out: list[Definition] = []
    seen: set[tuple[str, str, str]] = set()
    for chunk in chunks:
        matches = list(_TERM_RE.finditer(chunk.text))
        if not matches:
            continue
        for idx, match in enumerate(matches):
            term = _clean_term(match.group(1))
            if not term:
                continue
            next_start = matches[idx + 1].start() if idx + 1 < len(matches) else len(chunk.text)
            body_start = match.end()
            body = _clean_body(chunk.text[body_start:next_start])
            if body.casefold() in {"or", "and"} and idx + 1 < len(matches):
                next_match = matches[idx + 1]
                next_next = matches[idx + 2].start() if idx + 2 < len(matches) else len(chunk.text)
                body = _clean_body(chunk.text[next_match.end():next_next])
            if len(body) < 4 or not _CUE_RE.search(body):
                continue
            norm = normalize_term(term)
            key = (norm, doc_id, body)
            if key in seen:
                continue
            seen.add(key)
            out.append(
                Definition(
                    definition_id=_definition_id(doc_id, chunk.ord, term, body),
                    term=term,
                    norm_term=norm,
                    doc_id=doc_id,
                    source_title=source_title,
                    source_type=source_type,
                    scope=_scope_from_title(source_title, source_type, chunk.heading_path),
                    heading_path=chunk.heading_path,
                    anchor=chunk.anchor,
                    ord=chunk.ord,
                    body=body,
                )
            )
    return out
