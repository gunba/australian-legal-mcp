"""Template-based metadata classifier.

Empirically, ATO pages fall into a small number of structural templates
(~10 by volume). Rather than running a dozen regexes against free text
hoping one matches, we *classify* each page into a template using its
heading-shape signature, then apply the template's positional extractor.

This is more principled than regex-across-text because the ATO's
publishing stack is consistent within each doc type. A Taxation Ruling
always has ``<h1>Taxation Ruling</h1><h2>TR 2024/3</h2><h3>subtitle</h3>``;
a Practical Compliance Guideline always has
``<h1>Practical Compliance Guideline</h1><h2>PCG 2025/D6</h2><h3>topic</h3>``;
a Decision Impact Statement always has
``<h1>Decision impact statement</h1><h2>Smith v Commissioner</h2>``.
Extraction becomes a positional read rather than a pattern match.

Templates identified:

- ``T_OFFICIAL_PUB``   Rulings / PCG / TA / PS LA / ATO ID / SMSFRB.
                      h1 = type phrase, h2 = citation.
- ``T_CASE_H1``        Court case with party-v-party or Re-X in h1.
- ``T_CASE_H2``        Court case with name in h2 (rarer layout).
- ``T_DIS``            Decision Impact Statement — h1 = DIS phrase,
                      h2 = case name.
- ``T_ACT``            Act / Regulation — h1 = "<Name> Act <Year>".
- ``T_EM``             Explanatory Memorandum / Bill — shape varies;
                      best we do is pull year + bill phrase out.
- ``T_EPA``            Edited private advice — no headings, no citable
                      short form. Leaves human_code NULL.
- ``T_OTHER``          Fallback — docid-only extraction.

Field outputs:

- ``human_code``          Short citation form ("TR 2024/3", "[2024] HCA 41",
                         "PepsiCo Inc v Commissioner of Taxation"). May
                         be NULL for templates without a citable form.
- ``first_published_date`` ISO date if a precise one is visible, else
                          YYYY-01-01 from the year, else None.
- ``citation_year``       Integer year the citation refers to.
- ``variant``             Suffix like "Addendum", "Erratum", "EC".
- ``status``              "draft" / "withdrawn" / "consolidated" / None.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, replace
from enum import Enum
from typing import Callable


# ---------------------------------------------------------------------------
# Input / output shapes


@dataclass(frozen=True)
class RuleInputs:
    doc_id: str
    title: str | None = None
    headings: tuple[str, ...] = ()
    body_head: str = ""
    category: str | None = None
    pub_date: str | None = None

    @property
    def outer_prefix(self) -> str:
        segs = [s for s in self.doc_id.split("/") if s]
        return segs[0].upper() if segs else ""

    @property
    def inner_body(self) -> str:
        segs = [s for s in self.doc_id.split("/") if s]
        return segs[1] if len(segs) >= 2 else ""

    @property
    def h1(self) -> str:
        return self.headings[0].strip() if self.headings else ""

    @property
    def h2(self) -> str:
        return self.headings[1].strip() if len(self.headings) > 1 else ""

    @property
    def h3(self) -> str:
        return self.headings[2].strip() if len(self.headings) > 2 else ""


@dataclass(frozen=True)
class DerivedMetadata:
    """Output of the rule engine — two fields, nothing else.

    ``title``: human-readable identifier, citation inlined when one exists.
    Shaped like ``"TR 2024/3 — R&D tax incentive eligibility"`` for rulings,
    ``"PepsiCo Inc v Commissioner of Taxation"`` for cases,
    ``"Income Tax Assessment Act 1997"`` for Acts,
    ``"EV 1012345678901"`` for edited private advice.

    ``date``: ISO yyyy-mm-dd best-guess publication date used for filters
    and recency sort. Authoritative-looking but not authoritative — we
    surface whatever we could derive, in waterfall order:
    precise body date → scraped ``pub_date`` → year-from-docid.

    Currency markers (W2.2: ``withdrawn_date`` / ``superseded_by`` /
    ``replaces``) live on the HTML-side ``CurrencyInfo`` rather than here —
    the rule pipeline doesn't see the raw HTML, and threading them through
    the rule layer would just mean a no-op pass-through. The build code
    plumbs ``CurrencyInfo`` directly into the document row.
    """
    title: str | None = None
    date: str | None = None


_EMPTY = DerivedMetadata()


# ---------------------------------------------------------------------------
# Heading-shape classification
#
# Each shape is a cheap regex-based feature over one heading string. The
# classification dispatch reads shapes, not raw text.


class Shape(str, Enum):
    EMPTY = "EMPTY"
    RULING_TYPE_PHRASE = "RULING_TYPE_PHRASE"     # "Taxation Ruling", "Class Ruling", ...
    GUIDELINE_TYPE_PHRASE = "GUIDELINE_TYPE_PHRASE"  # "Practical Compliance Guideline(s)"
    ALERT_PHRASE = "ALERT_PHRASE"                 # "Taxpayer Alert"
    ATOID_PHRASE = "ATOID_PHRASE"                 # "ATO Interpretative Decision"
    PSLA_PHRASE = "PSLA_PHRASE"                   # "Practice Statement Law Administration"
    SMSFRB_PHRASE = "SMSFRB_PHRASE"               # "SMSF Regulator's Bulletin"
    DIS_PHRASE = "DIS_PHRASE"                     # "Decision impact statement"
    EM_PHRASE = "EM_PHRASE"                       # "Explanatory Memorandum"
    RULING_CITATION = "RULING_CITATION"           # "TR 2024/3" etc.
    RULING_UNSLASHED = "RULING_UNSLASHED"         # "IT 1", "MT 2005", "CRP 2017/1"
    ATOID = "ATOID"                               # "ATO ID 2024/3"
    PSLA = "PSLA"                                 # "PS LA 2024/3"
    SMSFRB = "SMSFRB"                             # "SMSFRB 2020/1"
    NEUTRAL_CITATION = "NEUTRAL_CITATION"         # "[2024] HCA 41"
    NAME_V_NAME = "NAME_V_NAME"                   # "Smith v Jones"
    RE_X = "RE_X"                                 # "Re Smith", "In re Smith"
    CASE_NUMBER = "CASE_NUMBER"                   # "Case 9/93", "Case K68"
    ACT_TITLE = "ACT_TITLE"                       # "Income Tax Assessment Act 1997"
    BILL_TITLE = "BILL_TITLE"                     # "... Bill 2024"
    OTHER = "OTHER"


# Regex index used for shape detection only. Recipes that extract values
# (year, number, variant) may re-use these — but dispatch cares about
# *which* shape fired, not the captured groups.

_RULING_SERIES_ALT = "|".join(sorted([
    "SMSFRB", "SMSFR", "SMSFD", "GSTR", "GSTD", "FBTR", "WETR", "WETD",
    "LCR", "SGR", "FTR", "PCG", "LCG", "PRR", "CLR", "COG", "TXD", "TPA",
    "FBT", "GII", "CR", "PR", "TR", "TD", "MT", "TA", "LI", "LG", "WT", "IT",
], key=len, reverse=True))

# Un-slashed legacy series: "IT 1", "IT 117", "MT 2005" — pre-year-based
# numbering where the number itself is the identifier.
_UNSLASHED_LEGACY_ALT = "|".join(["IT", "MT", "CRP"])

_RE_RULING_CITATION = re.compile(
    rf"^({_RULING_SERIES_ALT})\s+\d{{1,4}}/D?\d+(?:[A-Z0-9]+)?(?:\s|$|\()"
)
_RE_RULING_UNSLASHED = re.compile(
    rf"^({_UNSLASHED_LEGACY_ALT})\s+\d{{1,5}}(?:\s|$|[—-])"
)
_RE_ATOID = re.compile(r"^ATO\s+ID\s+\d{4}/\d+")
_RE_PSLA = re.compile(r"^PS\s+LA\s+\d{4}/")
_RE_SMSFRB = re.compile(r"^SMSFRB\s+\d{4}/")
_RE_NEUTRAL = re.compile(r"^\[\d{4}\]\s+[A-Z]+\s+\d+")
_RE_NAME_V_NAME = re.compile(
    r"^[A-Z][\w'.&-]*(?:\s+(?:\([^)]+\)|[A-Z][\w'.&-]*|and|of|the|for|on|in|an|Anor|ors?|No|nee))*"
    r"(?:,?\s+(?:Pty\s+)?(?:Ltd|Limited|Inc\.?|LLC|Corp|Co\.?|Plc))?"
    r"\s+(?:v\.?|vs\.?)\s+"
    r"(?:the|a|an)?\s*(?:\([^)]+\)\s*)?[A-Za-z][\w'.&-]*",
    re.IGNORECASE,
)
_RE_RE_X = re.compile(
    r"^(?:Re|In\s+re|In\s+the\s+Matter\s+of|Ex\s+parte)\s+[A-Z]",
    re.IGNORECASE,
)
_RE_CASE_NUMBER = re.compile(
    r"^Case\s+[A-Z]?\d+(?:/\d+)?$",
    re.IGNORECASE,
)
_RE_ACT_TITLE = re.compile(
    r"^(?:[A-Za-z][\w'&-]*|\([^)]+\))(?:\s+(?:[A-Za-z][\w'&-]*|\([^)]+\)))*"
    r"\s+(?:Act|Regulations?|Code|Rules)\s+(?:19|20)\d{2}"
    r"(?:\s*\(Cth\))?\s*$",
    re.IGNORECASE,
)
_RE_BILL_TITLE = re.compile(r"\bBill\s+(?:19|20)\d{2}\b")

_TYPE_PHRASES = {
    Shape.RULING_TYPE_PHRASE: {
        "taxation ruling", "class ruling", "product ruling",
        "law companion ruling", "gst ruling", "gst determination",
        "taxation determination", "superannuation guarantee ruling",
        "fuel tax ruling", "fringe benefits tax ruling",
        "income tax ruling", "miscellaneous taxation ruling",
        "law companion guideline", "wine equalisation tax ruling",
        "wine equalisation tax determination",
        "superannuation guarantee determination",
        "smsf ruling", "smsf determination", "ruling compendium",
        "goods and services tax ruling", "goods and services tax determination",
    },
    Shape.GUIDELINE_TYPE_PHRASE: {
        "practical compliance guideline", "practical compliance guidelines",
    },
    Shape.ALERT_PHRASE: {
        "taxpayer alert",
    },
    Shape.ATOID_PHRASE: {
        "ato interpretative decision",
    },
    Shape.PSLA_PHRASE: {
        "practice statement law administration",
        "ato practice statement law administration",
        "law administration practice statement",
    },
    Shape.SMSFRB_PHRASE: {
        "smsf regulator's bulletin", "smsf regulators bulletin",
    },
    Shape.DIS_PHRASE: {
        "decision impact statement", "decision impact statements",
    },
    Shape.EM_PHRASE: {
        "explanatory memorandum", "supplementary explanatory memorandum",
    },
}


def shape_of(heading: str) -> Shape:
    """Classify a heading string into a coarse structural shape.

    Order matters: citations before type phrases, because a heading like
    ``"TR 2024/3"`` might technically match ``"Taxation Ruling"`` (it
    doesn't, but the principle holds for shorter strings).
    """
    if not heading or not heading.strip():
        return Shape.EMPTY
    # Collapse internal whitespace — ATO headings occasionally embed
    # newlines mid-sentence ("COURT\nOF APPEAL OF NEW ZEALAND"), which
    # would otherwise break anchored regexes.
    t = " ".join(heading.split())
    t_lower = t.lower()

    # Citations — the most specific shapes.
    if _RE_NEUTRAL.match(t):
        return Shape.NEUTRAL_CITATION
    if _RE_ATOID.match(t):
        return Shape.ATOID
    if _RE_PSLA.match(t):
        return Shape.PSLA
    if _RE_SMSFRB.match(t):
        return Shape.SMSFRB
    if _RE_RULING_CITATION.match(t):
        return Shape.RULING_CITATION
    if _RE_RULING_UNSLASHED.match(t):
        return Shape.RULING_UNSLASHED
    # Structural phrases — exact match on a known set. We don't accept the
    # phrase as a *prefix* because longer sentences that start with a type
    # phrase ("Taxation Ruling system: explanation and status") would
    # otherwise get classified as the type phrase itself.
    for sh, phrases in _TYPE_PHRASES.items():
        if t_lower in phrases:
            return sh
    # Acts / Bills.
    if _RE_ACT_TITLE.match(t):
        return Shape.ACT_TITLE
    if _RE_BILL_TITLE.search(t):
        return Shape.BILL_TITLE
    # Case-name forms.
    if _RE_RE_X.match(t):
        return Shape.RE_X
    if _RE_CASE_NUMBER.match(t):
        return Shape.CASE_NUMBER
    if _RE_NAME_V_NAME.match(t) and len(t) < 200:
        return Shape.NAME_V_NAME
    return Shape.OTHER


# ---------------------------------------------------------------------------
# Templates
#
# Each template has:
# - matches(ins): does this template apply to this doc?
# - extract(ins) -> DerivedMetadata: how to read the fields
#
# Classification iterates templates in priority order; first match wins.
# Extraction is positional (headings[N] reads) with small helper regexes
# to pull year / number / variant tokens out of already-classified strings.


class Template(str, Enum):
    OFFICIAL_PUB = "OFFICIAL_PUB"
    CASE_H1 = "CASE_H1"
    CASE_H2 = "CASE_H2"
    HIST_CASE = "HIST_CASE"
    DIS = "DIS"
    ACT = "ACT"
    LEGISLATION_SECTION = "LEGISLATION_SECTION"
    BILL_EM = "BILL_EM"
    SMSFRB = "SMSFRB"
    EPA = "EPA"
    OTHER = "OTHER"


def classify(ins: RuleInputs) -> Template:
    """Pick a template based on heading shapes + category hints.

    We scan every heading (up to headings[:6]) rather than only h1/h2, so
    noise like a URL as h0 — an extraction artefact on some scraped pages
    — doesn't mask the real citation/type-phrase pair living at h1/h2.

    Priority order (most specific first):
    1. SMSFRB — bulletin shape anywhere in headings.
    2. OFFICIAL_PUB — a type-phrase heading and a citation heading both
       appear in the first several headings (in any order).
    3. DIS — Decision Impact Statement phrase + a name-v-name.
    4. CASE_H1 / CASE_H2 — court-case name forms.
    5. ACT — Act / Regulation title at h1.
    6. BILL_EM — EM / bill title.
    7. EPA — explicit Edited_private_advice category.
    8. OTHER — fallback to docid-only extraction.
    """
    shapes: list[Shape] = [shape_of(h) for h in ins.headings[:6]]
    has = lambda s: s in shapes  # noqa: E731 — tiny helper
    any_citation = any(
        s in (
            Shape.RULING_CITATION, Shape.RULING_UNSLASHED,
            Shape.ATOID, Shape.PSLA,
        )
        for s in shapes
    )
    any_type_phrase = any(s in (
        Shape.RULING_TYPE_PHRASE, Shape.GUIDELINE_TYPE_PHRASE, Shape.ALERT_PHRASE,
        Shape.ATOID_PHRASE, Shape.PSLA_PHRASE,
    ) for s in shapes)

    # SMSFRB — citation shape anywhere, OR the bulletin phrase.
    if has(Shape.SMSFRB) or has(Shape.SMSFRB_PHRASE):
        return Template.SMSFRB

    # Historical case: docid like JUD/*YYYY*REPORT/... — the year is
    # encoded in the docid itself between asterisks. Route before the
    # generic CASE_H1 so year-from-docid takes precedence over the
    # (often absent) neutral citation.
    if ins.outer_prefix == "JUD" and _RE_DOCID_JUD_STAR.match(ins.inner_body):
        return Template.HIST_CASE

    # Legislation-section: PAC/REG docs address a single section of an
    # Act or Regulation. The docid body encodes the Act number + section.
    if ins.outer_prefix in ("PAC", "REG") and _RE_DOCID_ACT_SECTION.match(ins.inner_body):
        return Template.LEGISLATION_SECTION

    # OFFICIAL_PUB — both a type phrase and a citation somewhere. A
    # missing type phrase is tolerated if a citation appears (covers pages
    # where the H1 is a URL artefact).
    if any_citation and (any_type_phrase or True):
        return Template.OFFICIAL_PUB

    # Un-slashed legacy ruling ("IT 1", "MT 2005") without a modern
    # citation — still an OFFICIAL_PUB template, just the legacy shape.
    if any(s == Shape.RULING_UNSLASHED for s in shapes):
        return Template.OFFICIAL_PUB

    # DIS — DIS phrase at any depth, with a case name nearby.
    if has(Shape.DIS_PHRASE) or (
        ins.category == "Decision_impact_statements" and has(Shape.NAME_V_NAME)
    ):
        return Template.DIS

    # Court case — name in h1 or h2, or any case-shape in heading list.
    if shapes and shapes[0] in (Shape.NAME_V_NAME, Shape.RE_X, Shape.NEUTRAL_CITATION, Shape.CASE_NUMBER):
        return Template.CASE_H1
    if len(shapes) >= 2 and shapes[1] == Shape.NAME_V_NAME and ins.category == "Cases":
        return Template.CASE_H2
    if ins.category == "Cases":
        # Any heading has a case shape — route to CASE_H1, extractor will
        # pick the right one.
        if any(s in (Shape.NAME_V_NAME, Shape.RE_X, Shape.NEUTRAL_CITATION,
                     Shape.CASE_NUMBER) for s in shapes):
            return Template.CASE_H1
        # Fallback for Cases — even an OTHER-shaped heading is usually the
        # case's identifier (court name + parties concatenated).
        return Template.CASE_H1

    # Legislation — Act / Regulation / Code.
    if shapes and shapes[0] == Shape.ACT_TITLE:
        return Template.ACT
    if has(Shape.ACT_TITLE) and ins.category == "Legislation_and_supporting_material":
        return Template.ACT

    # Bill / EM.
    if has(Shape.BILL_TITLE) or has(Shape.EM_PHRASE):
        return Template.BILL_EM

    if ins.category == "Edited_private_advice":
        return Template.EPA

    return Template.OTHER


# ---------------------------------------------------------------------------
# Extractors


_RE_CITATION_TOKEN = re.compile(
    rf"^({_RULING_SERIES_ALT})\s+(?P<year>\d{{1,4}})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)"
)
_RE_ATOID_TOKEN = re.compile(r"^ATO\s+ID\s+(?P<year>\d{4})/(?P<num>\d+)(?P<suffix>[A-Z0-9]*)")
_RE_PSLA_TOKEN = re.compile(r"^PS\s+LA\s+(?P<year>\d{4})/(?P<draft>D?)(?P<num>\d+)(?P<suffix>[A-Z0-9]*)")
_RE_SMSFRB_TOKEN = re.compile(r"^SMSFRB\s+(?P<year>\d{4})/(?P<num>\d+)")
_RE_NEUTRAL_TOKEN = re.compile(r"^\[(?P<year>\d{4})\]\s+(?P<court>[A-Z]+)\s+(?P<num>\d+)")
_RE_ACT_YEAR = re.compile(r"\b(?P<year>(?:19|20)\d{2})\b")
_RE_BILL_YEAR = re.compile(r"\bBill\s+(?P<year>(?:19|20)\d{2})\b")
_RE_WITHDRAWN = re.compile(r"\(\s*withdrawn\s*\)", re.IGNORECASE)
_RE_REPLACED = re.compile(r"\(\s*(?:replaced|superseded)\b", re.IGNORECASE)
_RE_PRECISE_DATE = re.compile(
    r"\b(\d{1,2})\s+(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{4})\b",
    re.IGNORECASE,
)
_MONTH = {m.lower(): i for i, m in enumerate(
    ["January", "February", "March", "April", "May", "June",
     "July", "August", "September", "October", "November", "December"], 1)}
_RE_OLD_REPORT = re.compile(r"\((?P<year>1[89]\d{2}|20\d{2})\)\s+(?:L\.?R\.?|AC|QB|KB|Ch|CLR|ALR|ATC|ATR|FCR|HL|PC|NSWLR|VR|QR|SASR)")

# Historical case docid — JUD/*YYYY*<reporter><number>/<part>.
# Example: ``JUD/*1881*17chd746/00001``.
_RE_DOCID_JUD_STAR = re.compile(r"^\*(?P<year>\d{4})\*(?P<rest>.+)$")

# Legislation-section docid — PAC/<year><actno>/<section>, REG/<year><regno>/<regnum>.
# Example: ``PAC/19210026/1`` = Act of 1921 No.26, section 1.
_RE_DOCID_ACT_SECTION = re.compile(r"^(?P<year>\d{4})(?P<actno>\d{4})$")

# Body MailTo encoding — ATO pages embed a mailto URL whose Body contains
# %0D-separated breadcrumb lines that mirror the navigation context. This
# is present on nearly every legislation / case doc and often survives
# when the extractor otherwise returned only a URL heading.
_RE_MAILTO_BODY = re.compile(r"MailTo:\?Subject=[^&]*&Body=([^)\s\"]+)")

# Case header seen at the top of judgement bodies:
# ``*## <Case Name>* | **(YYYY) REPORT ...** |`` or ``**[YYYY] NEUTRAL ...**``.
_RE_CASE_HEADER_NAME = re.compile(r"^\*##\s+(?P<name>[^*\n]+?)\s*\*")


def _clean_citation(raw: str) -> str:
    """Normalise a citation heading to the ATO display form.

    Example: ``'LCR 2019/2EC (Withdrawn)'`` -> ``'LCR 2019/2EC'``.
    """
    cleaned = _RE_WITHDRAWN.sub("", raw).strip()
    cleaned = re.sub(r"\s+", " ", cleaned)
    m = re.match(
        rf"^({_RULING_SERIES_ALT}|ATO\s+ID|PS\s+LA|SMSFRB)\s+(\d{{1,4}})/(D?)(\d+)([A-Z]{{1,2}}\d*)?$",
        cleaned,
    )
    if m:
        series = m.group(1)
        year = m.group(2)
        draft = m.group(3)
        num = m.group(4)
        suffix = m.group(5) or ""
        return f"{series} {year}/{draft}{num}{suffix}"
    return cleaned


def _year_from_token(token: str) -> int | None:
    m = _RE_CITATION_TOKEN.match(token) or _RE_ATOID_TOKEN.match(token) or \
        _RE_PSLA_TOKEN.match(token) or _RE_SMSFRB_TOKEN.match(token) or \
        _RE_NEUTRAL_TOKEN.match(token)
    if m:
        y = m.groupdict().get("year")
        if y:
            try:
                return int(y) if len(y) == 4 else (1900 + int(y))
            except ValueError:
                return None
    return None


def _precise_date(text: str) -> str | None:
    m = _RE_PRECISE_DATE.search(text)
    if not m:
        return None
    day, month_name, year = m.groups()
    month = _MONTH[month_name.lower()]
    return f"{int(year):04d}-{month:02d}-{int(day):02d}"


_TYPE_PHRASE_SHAPES = {
    Shape.RULING_TYPE_PHRASE, Shape.GUIDELINE_TYPE_PHRASE,
    Shape.ATOID_PHRASE, Shape.PSLA_PHRASE, Shape.SMSFRB_PHRASE,
    Shape.DIS_PHRASE, Shape.ALERT_PHRASE, Shape.EM_PHRASE,
}

_CITATION_SHAPES = {
    Shape.RULING_CITATION, Shape.RULING_UNSLASHED,
    Shape.ATOID, Shape.PSLA, Shape.SMSFRB, Shape.NEUTRAL_CITATION,
    Shape.NAME_V_NAME, Shape.RE_X, Shape.CASE_NUMBER,
    Shape.ACT_TITLE, Shape.BILL_TITLE,
}


def _compose_title(primary: str | None, ins: RuleInputs) -> str | None:
    """Build a human-readable title from a primary identifier + one subtitle.

    ``primary`` is the specific identifier the template found — a citation,
    case name, or Act name. We append at most one additional heading as a
    subtitle, skipping type-phrase prefixes ("Taxation Ruling") and other
    citation-shaped headings (which would just repeat the primary).
    """
    if not primary:
        return None
    primary = " ".join(primary.split())
    parts = [primary]
    seen = {primary.lower()}
    for h in ins.headings[:5]:
        t = " ".join((h or "").split())
        if not t or t.lower() in seen:
            continue
        if t.startswith("/law/view/"):
            continue
        s = shape_of(t)
        if s in _TYPE_PHRASE_SHAPES or s in _CITATION_SHAPES:
            continue
        parts.append(t)
        break
    return " — ".join(parts)


# --- Template: Official Publication -----------------------------------------


def _extract_official_pub(ins: RuleInputs) -> DerivedMetadata:
    """Rulings, PCG, TA, PS LA, ATO ID — uniform shape.

    The citation is whichever heading matched RULING_CITATION / ATOID /
    PSLA during classification. We re-scan to locate it because a URL
    artefact at h0 may push the real citation to h3.
    """
    citation_heading: str | None = None
    unslashed_heading: str | None = None
    for h in ins.headings[:6]:
        s = shape_of(h)
        if s in (Shape.RULING_CITATION, Shape.ATOID, Shape.PSLA):
            citation_heading = h
            break
        if unslashed_heading is None and s == Shape.RULING_UNSLASHED:
            unslashed_heading = h
    if citation_heading is None and unslashed_heading is not None:
        # Legacy un-yeared series — "IT 1", "MT 2005". The heading IS the
        # identifier; no slash, no year.
        t = " ".join(unslashed_heading.split())
        t = re.sub(r"\s*[—-].*$", "", t).strip()
        return DerivedMetadata(
            title=_compose_title(t, ins),
            date=_precise_date(ins.body_head[:600]),
        )
    if citation_heading is None:
        # Classifier routed us here but no citation shape matched — fall
        # back to docid.
        return _extract_other(ins)
    cleaned = _clean_citation(citation_heading)
    year = _year_from_token(cleaned)
    precise = _precise_date(ins.body_head[:600])
    # If the raw heading carried a ``(Withdrawn)`` marker, keep it — agents
    # use that signal to avoid quoting superseded rulings.
    if _RE_WITHDRAWN.search(citation_heading):
        cleaned = f"{cleaned} (Withdrawn)"
    return DerivedMetadata(
        title=_compose_title(cleaned, ins),
        date=precise or (f"{year}-01-01" if year else None),
    )


# --- Templates: Court Cases -------------------------------------------------


def _case_name_from(heading: str) -> str | None:
    # Collapse internal whitespace — some scraped headings embed newlines
    # mid-party-name (e.g. "COMMISSIONER\nOF INLAND REVENUE (NZ) v ...").
    t = " ".join(heading.split())
    # Reject fragments that look too long to be a case name.
    if not t or len(t) > 200:
        return None
    # Trim trailing "[YYYY] COURT N..." if it's attached to the name.
    t = re.sub(r"\s*\[\d{4}\].*$", "", t).strip()
    # Normalise "v." -> "v".
    t = re.sub(r"\bv\.\s+", "v ", t)
    return t


def _extract_case_h1(ins: RuleInputs) -> DerivedMetadata:
    """Court case — case identifier in one of the first few headings.

    The heading that carries the identifier varies:
    - Modern docs: h1 is the case name.
    - Legacy docs: h1 is the court name and h2 / a later heading has the
      case name, OR h1 is a concatenated "COURT — PARTY v PARTY — JUDGE"
      string (a single heading with em-dash separators).
    - Board of Review cases: h1 is "Case 9/93" or "Case K68".
    """
    name: str | None = None
    year: int | None = None

    for h in ins.headings[:5]:
        s = shape_of(h)
        if s == Shape.NEUTRAL_CITATION:
            m = _RE_NEUTRAL_TOKEN.match(h.strip())
            if m:
                name = f"[{m['year']}] {m['court']} {m['num']}"
                year = int(m["year"])
                break
        if s == Shape.NAME_V_NAME:
            name = _case_name_from(h)
            break
        if s == Shape.RE_X:
            name = _case_name_from(h)
            break
        if s == Shape.CASE_NUMBER:
            name = " ".join(h.split())
            break

    if name is None:
        for h in ins.headings[:3]:
            for part in re.split(r"\s+—\s+", h):
                part = " ".join(part.split())
                ps = shape_of(part)
                if ps == Shape.NAME_V_NAME and part != h:
                    name = _case_name_from(part)
                    break
                if ps == Shape.NEUTRAL_CITATION:
                    m = _RE_NEUTRAL_TOKEN.match(part)
                    if m:
                        name = f"[{m['year']}] {m['court']} {m['num']}"
                        year = int(m["year"])
                        break
            if name:
                break

    if name is None and ins.category == "Cases":
        for h in ins.headings[:3]:
            clean = " ".join(h.split())
            if clean and not clean.startswith("/law/view/") and len(clean) < 200:
                name = clean
                break

    if year is None:
        for src in [ins.title or "", *ins.headings[:5]]:
            m = _RE_NEUTRAL_TOKEN.search(src)
            if m:
                year = int(m["year"])
                break
    if year is None:
        old = _RE_OLD_REPORT.search(ins.body_head[:400])
        if old:
            year = int(old["year"])

    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=name,
        date=precise or (f"{year}-01-01" if year else None),
    )


def _extract_case_h2(ins: RuleInputs) -> DerivedMetadata:
    """Court case layouts where h2 carries the case name."""
    name = _case_name_from(ins.h2)
    neutral = _RE_NEUTRAL_TOKEN.search(ins.body_head[:500])
    year = int(neutral["year"]) if neutral else None
    if year is None:
        old = _RE_OLD_REPORT.search(ins.body_head[:500])
        if old:
            year = int(old["year"])
    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=name,
        date=precise or (f"{year}-01-01" if year else None),
    )


def _extract_dis(ins: RuleInputs) -> DerivedMetadata:
    """Decision Impact Statement — h1 is DIS phrase, h2 is the case name."""
    case_name = _case_name_from(ins.h2) if ins.h2 else _case_name_from(ins.h1)
    neutral = _RE_NEUTRAL_TOKEN.search(ins.body_head[:1200])
    year = int(neutral["year"]) if neutral else None
    precise = _precise_date(ins.body_head[:600])
    # Prefix "DIS: " so the agent can distinguish a DIS title from the
    # underlying judgement — both share the case name otherwise.
    title = f"DIS: {case_name}" if case_name else None
    return DerivedMetadata(
        title=title,
        date=precise or (f"{year}-01-01" if year else None),
    )


# --- Template: Legislation / Act --------------------------------------------


def _extract_act(ins: RuleInputs) -> DerivedMetadata:
    """Act / Regulation / Code title in h1 — the Act's name IS the title."""
    name = " ".join(ins.h1.split())
    ym = _RE_ACT_YEAR.search(name)
    year = int(ym["year"]) if ym else None
    return DerivedMetadata(
        title=name or None,
        date=f"{year}-01-01" if year else None,
    )


# --- Template: Legislation section (PAC / REG) ------------------------------


def _parse_mailto_body(body_head: str) -> list[str]:
    """Extract the %0D-separated breadcrumb lines from the body's mailto URL.

    Returns decoded trimmed lines, excluding empty ones and the trailing
    ``Link: https://...`` line. Many ATO legislation / judgement pages that
    otherwise lack h1 text carry their real breadcrumb here.
    """
    m = _RE_MAILTO_BODY.search(body_head or "")
    if not m:
        return []
    raw = m.group(1)
    # URL decode — %0D is the line separator, but actual content may also
    # have %20 (space) etc. We handle %0D manually to preserve line breaks.
    parts = raw.split("%0D")
    out: list[str] = []
    for p in parts:
        # Collapse any remaining % escapes via urllib.
        try:
            from urllib.parse import unquote
            text = unquote(p).strip()
        except Exception:
            text = p.strip()
        if not text or text.lower().startswith("link:"):
            continue
        out.append(text)
    return out


def _extract_legislation_section(ins: RuleInputs) -> DerivedMetadata:
    """PAC/REG docs — one section of an Act / Regulation.

    Title priority: extract Act name from headings or mailto breadcrumb,
    append " s <num>" (for Acts) or " reg <num>" (for Regulations). Fall
    back to the docid-derived form when no Act name is found.
    """
    inner = ins.inner_body
    m = _RE_DOCID_ACT_SECTION.match(inner)
    year: int | None = None
    act_no: str | None = None
    if m:
        year = int(m["year"])
        act_no = m["actno"].lstrip("0") or "0"
    segs = [s for s in ins.doc_id.split("/") if s]
    section_id = segs[2] if len(segs) >= 3 else ""

    act_name: str | None = None
    for h in ins.headings[:6]:
        t = " ".join(h.split())
        if _RE_ACT_TITLE.match(t):
            act_name = t
            break
    if not act_name:
        for line in _parse_mailto_body(ins.body_head):
            if _RE_ACT_TITLE.match(line):
                act_name = line
                break

    if act_name:
        if section_id:
            title = f"{act_name} s {section_id}" if ins.outer_prefix == "PAC" else f"{act_name} reg {section_id}"
        else:
            title = act_name
    else:
        if ins.outer_prefix == "PAC":
            title = f"Act {year} No. {act_no} s {section_id}" if year else f"PAC {inner}/{section_id}"
        else:
            title = f"Regulations {year} reg {section_id}" if year else f"REG {inner}/{section_id}"

    if year is None:
        ym = _RE_ACT_YEAR.search(act_name or "")
        year = int(ym["year"]) if ym else None

    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=title,
        date=precise or (f"{year}-01-01" if year else None) or ins.pub_date,
    )


# --- Template: Historical Case (JUD/*YYYY*...) ------------------------------


def _extract_historical_case(ins: RuleInputs) -> DerivedMetadata:
    """Pre-modern cases whose docid encodes the year between asterisks.

    Body typically starts with ``*## <Case Name>* | **(YYYY) REPORT**``
    or has a breadcrumb mailto carrying ``<Court (YYYY)> | <Case> - (date)``.
    """
    m = _RE_DOCID_JUD_STAR.match(ins.inner_body)
    year: int | None = int(m["year"]) if m else None

    name: str | None = None
    hdr = _RE_CASE_HEADER_NAME.search(ins.body_head[:400])
    if hdr:
        name = " ".join(hdr["name"].split())
    if name is None:
        for line in _parse_mailto_body(ins.body_head):
            if line.lower() in ("cases",):
                continue
            if " v " in line or " - (" in line:
                nm = re.sub(r"\s*-\s*\([^)]+\)\s*$", "", line).strip()
                if nm and len(nm) < 200:
                    name = nm
                    break
    if name is None:
        for h in ins.headings[:4]:
            t = " ".join(h.split())
            if t and not t.startswith("/law/view/") and len(t) < 200:
                name = t
                break
    if name is None:
        name = ins.inner_body or None

    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=name,
        date=precise or (f"{year}-01-01" if year else None) or ins.pub_date,
    )


# --- Template: Bill / Explanatory Memorandum --------------------------------


def _extract_bill_em(ins: RuleInputs) -> DerivedMetadata:
    """Bill / EM — no canonical short code but we can give a useful label."""
    source = ins.h2 if _RE_BILL_YEAR.search(ins.h2) else ins.h1
    bill_title = " ".join(source.split())
    if not _RE_BILL_YEAR.search(bill_title) and not _RE_ACT_TITLE.match(bill_title):
        for line in re.findall(r"\*\*([^*]+?)\*\*", ins.body_head[:800]):
            line = " ".join(line.split())
            if _RE_BILL_YEAR.search(line) or _RE_ACT_TITLE.match(line):
                bill_title = line
                break
    ym = _RE_BILL_YEAR.search(bill_title) or _RE_ACT_YEAR.search(bill_title)
    year = int(ym["year"]) if ym else None
    title = None
    if bill_title:
        title = f"EM to {bill_title}" if ("Explanatory" not in bill_title and ym) else bill_title
    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=title,
        date=precise or (f"{year}-01-01" if year else None) or ins.pub_date,
    )


def _extract_smsfrb(ins: RuleInputs) -> DerivedMetadata:
    for h in ins.headings[:4]:
        m = _RE_SMSFRB_TOKEN.search(h)
        if m:
            year = int(m["year"])
            return DerivedMetadata(
                title=_compose_title(f"SMSFRB {m['year']}/{m['num']}", ins),
                date=f"{year}-01-01",
            )
    return _extract_other(ins)


# --- Templates: EPA and OTHER (fallback) ------------------------------------


_DOCID_YEAR4_RE = re.compile(
    rf"^({_RULING_SERIES_ALT})(?P<year>(?:19|20)\d{{2}})(?P<draft>D?)(?P<num>\d+)$"
)
_DOCID_YEAR2_RE = re.compile(
    rf"^({_RULING_SERIES_ALT})(?P<year>[89]\d)(?P<num>\d+)$"
)
_DOCID_PSLA_RE = re.compile(r"^PSLA(?P<year>\d{4})(?P<num>\d+)$")
_DOCID_PSLA_DRAFT_RE = re.compile(r"^PSD(?P<year>\d{4})D?(?P<num>\d+)$")
_DOCID_ATOID_RE = re.compile(r"^(?:ATOID|AID)(?P<year>\d{4})(?P<num>\d+)$")


def _extract_from_docid(ins: RuleInputs) -> tuple[str | None, int | None]:
    body = ins.inner_body
    m = _DOCID_YEAR4_RE.match(body)
    if m:
        series = m.group(1)
        y = int(m["year"])
        draft = m["draft"] or ""
        return f"{series} {m['year']}/{draft}{m['num']}", y
    m = _DOCID_PSLA_RE.match(body)
    if m:
        return f"PS LA {m['year']}/{m['num']}", int(m["year"])
    m = _DOCID_PSLA_DRAFT_RE.match(body)
    if m:
        return f"PS LA {m['year']}/D{m['num']}", int(m["year"])
    m = _DOCID_ATOID_RE.match(body)
    if m:
        return f"ATO ID {m['year']}/{m['num']}", int(m["year"])
    m = _DOCID_YEAR2_RE.match(body)
    if m:
        series = m.group(1)
        return f"{series} {m['year']}/{m['num']}", 1900 + int(m["year"])
    return None, None


_RE_DATE_OF_ADVICE = re.compile(
    r"\bDate\s+of\s+(?:advice|ruling|issue)\s*[:\-]?\s*"
    r"(?P<day>\d{1,2})\s+(?P<mon>January|February|March|April|May|June|July|August|September|October|November|December)\s+(?P<year>\d{4})",
    re.IGNORECASE,
)


def _extract_epa(ins: RuleInputs) -> DerivedMetadata:
    """Edited private advice — the auth number is the identity.

    The ATO identifies each EPA by its authorisation number (the inner
    segment of the docid, e.g. ``EV/1012101718232``). Date comes from
    ``Date of advice: ...`` in the body when present.
    """
    auth = ins.inner_body.strip()
    code = f"{ins.outer_prefix} {auth}".strip() if auth else None
    precise: str | None = None
    m = _RE_DATE_OF_ADVICE.search(ins.body_head[:1500])
    if m:
        month = _MONTH[m["mon"].lower()]
        precise = f"{int(m['year']):04d}-{month:02d}-{int(m['day']):02d}"
    date = precise or ins.pub_date
    return DerivedMetadata(title=code, date=date)


def _extract_other(ins: RuleInputs) -> DerivedMetadata:
    """Fallback — try docid, pub_date, body date; leave NULL if nothing fires."""
    code, year = _extract_from_docid(ins)
    if ins.pub_date and len(ins.pub_date) >= 4 and ins.pub_date[:4].isdigit() and year is None:
        year = int(ins.pub_date[:4])
    precise = _precise_date(ins.body_head[:600])
    return DerivedMetadata(
        title=code,
        date=precise or ins.pub_date or (f"{year}-01-01" if year else None),
    )


# ---------------------------------------------------------------------------
# Public API


_EXTRACTORS: dict[Template, Callable[[RuleInputs], DerivedMetadata]] = {
    Template.OFFICIAL_PUB: _extract_official_pub,
    Template.CASE_H1: _extract_case_h1,
    Template.CASE_H2: _extract_case_h2,
    Template.HIST_CASE: _extract_historical_case,
    Template.DIS: _extract_dis,
    Template.ACT: _extract_act,
    Template.LEGISLATION_SECTION: _extract_legislation_section,
    Template.BILL_EM: _extract_bill_em,
    Template.SMSFRB: _extract_smsfrb,
    Template.EPA: _extract_epa,
    Template.OTHER: _extract_other,
}


def _universal_fallback_title(ins: RuleInputs) -> str | None:
    """Last-resort title from the docid itself.

    ATO's docid has the shape ``<PREFIX>/<body>/<part>``. Combining the
    prefix and body gives a unique, human-readable label that mirrors how
    the ATO's own URL bar identifies the doc — always populated.
    """
    if ins.outer_prefix and ins.inner_body:
        return f"{ins.outer_prefix} {ins.inner_body}"
    if ins.outer_prefix:
        return ins.outer_prefix
    return None


def derive_metadata(inputs: RuleInputs) -> DerivedMetadata:
    """Classify the page's template, then run that template's extractor.

    Two-stage fallback ensures every doc gets a title and a date:
    title → template → docid-citation parser → ``<prefix> <body>``.
    date  → template → pub_date → year-from-docid.
    """
    template = classify(inputs)
    result = _EXTRACTORS[template](inputs)
    if result.title is None:
        fallback_code, fb_year = _extract_from_docid(inputs)
        if fallback_code:
            result = replace(result, title=fallback_code)
            if result.date is None and fb_year:
                result = replace(result, date=f"{fb_year}-01-01")
    if result.title is None:
        universal = _universal_fallback_title(inputs)
        if universal:
            result = replace(result, title=universal)
    if result.date is None:
        if inputs.pub_date:
            result = replace(result, date=inputs.pub_date)
        else:
            fb_year = _year_from_docid(inputs)
            if fb_year:
                result = replace(result, date=f"{fb_year}-01-01")
    return result


def _year_from_docid(ins: RuleInputs) -> int | None:
    """Scan the docid's path segments for a 4-digit year.

    Handles:
      - ``*YYYY*`` between asterisks (historical JUD).
      - ``YYYYNNNN`` 4-digit-year prefix on 8-digit PAC/REG bodies.
      - ``<series>YYYY<num>`` on ruling series docids (already handled by
        ``_extract_from_docid`` but we allow a second pass here for cases
        the template extractor missed).
    """
    body = ins.inner_body
    m = _RE_DOCID_JUD_STAR.match(body)
    if m:
        try:
            return int(m["year"])
        except ValueError:
            return None
    m = _RE_DOCID_ACT_SECTION.match(body)
    if m:
        try:
            return int(m["year"])
        except ValueError:
            return None
    # Generic 4-digit prefix (NEM/200615 = 2006, though the trailing 15
    # is part of the same id — we only want the year).
    m = re.match(r"^((?:19|20)\d{2})", body)
    if m:
        try:
            return int(m.group(1))
        except ValueError:
            return None
    return None


def template_of(inputs: RuleInputs) -> Template:
    """Expose the classification decision (used by tests and for debugging)."""
    return classify(inputs)
