"""HTML -> cleaned agent-facing content for ATO documents.

Strategy:
1. Parse with selectolax (lexbor) for speed.
2. Find the content container (``lawContents`` div, falling back to ``<article>``
   or ``<main>`` / ``<body>`` with nav stripped).
3. Strip ubiquitous ATO history/navigation noise and unsafe/presentational
   attributes.
4. Rewrite internal ATO document links to ``data-doc-id`` attributes and
   rewrite retained images to compact asset references.
5. Emit cleaned HTML plus the plain semantic text used by metadata checks.

Output also includes source headings, anchors, and image asset metadata used by
the build pipeline.
"""
from __future__ import annotations

import base64
import hashlib
import html as html_lib
import mimetypes
import re
from dataclasses import dataclass, field
from pathlib import Path

from urllib.parse import parse_qs, quote, unquote, urlparse

from bs4 import BeautifulSoup
from selectolax.parser import HTMLParser, Node

from . import chunk as chunk_mod

_HEADING_TAGS = ("h1", "h2", "h3", "h4", "h5", "h6")
_NAV_LIKE_CLASSES = ("minimenu", "minimenu-bar")
_NAV_LIKE_IDS = {"LawMiniMenuHeader"}
_DROP_ATTRS = {
    "style", "width", "height", "align", "valign", "bgcolor",
    "name",
    "data-icon", "cite",
}
_DROP_PREFIXES = ("on",)


@dataclass(frozen=True)
class ExtractedAsset:
    asset_ref: str
    source_path: str
    relative_path: str
    media_type: str | None
    alt: str | None
    title: str | None
    sha256: str
    size: int
    data_b64: str


@dataclass
class ExtractedDoc:
    html: str
    text: str
    title: str | None
    html_title: str | None = None  # raw <title> (browser tab text)
    headings: list[str] = field(default_factory=list)
    heading_levels: list[int] = field(default_factory=list)  # parallel to headings (1-6)
    anchors: list[tuple[str, str]] = field(default_factory=list)  # (heading_text, anchor_id)
    assets: list[ExtractedAsset] = field(default_factory=list)
    # Parliamentary EM / regulation Explanatory Statement front-matter signals.
    # See _collect_em_front_matter for the structural fingerprint these capture.
    front_matter_refs: list[str] = field(default_factory=list)
    front_matter_phrase: str | None = None


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


def extract(
    html: str,
    *,
    doc_id: str | None = None,
    source_path: Path | None = None,
) -> ExtractedDoc:
    if not html or not html.strip():
        return ExtractedDoc(html="", text="", title=None, html_title=None)

    tree = HTMLParser(html)
    html_title = _first_text(tree, "title")

    container = _pick_container(tree)
    if container is None:
        return ExtractedDoc(html="", text="", title=None, html_title=html_title)

    _strip_noise(container)

    # Capture EM / Explanatory Statement front-matter BEFORE link rewriting.
    # The front-matter wrapper (#Lawfront) is a stable structural marker on
    # parliamentary EM and regulation ES pages.
    fm_refs, fm_phrase = _collect_em_front_matter(container)

    # Capture "title headings" — consecutive leading headings before any body
    # content. On ATO rulings that gives h1=doc_type, h2=code, h3=subject.
    lead_headings = _leading_headings(container)
    title = _compose_title(lead_headings) or html_title

    _normalise_named_anchors(container)
    _strip_history_ui_controls(container)
    container = _rewrite_links_html(container)
    assets = _rewrite_images_html(container, doc_id=doc_id, source_path=source_path)
    _strip_attributes(container)
    anchors = _collect_anchors(container)

    headings = []
    heading_levels = []
    for h in container.css(",".join(_HEADING_TAGS)):
        text = h.text(deep=True, separator=" ", strip=True) or ""
        headings.append(text)
        try:
            heading_levels.append(int((h.tag or "h0")[1:]))
        except ValueError:
            heading_levels.append(0)
    html_fragment = container.html or ""
    text = chunk_mod.html_to_text(html_fragment)
    return ExtractedDoc(
        html=html_fragment.strip(),
        text=text,
        title=title,
        html_title=html_title,
        headings=headings,
        heading_levels=heading_levels,
        anchors=anchors,
        assets=assets,
        front_matter_refs=fm_refs,
        front_matter_phrase=fm_phrase,
    )


def _collect_em_front_matter(container: Node) -> tuple[list[str], str | None]:
    """Capture parliamentary EM / regulation ES front-matter signals.

    Parliamentary Explanatory Memoranda (NEM) and regulation Explanatory
    Statements (EXN/EXM/ESI/ESG/...) tuck the doc-type label and the
    bill/regulation citation into a ``<div id="Lawfront">`` block instead of
    a leading h1. Without those signals downstream rules can't compose a
    title beyond the docid form.

    Returns ``(refs, phrase)``:
    - ``refs``: text of every ``<strong>`` inside ``<div class="ref">``
      blocks under the front-matter, in document order.
    - ``phrase``: the first ``<p><strong>...</strong></p>`` whose text starts
      with "Explanatory " (Memorandum / Statement / future variants).
    """
    front = container.css_first("#Lawfront")
    if front is None:
        return [], None

    refs: list[str] = []
    phrase: str | None = None

    for child in front.iter(include_text=False):
        tag = (child.tag or "").lower()
        if tag == "div":
            cls = (child.attributes.get("class") or "").split()
            if "ref" in cls:
                strong = child.css_first("strong")
                if strong is not None:
                    ref_text = _text_norm(strong.text(deep=True, separator=" ", strip=True))
                    if ref_text:
                        refs.append(ref_text)
            continue
        if tag == "p" and phrase is None:
            strong = child.css_first("strong")
            if strong is not None:
                ptext = _text_norm(strong.text(deep=True, separator=" ", strip=True))
                if ptext.lower().startswith("explanatory "):
                    phrase = ptext

    return refs, phrase


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
    for selector in ("#LawContent", "#lawContents", "#LawContents", "#contents"):
        node = tree.css_first(selector)
        if node is not None:
            return node
    return tree.body or tree.root


def _strip_noise(node: Node) -> None:
    for selector in ("script", "style", "noscript", "template"):
        for el in node.css(selector):
            el.decompose()
    for ident in _NAV_LIKE_IDS:
        for el in node.css(f"#{ident}"):
            el.decompose()
    for cls in _NAV_LIKE_CLASSES:
        for el in node.css(f".{cls}"):
            el.decompose()
    for el in node.css("nav"):
        el.decompose()


def _text_norm(value: str | None) -> str:
    return " ".join((value or "").split()).strip()


def _text_lc(value: str | None) -> str:
    return _text_norm(value).lower()


def _normalise_named_anchors(node: Node) -> None:
    for el in node.css("a"):
        name = el.attributes.get("name")
        if name and not el.attributes.get("id"):
            el.attrs["id"] = name
        _drop_attr(el, "name")


# UI control labels that wrap the show/hide JavaScript toggles on ATO
# history-note panels. The actual history-note body (the "History" heading
# and the inserted-by-Act-N text) stays — only the toggle-icon images and
# their literal label text get pruned. Match is case-insensitive on the
# image's title/alt and on bare text-node content.
_HISTORY_UI_LABELS = {
    "view history note",
    "hide history note",
    "view history reference",
    "hide history reference",
}


def _strip_history_ui_controls(node: Node) -> None:
    """Decompose the show/hide-history-note image toggles + their label text.

    Leaves the actual history-note body (which lives inside the same panel
    but contains substantive content like 'Pt 3-1 inserted by No 46 of
    1998.') untouched. Without this, every history panel emits noise like
    `[asset:ato-image://X/0] View history note [asset:...] Hide history note`
    that competes with the real history-note content for the agent's
    attention budget.
    """
    for el in list(node.traverse(include_text=False)):
        tag = (el.tag or "").lower()
        if tag != "img":
            continue
        for attr in ("title", "alt"):
            value = (el.attributes.get(attr) or "").strip().lower()
            if value in _HISTORY_UI_LABELS:
                el.decompose()
                break
    for text_node in list(node.traverse(include_text=True)):
        if (text_node.tag or "").lower() != "-text":
            continue
        raw = (text_node.text() or "").strip().lower()
        if raw in _HISTORY_UI_LABELS:
            text_node.decompose()


def _rewrite_links_html(node: Node) -> Node:
    original_id = node.attributes.get("id")
    soup = BeautifulSoup(node.html or "", "html.parser")
    for el in soup.find_all("a"):
        href = el.get("href")
        if not href:
            continue
        resolved = _doc_id_from_ato_link(href)
        if resolved:
            doc_id, pit, view = resolved
            el["data-doc-id"] = doc_id
            if pit:
                el["data-pit"] = pit
            if view:
                el["data-view"] = view
            del el["href"]
            continue
        clean = _safe_href(href)
        if clean:
            el["href"] = clean
        else:
            del el["href"]
    tree = HTMLParser(str(soup))
    if original_id:
        replacement = tree.css_first(f"#{original_id}")
        if replacement is not None:
            return replacement
    return tree.body or tree.root


def _safe_href(href: str) -> str | None:
    value = html_lib.unescape(href).strip()
    if not value:
        return None
    if re.match(r"(?is)^\s*(?:javascript|data):", value):
        return None
    return value


def _asset_ref(doc_id: str, ordinal: int) -> str:
    return f"ato-image://{quote(doc_id, safe='')}/{ordinal}"


def _asset_relative_path(data: bytes, source: str) -> tuple[str, str]:
    sha = hashlib.sha256(data).hexdigest()
    suffix = Path(urlparse(source).path).suffix.lower()
    if not suffix or len(suffix) > 10:
        suffix = mimetypes.guess_extension(mimetypes.guess_type(source)[0] or "") or ".bin"
    return f"assets/{sha[:2]}/{sha}{suffix}", sha


def _asset_path_for(source_path: Path | None, src: str) -> Path | None:
    if source_path is None or not src:
        return None
    parsed = urlparse(src)
    if parsed.scheme or src.startswith("/"):
        return None
    return (source_path.parent / src).resolve()


def _image_label(img: Node) -> tuple[str | None, str | None, str]:
    alt = _text_norm(img.attributes.get("alt")) or None
    title = _text_norm(img.attributes.get("title")) or None
    label = alt or title or ""
    return alt, title, label


def _rewrite_images_html(
    node: Node,
    *,
    doc_id: str | None,
    source_path: Path | None,
) -> list[ExtractedAsset]:
    assets: list[ExtractedAsset] = []
    image_ord = 0
    for img in list(node.css("img")):
        alt, title, label = _image_label(img)
        if _text_lc(label) == "exclamation":
            img.decompose()
            continue

        src = _text_norm(img.attributes.get("src"))
        data: bytes | None = None
        asset_path = _asset_path_for(source_path, src)
        if asset_path is not None and asset_path.exists():
            data = asset_path.read_bytes()

        asset_ref: str | None = None
        relative_path: str | None = None
        media_type = mimetypes.guess_type(src)[0]
        if data is not None and doc_id:
            asset_ref = _asset_ref(doc_id, image_ord)
            relative_path, sha = _asset_relative_path(data, src)
            assets.append(
                ExtractedAsset(
                    asset_ref=asset_ref,
                    source_path=src,
                    relative_path=relative_path,
                    media_type=media_type,
                    alt=alt,
                    title=title,
                    sha256=sha,
                    size=len(data),
                    data_b64=base64.b64encode(data).decode("ascii"),
                )
            )
            image_ord += 1

        if asset_ref is None and not label:
            img.decompose()
            continue

        attrs = []
        if asset_ref:
            attrs.append(f'data-asset-ref="{html_lib.escape(asset_ref, quote=True)}"')
        if media_type:
            attrs.append(f'data-media-type="{html_lib.escape(media_type, quote=True)}"')
        text = f"[image: {label}]" if label else "[image]"
        replacement = HTMLParser(
            f"<span {' '.join(attrs)}>{html_lib.escape(text)}</span>"
        ).css_first("span")
        if replacement is not None:
            img.replace_with(replacement)
        else:
            img.decompose()
    return assets


def _strip_attributes(node: Node) -> None:
    for el in node.traverse():
        for attr in list(el.attributes):
            attr_lc = attr.lower()
            if attr_lc in _DROP_ATTRS or attr_lc.startswith(_DROP_PREFIXES):
                _drop_attr(el, attr)


def _drop_attr(node: Node, attr: str) -> None:
    try:
        del node.attrs[attr]
    except KeyError:
        pass


_ATO_DOC_PATH_HINTS = (
    "/law/view/document",
    "/law/view/view.htm",
    "/law/view.htm",
    "/atolaw/view.htm",
    "/view.htm",
)


def _docid_from_query_string(query: str) -> str | None:
    # Case-insensitive lookup over docid / locid in any quote casing the ATO
    # has shipped: docid, DocID, Docid, LocID, locid, etc.
    for key, values in parse_qs(query).items():
        if key.lower() in ("docid", "locid") and values:
            return values[0]
    return None


def _doc_id_from_ato_link(target: str) -> tuple[str, str | None, str | None] | None:
    """Parse an ATO link href into (doc_id, pit_timestamp, view_db).

    Returns ``None`` for non-doc URLs.
    - ``pit_timestamp``: the literal ``PiT=YYYYMMDDHHMMSS`` query parameter
      when present, otherwise ``None``. Identifies a point-in-time snapshot
      of the same doc.
    - ``view_db``: the ``db=`` query parameter normalised to upper-case when
      it matches a known view (currently ``HISTFT`` — the amendment-trail
      rendering of the same doc, with EM / Second Reading Speech links).
      Preserves links that target an alternative view of the doc so the
      ``[doc:X view=HISTFT]`` marker in chunk text tells the agent the
      cross-reference points at the amendment history surface, not the
      live text.
    """
    target = target.strip()
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    if " " in target:
        target = target.split(" ", 1)[0]
    try:
        parsed = urlparse(target)
    except ValueError:
        # urlparse rejects unbalanced brackets ("Invalid IPv6 URL") and other
        # malformed inputs. Treat unparseable hrefs as non-ATO links.
        return None
    host = (parsed.hostname or "").lower()
    path_lower = parsed.path.lower()
    is_ato_host = host.endswith("ato.gov.au")
    has_ato_path = any(hint in path_lower for hint in _ATO_DOC_PATH_HINTS)
    if not (is_ato_host or has_ato_path):
        return None
    raw = _docid_from_query_string(parsed.query)
    pit = _pit_from_query_string(parsed.query)
    view = _view_from_query_string(parsed.query)
    if raw is None and parsed.fragment:
        # SPA-style URLs hide the doc id in the fragment, e.g.
        # `/law/#Law/table-of-contents?docid=X` or
        # `/single-page-applications/legaldatabase/#Law/table-of-contents?docid=X`.
        if "?" in parsed.fragment:
            _, _, frag_query = parsed.fragment.partition("?")
            raw = _docid_from_query_string(frag_query)
            if pit is None:
                pit = _pit_from_query_string(frag_query)
            if view is None:
                view = _view_from_query_string(frag_query)
    if not raw:
        return None
    # SPA category links carry a trailing `?` flag (e.g. `docid=tpa?`,
    # `locid=rtf/sca?`) and point to the category browser rather than to a
    # specific document — drop those entirely instead of emitting a bogus id.
    if raw.endswith("?"):
        return None
    doc_id = unquote(raw).strip().strip('"')
    # Real ATO doc ids always contain a `/` (e.g. TXR/TR.../NAT/ATO).
    if not doc_id or "/" not in doc_id:
        return None
    return doc_id, pit, view


def _pit_from_query_string(query: str) -> str | None:
    """Return the PiT timestamp from a query string, case-insensitive."""
    for key, values in parse_qs(query).items():
        if key.lower() == "pit" and values:
            v = values[0].strip()
            if v:
                return v
    return None


# ATO uses ``db=HISTFT`` to render a doc's amendment-trail view (assent
# dates, EM / Second Reading Speech links). It's not a historical snapshot
# (that's PiT) — it's an alternative surface of the same live doc. We
# preserve the qualifier in the [doc:X view=HISTFT] inline marker so the
# agent reading the cross-reference knows it pointed at the amendment
# history, not the live text.
_KNOWN_DOC_VIEWS = {"HISTFT"}


def _view_from_query_string(query: str) -> str | None:
    """Return a normalised view qualifier from ``db=`` if known."""
    for key, values in parse_qs(query).items():
        if key.lower() == "db" and values:
            v = values[0].strip().upper()
            if v in _KNOWN_DOC_VIEWS:
                return v
    return None


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


def _alert_text_for_currency(html: str) -> str:
    """Concatenate the status-alert panel text in document order.

    The alert panel on a still-current doc that has been ADDENDUM'd reads
    "This document has been Withdrawn" only on actually-withdrawn rulings, so
    the ``_RE_WITHDRAWN_PROSE`` pattern (which requires a date alongside)
    won't fire spuriously.
    """
    tree = HTMLParser(html)
    chunks: list[str] = []
    for el in tree.css("div.alert"):
        chunks.append(el.text(deep=True, separator=" ", strip=True) or "")
    return " \n ".join(c for c in chunks if c)


def _body_text_for_currency(html: str) -> str:
    """Concatenate the LawBody / LawContent / body prose for currency regexes."""
    tree = HTMLParser(html)
    body = tree.css_first("#LawBody") or tree.css_first("#LawContent") or tree.body
    if body is None:
        return ""
    return body.text(deep=True, separator=" ", strip=True) or ""


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


def _scan_currency_text(text: str) -> tuple[str | None, str | None, str | None]:
    """Return ``(withdrawn_date, superseded_by, replaces)`` for one text blob.

    Each field is None when the corresponding regex did not match in this
    blob. Used independently against alert-panel text and body-prose text so
    callers can attribute hits to the surface they came from.
    """
    if not text:
        return None, None, None
    withdrawn_date = _extract_self_withdrawn_date(text)
    superseded_by = _extract_self_withdrawn_by(text)
    if superseded_by is None:
        m = _RE_SUPERSEDED_BY_PROSE.search(text)
        if m is not None:
            superseded_by = _normalise_citation(m.group("cite"))
    replaces: str | None = None
    m = _RE_REPLACES_PROSE.search(text)
    if m is not None:
        replaces = _normalise_citation(m.group("cite"))
    return withdrawn_date, superseded_by, replaces


def _has_withdrawn_title_suffix(html: str) -> bool:
    """Detect a literal ``(Withdrawn)`` suffix in a leading h1/h2/h3.

    Many ATO IDs and PSLAs carry their withdrawn status only in the heading
    text (e.g. ``<h2>ATO ID 2001/746 (Withdrawn)</h2>``) with no alert panel,
    timeline row, or prose date. Without this signal those documents would
    keep ``withdrawn_date IS NULL`` and leak into default searches.
    """
    tree = HTMLParser(html)
    for el in tree.css("h1, h2, h3"):
        text = el.text(deep=True, separator=" ", strip=True) or ""
        if "(withdrawn)" in text.lower():
            return True
    return False


# Sentinel used when the only withdrawal signal is a ``(Withdrawn)`` suffix
# in a heading. Real withdrawal dates are ISO ``yyyy-mm-dd``; this value is
# distinguishable so the runtime markdown formatter can render it as
# "date unknown" while still satisfying the schema's TEXT type and the
# `withdrawn_date IS NOT NULL` filter that drives current-only search.
_TITLE_SUFFIX_WITHDRAWN_SENTINEL = "0001-01-01"


def extract_currency(html: str) -> CurrencyInfo:
    """Best-effort currency / supersession extraction from raw page HTML.

    Returns ``CurrencyInfo()`` (all None) on empty input or when no markers
    are present. Each field is filled independently — see ``CurrencyInfo``.
    """
    if not html or not html.strip():
        return CurrencyInfo()

    alert_text = _alert_text_for_currency(html)
    body_text = _body_text_for_currency(html)

    a_withdrawn, a_super, a_replaces = _scan_currency_text(alert_text)
    p_withdrawn, p_super, p_replaces = _scan_currency_text(body_text)

    paths: set[str] = set()

    withdrawn_date = a_withdrawn
    if withdrawn_date is not None:
        paths.add("alert")
    if withdrawn_date is None and p_withdrawn is not None:
        withdrawn_date = p_withdrawn
        paths.add("prose")
    if withdrawn_date is None:
        timeline_date = _date_from_history_table(html)
        if timeline_date is not None:
            withdrawn_date = timeline_date
            paths.add("timeline")
    if withdrawn_date is None and _has_withdrawn_title_suffix(html):
        withdrawn_date = _TITLE_SUFFIX_WITHDRAWN_SENTINEL
        paths.add("title_suffix")

    superseded_by = a_super
    if superseded_by is not None:
        paths.add("alert")
    if superseded_by is None and p_super is not None:
        superseded_by = p_super
        paths.add("prose")

    replaces = a_replaces
    if replaces is not None:
        paths.add("alert")
    if replaces is None and p_replaces is not None:
        replaces = p_replaces
        paths.add("prose")

    return CurrencyInfo(
        withdrawn_date=withdrawn_date,
        superseded_by=superseded_by,
        replaces=replaces,
    )
