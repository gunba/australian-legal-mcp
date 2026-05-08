"""HTML -> structured markdown for ATO documents.

Strategy:
1. Parse with selectolax (lexbor) for speed.
2. Find the content container (``lawContents`` div, falling back to ``<article>``
   or ``<main>`` / ``<body>`` with nav stripped).
3. Walk headings and collect anchor IDs onto a ``{#anchor}`` suffix.
4. Convert to markdown via ``markdownify`` with a tight tag whitelist.
5. Emit a bare markdown string; the chunker handles heading-based splits.

Output also includes a plain-text ``outline`` (heading-path tuples) used by the
metadata/chunking steps.
"""
from __future__ import annotations

import html as html_lib
import re
from dataclasses import dataclass, field
from typing import Iterable
from urllib.parse import parse_qs, unquote, urlparse

from markdownify import markdownify
from selectolax.parser import HTMLParser, Node

_HEADING_TAGS = ("h1", "h2", "h3", "h4", "h5", "h6")
_NAV_LIKE_CLASSES = (
    "site-header",
    "global-header",
    "breadcrumb",
    "breadcrumbs",
    "site-footer",
    "page-footer",
    "navigation",
    "skip-links",
)


@dataclass
class ExtractedDoc:
    markdown: str
    title: str | None
    html_title: str | None = None  # raw <title> (browser tab text)
    headings: list[str] = field(default_factory=list)
    anchors: list[tuple[str, str]] = field(default_factory=list)  # (heading_text, anchor_id)


@dataclass(frozen=True)
class CurrencyInfo:
    """Currency / supersession metadata derived from the page HTML.

    All fields default to ``None`` when no relevant marker is found. ATO pages
    are inconsistent (rule body prose vs. status-alert panels vs. timeline
    tables vs. dedicated withdrawal-notice pages), so each field is filled
    independently — a missing field never blocks others.

    - ``withdrawn_date``: ISO ``yyyy-mm-dd`` if the page indicates the doc was
      withdrawn / superseded as of a specific date.
    - ``superseded_by``: short citation of the doc that replaces this one,
      when the page links/quotes one (e.g. ``"TR 2022/1"``).
    - ``replaces``: short citation of the doc that THIS doc replaces (the
      converse of ``superseded_by``), when stated.
    """

    withdrawn_date: str | None = None
    superseded_by: str | None = None
    replaces: str | None = None


@dataclass(frozen=True)
class _FormulaCell:
    index: int
    parts: list[str]
    has_break: bool
    has_underline: bool


def extract(html: str) -> ExtractedDoc:
    if not html or not html.strip():
        return ExtractedDoc(markdown="", title=None, html_title=None)

    tree = HTMLParser(html)
    html_title = _first_text(tree, "title")

    container = _pick_container(tree)
    if container is None:
        return ExtractedDoc(markdown="", title=None, html_title=html_title)

    _strip_noise(container)
    anchors = _collect_anchors(container)

    # Capture "title headings" — consecutive leading headings before any body
    # content. On ATO rulings that gives h1=doc_type, h2=code, h3=subject.
    lead_headings = _leading_headings(container)
    title = _compose_title(lead_headings) or html_title

    _rewrite_formula_html_tables(container)
    _inject_anchor_suffixes(container)

    headings = [
        (h.text(deep=True, separator=" ", strip=True) or "")
        for h in container.css(",".join(_HEADING_TAGS))
    ]
    html_fragment = container.html or ""
    markdown = markdownify(
        html_fragment,
        heading_style="ATX",
        bullets="-",
        strip=["script", "style", "iframe"],
    )
    markdown = _tidy_markdown(markdown)
    return ExtractedDoc(
        markdown=markdown, title=title, html_title=html_title,
        headings=headings, anchors=anchors,
    )


def _leading_headings(container: Node) -> list[str]:
    """Return text of consecutive headings at the start of the container.

    Walk direct children; collect ``h1-h6`` text until we hit a non-heading
    element with substantial content. Wrapper divs that only contain other
    headings count as heading-bearing too (so we don't stop at a ``LawFront``
    / ``front`` / ``LawPreamble`` div wrapping the h1/h2/h3 block).

    Once we have dived into a front-matter wrapper, we do not dive into any
    subsequent wrapper — the next div is almost always the body.
    """
    out: list[str] = []
    dived = False
    for child in container.iter(include_text=False):
        tag = (child.tag or "").lower()
        if tag in _HEADING_TAGS:
            text = child.text(deep=True, separator=" ", strip=True) or ""
            if text:
                out.append(text)
            continue
        if dived:
            break
        nested_headings = child.css(",".join(_HEADING_TAGS))
        non_heading_text = child.text(deep=True, separator=" ", strip=True) or ""
        if nested_headings and len(non_heading_text) <= 800:
            for h in nested_headings:
                t = h.text(deep=True, separator=" ", strip=True) or ""
                if t:
                    out.append(t)
            dived = True
            continue
        if non_heading_text.strip():
            break
    return out[:4]


def _compose_title(headings: list[str]) -> str | None:
    """Join a small number of leading headings into a readable title."""
    # [IB-07] Compose title from leading headings (h1=doc_type, h2=code, h3=subject on rulings); suppress prefix-overlap so 'TR' + 'TR 2024/3' doesn't double up; falls back to <title> at caller.
    cleaned = [h.strip() for h in headings if h and h.strip()]
    if not cleaned:
        return None
    if len(cleaned) == 1:
        return cleaned[0]
    out: list[str] = []
    for h in cleaned:
        if out and (out[-1].lower().startswith(h.lower()) or h.lower().startswith(out[-1].lower())):
            continue
        out.append(h)
    return " — ".join(out)


def _first_text(tree: HTMLParser, selector: str) -> str | None:
    node = tree.css_first(selector)
    if node is None:
        return None
    text = node.text(deep=True, separator=" ", strip=True)
    return text or None


def _pick_container(tree: HTMLParser) -> Node | None:
    # [IB-06] Container fallback chain absorbs the various wrapper IDs ATO has used over the years; final fallback is body or root.
    # ATO has used several wrapper ids over the years; try each.
    for selector in ("#LawContent", "#lawContents", "#contents", "#content", "article", "main"):
        node = tree.css_first(selector)
        if node is not None:
            return node
    return tree.body or tree.root


def _strip_noise(node: Node) -> None:
    for selector in ("script", "style", "noscript", "template"):
        for el in node.css(selector):
            el.decompose()
    for cls in _NAV_LIKE_CLASSES:
        for el in node.css(f".{cls}"):
            el.decompose()
    for el in node.css("nav"):
        el.decompose()


def _inject_anchor_suffixes(node: Node) -> None:
    """Rewrite ``<h* id="foo">Title</h*>`` to append ``{#foo}`` inside the heading.

    markdownify preserves the text; we append the anchor so chunks can reference
    it directly. Same rule applied to ``<a name="foo">`` siblings.
    """
    # [IB-08] Inject heading id as ' {#anchor}' suffix so the chunker can reference sections; markdownify runs with heading_style=ATX, bullets='-', and script/style/iframe stripped.
    for tag in _HEADING_TAGS:
        for heading in node.css(tag):
            anchor = heading.attributes.get("id")
            if not anchor:
                # Look for a child <a name="...">
                for a in heading.css("a"):
                    name = a.attributes.get("name") or a.attributes.get("id")
                    if name:
                        anchor = name
                        break
            if not anchor:
                continue
            # Append ' {#anchor}' to the heading text
            heading.insert_child(f" {{#{anchor}}}")


def _clean_formula_part(value: str) -> str:
    return " ".join(value.replace("\xa0", " ").split())


def _formula_cell_parts(cell: Node) -> list[str]:
    raw = cell.text(deep=True, separator="\n", strip=True) or ""
    parts = [_clean_formula_part(part) for part in raw.splitlines()]
    parts = [part for part in parts if part]
    if len(parts) > 1:
        parts = [part for part in parts if part != "*"]
    return [part.removeprefix("*").strip() if part.startswith("* ") else part for part in parts]


def _formula_cells(row: Node) -> list[_FormulaCell]:
    cells: list[_FormulaCell] = []
    for index, cell in enumerate(row.css("td,th")):
        parts = _formula_cell_parts(cell)
        if not parts:
            continue
        html = (cell.html or "").lower()
        cells.append(
            _FormulaCell(
                index=index,
                parts=parts,
                has_break="<br" in html,
                has_underline="<u" in html,
            )
        )
    return cells


def _formula_text(parts: list[str]) -> str:
    text = " ".join(parts).replace("×", "x")
    return " ".join(text.split())


def _looks_like_formula_numerator(text: str) -> bool:
    padded = f" {text} "
    return bool(re.search(r"\d\s*%|\s[+\-/]\s|[×*]|\b[xX]\b", padded))


def _formula_from_html_table(table: Node) -> str | None:
    rows = [_formula_cells(row) for row in table.css("tr")]
    rows = [row for row in rows if row]
    if len(rows) == 1 and len(rows[0]) == 1:
        cell = rows[0][0]
        if cell.has_break and cell.has_underline and len(cell.parts) == 2:
            return f"Formula: {_formula_text([cell.parts[0]])} / {_formula_text([cell.parts[1]])}"
        return None

    if len(rows) != 2:
        return None
    numerator = rows[0]
    denominator = rows[1]
    if len(denominator) != 1:
        return None
    denominator_col = denominator[0].index
    numerator_cols = [cell.index for cell in numerator]
    if len(numerator) == 1 and numerator[0].index != denominator_col:
        return None
    if len(numerator) > 1 and denominator_col not in numerator_cols:
        return None

    numerator_text = " ".join(_formula_text(cell.parts) for cell in numerator)
    denominator_text = _formula_text(denominator[0].parts)
    if not numerator_text or not denominator_text:
        return None
    if not _looks_like_formula_numerator(numerator_text):
        return None
    return f"Formula: ({numerator_text}) / {denominator_text}"


def _rewrite_formula_html_tables(node: Node) -> None:
    for table in node.css("table"):
        formula = _formula_from_html_table(table)
        if formula is None:
            continue
        replacement = HTMLParser(f"<p>{html_lib.escape(formula)}</p>").css_first("p")
        if replacement is not None:
            table.replace_with(replacement)


_MD_COLLAPSE = re.compile(r"\n{3,}")
_MD_TRAIL_WS = re.compile(r"[ \t]+\n")
_MD_NUMERIC_RANGE = re.compile(r"(?<=\d)\s+-\s+(?=\d)")
_MD_SPACED_QUOTE = re.compile(r'"\s+([^"\n]*?)\s+"')
_HISTORY_LINK_LINE = re.compile(r'^\[View history reference\]\([^)]+\)\s*$', re.IGNORECASE)
_HISTORY_TOGGLE_LINE = re.compile(
    r'!\[[^\]]*\]\([^)]*(?:"(?:View|Hide) history note"[^)]*)?\)\s*(?:View|Hide) history note',
    re.IGNORECASE,
)
_MARKDOWN_LINK_START = re.compile(r"\[[^\]\n]{0,300}\]\(")


def _is_structural_markdown_line(line: str) -> bool:
    stripped = line.lstrip()
    return (
        not stripped
        or stripped.startswith(("#", ">", "|", "```", "---"))
        or bool(re.match(r"([-*+]|\d+[.)])\s+", stripped))
    )


def _unwrap_prose_lines(md: str) -> str:
    """Join source-wrapped inline fragments inside plain prose blocks.

    ATO legislation pages sometimes split inline spans, quotes, and hyphenated
    amendment ranges across physical HTML source lines. markdownify preserves
    those newlines, which turns inline phrases into broken markdown. Keep
    structural markdown blocks intact and unwrap only plain prose paragraphs.
    """

    # [IE-03] Plain prose blocks are unwrapped after markdownify so source line breaks inside inline spans/quotes don't shatter amendment notes.
    blocks = re.split(r"(\n\s*\n)", md)
    out: list[str] = []
    for block in blocks:
        if not block or block.isspace():
            out.append(block)
            continue
        lines = block.splitlines()
        if len(lines) <= 1 or any(_is_structural_markdown_line(line) for line in lines):
            out.append(block)
            continue
        joined = " ".join(line.strip() for line in lines if line.strip())
        joined = _MD_NUMERIC_RANGE.sub("-", joined)
        joined = _MD_SPACED_QUOTE.sub(r'"\1"', joined)
        out.append(joined)
    return "".join(out)


def _tidy_markdown(md: str) -> str:
    md = _rewrite_internal_links(md)
    md = _strip_history_noise(md)
    md = _unwrap_prose_lines(md)
    md = _MD_TRAIL_WS.sub("\n", md)
    md = _MD_COLLAPSE.sub("\n\n", md)
    return md.strip() + "\n"


def _doc_id_from_ato_link(target: str) -> str | None:
    target = target.strip()
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    if " " in target:
        target = target.split(" ", 1)[0]
    if not ("/law/view/document" in target or "ato.gov.au/law/view/document" in target):
        return None
    parsed = urlparse(target)
    query = parse_qs(parsed.query)
    raw = (query.get("docid") or query.get("DocID") or query.get("LocID") or query.get("locid"))
    if not raw:
        return None
    doc_id = unquote(raw[0]).strip().strip('"')
    return doc_id or None


def _rewrite_internal_links(md: str) -> str:
    """Replace noisy ATO markdown URLs with compact doc_id references."""

    out: list[str] = []
    i = 0
    while i < len(md):
        start = md.find("[", i)
        if start < 0:
            out.append(md[i:])
            break
        out.append(md[i:start])
        label_end = md.find("](", start)
        if label_end < 0:
            out.append(md[start:])
            break
        label = md[start + 1:label_end]
        target_start = label_end + 2
        depth = 1
        j = target_start
        while j < len(md):
            if md[j] == "(":
                depth += 1
            elif md[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        if j >= len(md):
            out.append(md[start:])
            break
        target = md[target_start:j]
        doc_id = _doc_id_from_ato_link(target)
        if doc_id is None:
            out.append(md[start:j + 1])
        elif label.strip().lower() == "view history reference":
            out.append("")
        else:
            clean_label = " ".join(label.split()) or doc_id
            if clean_label == doc_id or clean_label.endswith(f"docid={doc_id}"):
                out.append(f"[doc_id: {doc_id}]")
            else:
                out.append(f"{clean_label} [doc_id: {doc_id}]")
        i = j + 1
    return "".join(out)


def _is_history_boundary(line: str) -> bool:
    stripped = line.strip()
    if not stripped:
        return False
    return (
        stripped.startswith("#")
        or stripped.startswith("***")
        or stripped.startswith("**")
        or bool(_MARKDOWN_LINK_START.match(stripped))
    )


def _strip_history_noise(md: str) -> str:
    lines = md.splitlines()
    out: list[str] = []
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        if _HISTORY_LINK_LINE.match(stripped) or _HISTORY_TOGGLE_LINE.search(stripped):
            i += 1
            continue
        if stripped == "History":
            i += 1
            while i < len(lines) and not _is_history_boundary(lines[i]):
                i += 1
            continue
        out.append(lines[i])
        i += 1
    return "\n".join(out)


def _collect_anchors(node: Node) -> list[tuple[str, str]]:
    out: list[tuple[str, str]] = []
    for heading in node.css(",".join(_HEADING_TAGS)):
        anchor = heading.attributes.get("id")
        if not anchor:
            for a in heading.css("a"):
                name = a.attributes.get("name") or a.attributes.get("id")
                if name:
                    anchor = name
                    break
        if anchor:
            text = heading.text(deep=True, separator=" ", strip=True)
            out.append((text, anchor))
    return out


def heading_outline(headings: Iterable[str]) -> str:
    return " › ".join(h for h in headings if h)


# ---------------------------------------------------------------------------
# Currency / supersession extraction (W2.2)
#
# ATO publishes withdrawal and supersession status across several surfaces on a
# document page:
#   1. A status panel (``div.alert.alert-warning``) at the top of the doc:
#        "This document has been Withdrawn." [+ link to "Withdrawal notice"]
#        "This Ruling, which applies from 1 July 2022, replaces TR 2021/3"
#   2. Body prose on the doc itself or its Notice-of-Withdrawal sibling page:
#        "TR 2022/1 is withdrawn with effect from 31 October 2025."
#        "Draft Taxation Ruling TR 2007/D10 was withdrawn with effect from
#         7 December 2016."
#        "This Ruling replaces Taxation Ruling TR 2021/3"
#   3. A history/timeline panel near the bottom of the page with a
#        ``<td class="date-right2">7 December 2016</td>`` row whose neighbour
#        cell links to "Withdrawal" / "Updated withdrawal".
#
# These surfaces are noisy: dates appear in multiple formats ("31 October 2025"
# vs. "31/10/2025" vs. "2025-10-31"); the citation in "replaces TR 2021/3" can
# embed a series prefix or appear as a hyperlink with a slightly different
# label. The extractor therefore captures whichever marker fires first per
# field — never blocking the other fields if one is absent.

_RULING_SERIES_FOR_CURRENCY = (
    "SMSFRB|SMSFR|SMSFD|GSTR|GSTD|FBTR|WETR|WETD|"
    "LCR|SGR|FTR|PCG|LCG|PRR|CLR|COG|TXD|TPA|"
    "FBT|GII|CR|PR|TR|TD|MT|TA|LI|LG|WT|IT"
)
_CITATION_PATTERN = (
    rf"(?:{_RULING_SERIES_FOR_CURRENCY}|ATO\s+ID|PS\s+LA|SMSFRB)"
    r"\s+\d{1,4}/D?\d+[A-Z0-9]*"
)
_DATE_PROSE_PATTERN = (
    r"\d{1,2}\s+(?:January|February|March|April|May|June|July|August|"
    r"September|October|November|December)\s+\d{4}|"
    r"\d{1,2}/\d{1,2}/\d{4}|\d{4}-\d{2}-\d{2}"
)
_WITHDRAWN_DATE_PREFIX_PATTERN = (
    r"\b(?:was|is|were|are|been|being|has\s+been|have\s+been)?\s*withdrawn"
    r"(?:\s+(?:with\s+effect)?\s*(?:from|on|as\s+of))?\s+"
)
# Matches "withdrawn with effect from 7 December 2016", "withdrawn from
# 1 July 2022", "withdrawn on 6 October 2022", "is withdrawn with effect from
# 31 October 2025." The verb "withdrawn" comes first, then optionally
# "with effect", "on", "from", "as of"; the date follows.
_RE_WITHDRAWN_PROSE = re.compile(
    _WITHDRAWN_DATE_PREFIX_PATTERN + rf"(?P<date>{_DATE_PROSE_PATTERN})",
    re.IGNORECASE,
)
_RE_WITHDRAWN_BY_PROSE = re.compile(
    _WITHDRAWN_DATE_PREFIX_PATTERN
    + rf"(?P<date>{_DATE_PROSE_PATTERN})"
    + r"\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*"
    + r"(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+"
    + rf"(?P<cite>{_CITATION_PATTERN})",
    re.IGNORECASE,
)
# Sentences containing any of these verbs describe the relationship between
# two rulings (this one and a predecessor/successor); a "withdrawn ... date"
# clause inside such a sentence almost always belongs to the OTHER ruling.
# We use a "this Ruling/Determination/..." anchor as the override signal —
# when the subject of the sentence is THIS document, the withdrawal date
# belongs to it even if the sentence also mentions replacement.
_RE_REPLACEMENT_VERB = re.compile(
    r"\b(replaces|replaced\s+by|supersed(?:e|es|ed|ing)|in\s+lieu\s+of)\b",
    re.IGNORECASE,
)
_RE_SELF_ANCHOR = re.compile(
    r"\bthis\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\b",
    re.IGNORECASE,
)
# Sentence boundary split — periods, semicolons, and newlines all break
# clauses cleanly enough for this heuristic. We deliberately keep it crude
# (no NLP) — over-splitting is harmless because each fragment is independently
# scanned.
_RE_SENTENCE_SPLIT = re.compile(r"[.;\n]+")
# Matches "replaces Taxation Ruling TR 2021/3", "replaces TR 2021/3",
# "Replaced by TR 98/17", "superseded by TR 94/13".
_RE_REPLACES_PROSE = re.compile(
    rf"\b(?:this\s+(?:Ruling|Determination|Guideline|Practice\s+Statement)\s+)?"
    rf"replaces\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|Practical|GST)?\s*"
    rf"(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?\s+"
    rf"(?P<cite>{_CITATION_PATTERN})",
    re.IGNORECASE,
)
_RE_SUPERSEDED_BY_PROSE = re.compile(
    rf"\b(?:replaced|superseded)\s+by\b(?:\s+(?:draft\s+)?(?:Taxation|Class|Product|"
    rf"Practical|GST)?\s*(?:Ruling|Determination|Guideline|Practice\s+Statement)?)?"
    rf"\s+(?P<cite>{_CITATION_PATTERN})",
    re.IGNORECASE,
)
_MONTHS = {
    name.lower(): idx
    for idx, name in enumerate(
        [
            "January", "February", "March", "April", "May", "June",
            "July", "August", "September", "October", "November", "December",
        ],
        1,
    )
}


def _normalise_date(raw: str | None) -> str | None:
    """Normalise an ATO-formatted date to ISO ``yyyy-mm-dd``.

    Handles three observed formats:
      - ``"31 October 2025"`` (the usual prose form).
      - ``"31/10/2025"`` (DD/MM/YYYY — Australian convention).
      - ``"2025-10-31"`` (already ISO).
    Returns ``None`` for unparseable input.
    """
    if raw is None:
        return None
    s = " ".join(raw.split())
    m = re.fullmatch(r"(\d{1,2})\s+([A-Za-z]+)\s+(\d{4})", s)
    if m:
        day, month_name, year = m.group(1), m.group(2).lower(), m.group(3)
        month = _MONTHS.get(month_name)
        if month is None:
            return None
        return f"{int(year):04d}-{month:02d}-{int(day):02d}"
    m = re.fullmatch(r"(\d{1,2})/(\d{1,2})/(\d{4})", s)
    if m:
        day, month, year = m.group(1), m.group(2), m.group(3)
        return f"{int(year):04d}-{int(month):02d}-{int(day):02d}"
    m = re.fullmatch(r"(\d{4})-(\d{2})-(\d{2})", s)
    if m:
        return s
    return None


def _normalise_citation(raw: str | None) -> str | None:
    """Collapse internal whitespace and trim a citation token."""
    if raw is None:
        return None
    return " ".join(raw.split()) or None


def _withdrawal_fragment_is_self(fragment: str, withdrawn_start: int) -> bool:
    if _RE_REPLACEMENT_VERB.search(fragment) is None:
        return True
    anchor = _RE_SELF_ANCHOR.search(fragment)
    if anchor is None:
        return False
    # The window between the anchor's end and the withdrawn keyword must be
    # free of replacement verbs; otherwise the anchor's subject is that other
    # verb rather than "withdrawn".
    between = fragment[anchor.end():withdrawn_start]
    return _RE_REPLACEMENT_VERB.search(between) is None


def _extract_self_withdrawn_date(text: str) -> str | None:
    """Pick a withdrawal date that applies to THIS document, not a referenced one.

    The naive `_RE_WITHDRAWN_PROSE.search(text)` approach trips on sentences
    like "This Ruling replaces TR 2021/3, which is withdrawn from 1 July
    2022" — the date there belongs to TR 2021/3, not to the current ruling.

    Strategy: split into sentence-ish fragments. For each fragment that
    matches `_RE_WITHDRAWN_PROSE`:
      - If the fragment contains NO replacement verb, it's a clean
        self-withdrawal — keep the date.
      - If the fragment DOES contain a replacement verb, only keep the date
        when the "this Ruling/Determination/..." anchor sits immediately
        before the `withdrawn` keyword (within a short character window).
        This covers "This Ruling is withdrawn from ..." (anchor → withdrawn,
        no other verbs between) but NOT "This Ruling replaces TR X, which
        is withdrawn from ..." (the anchor's subject is `replaces`, not
        `withdrawn`).
    Returns the first surviving date as ISO ``yyyy-mm-dd``.
    """
    for fragment in _RE_SENTENCE_SPLIT.split(text):
        m = _RE_WITHDRAWN_PROSE.search(fragment)
        if m is None:
            continue
        if _withdrawal_fragment_is_self(fragment, m.start()):
            return _normalise_date(m.group("date"))
    return None


def _extract_self_withdrawn_by(text: str) -> str | None:
    """Return the successor citation in "withdrawn ... by TR X" self clauses."""
    for fragment in _RE_SENTENCE_SPLIT.split(text):
        m = _RE_WITHDRAWN_BY_PROSE.search(fragment)
        if m is None:
            continue
        if _withdrawal_fragment_is_self(fragment, m.start()):
            return _normalise_citation(m.group("cite"))
    return None


def _container_text_for_currency(html: str) -> str:
    """Combine the status-alert panel text and body prose into one search
    string so a single regex pass can pick up either surface.

    We intentionally include the alert panel verbatim (including links) — the
    panel text on a still-current doc that has been ADDENDUM'd reads "This
    document has been Withdrawn" only on actually-withdrawn rulings, so the
    `_RE_WITHDRAWN_PROSE` pattern (which requires a date alongside) won't fire
    spuriously.
    """
    tree = HTMLParser(html)
    chunks: list[str] = []
    for el in tree.css("div.alert"):
        chunks.append(el.text(deep=True, separator=" ", strip=True) or "")
    body = tree.css_first("#LawBody") or tree.css_first("#LawContent") or tree.body
    if body is not None:
        chunks.append(body.text(deep=True, separator=" ", strip=True) or "")
    return " \n ".join(c for c in chunks if c)


def _date_from_history_table(html: str) -> str | None:
    """Pull the most recent withdrawal date from the timeline/history table.

    The history panel renders as ``<td class="date-right2">7 December
    2016</td>`` followed by a sibling cell with link text "Withdrawal" /
    "Updated withdrawal". When present, this is the most authoritative
    date — but it's optional (older docs lack the table).
    """
    tree = HTMLParser(html)
    timeline = tree.css_first("a[name='LawTimeLine']")
    if timeline is None:
        return None
    # Walk up to the enclosing panel that holds the timeline rows. The anchor
    # nests inside ``panel-heading``; the rows live in a sibling
    # ``panel-body``. Walking to the first ancestor whose class contains the
    # whole word ``panel`` (and not ``panel-heading`` / ``panel-body``)
    # picks the panel root deterministically.
    panel = timeline
    for _ in range(8):
        if panel.parent is None:
            break
        panel = panel.parent
        classes = (panel.attributes.get("class") or "").split()
        if (panel.tag or "").lower() == "table":
            break
        if "panel" in classes:
            break
    rows = panel.css("tr")
    latest: str | None = None
    for row in rows:
        cells = row.css("td")
        if len(cells) < 2:
            continue
        date_cell = None
        label_cell = None
        for cell in cells:
            cls = (cell.attributes.get("class") or "").lower()
            text = cell.text(deep=True, separator=" ", strip=True) or ""
            if "date" in cls and date_cell is None:
                date_cell = text
            elif date_cell is not None and label_cell is None:
                label_cell = text.lower()
        if date_cell is None:
            continue
        if label_cell is None:
            label_cell = (cells[-1].text(deep=True, separator=" ", strip=True) or "").lower()
        if "withdraw" not in label_cell:
            continue
        iso = _normalise_date(date_cell)
        if iso is not None:
            # Latest entry wins (timeline is chronological, last row is most
            # recent).
            latest = iso
    return latest


def extract_currency(html: str) -> CurrencyInfo:
    """Best-effort currency / supersession extraction from raw page HTML.

    Returns ``CurrencyInfo()`` (all None) on empty input or when no markers
    are present. Each field is filled independently — see ``CurrencyInfo``.
    """
    if not html or not html.strip():
        return CurrencyInfo()
    text = _container_text_for_currency(html)
    if not text:
        return CurrencyInfo()

    withdrawn_date: str | None = None
    superseded_by: str | None = None
    replaces: str | None = None

    withdrawn_date = _extract_self_withdrawn_date(text)

    if withdrawn_date is None:
        # Fall back to the timeline table if the prose form didn't fire.
        withdrawn_date = _date_from_history_table(html)

    superseded_by = _extract_self_withdrawn_by(text)

    m = _RE_SUPERSEDED_BY_PROSE.search(text)
    if m and superseded_by is None:
        superseded_by = _normalise_citation(m.group("cite"))

    m = _RE_REPLACES_PROSE.search(text)
    if m:
        replaces = _normalise_citation(m.group("cite"))

    return CurrencyInfo(
        withdrawn_date=withdrawn_date,
        superseded_by=superseded_by,
        replaces=replaces,
    )
