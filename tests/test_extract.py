"""HTML extraction edge cases."""
from __future__ import annotations

from pathlib import Path

from ato_mcp.indexer.extract import (
    CurrencyInfo,
    _doc_id_from_ato_link,
    extract,
    extract_currency,
)


def test_extract_law_contents_div() -> None:
    html = """
    <html><body>
        <header>skip me</header>
        <div id="lawContents">
            <h1 id="top">Taxation Ruling TR 2024/3</h1>
            <p>This ruling sets out the Commissioner's view.</p>
            <h2 id="background">Background</h2>
            <p>There is a specific scheme in place.</p>
        </div>
    </body></html>
    """
    doc = extract(html)
    assert "Taxation Ruling TR 2024/3" in doc.text
    assert doc.title == "Taxation Ruling TR 2024/3"
    # Anchor captured on heading
    assert ("Taxation Ruling TR 2024/3", "top") in doc.anchors
    assert ("Background", "background") in doc.anchors
    # Header nav was stripped
    assert "skip me" not in doc.text


def test_extract_missing_lawcontents_uses_article() -> None:
    html = """
    <html><body>
        <article>
            <h1>Court Decision</h1>
            <p>Judgment text.</p>
        </article>
    </body></html>
    """
    doc = extract(html)
    assert "Court Decision" in doc.text
    assert "Judgment text." in doc.text


def test_picker_handles_law_contents_capital_outermost() -> None:
    html = """
    <html><body>
      <div id="LawContents">
        <div id="LawBody">
          <h1>Old-format ATO doc</h1>
          <p>This document uses the capital-L plural wrapper as outermost.</p>
        </div>
      </div>
    </body></html>
    """
    doc = extract(html)
    assert "Old-format ATO doc" in doc.text
    assert "This document uses the capital-L plural wrapper as outermost." in doc.text


def test_extract_empty_returns_empty_text() -> None:
    doc = extract("")
    assert doc.text == ""
    assert doc.title is None


def test_extract_strips_scripts() -> None:
    html = """
    <div id="lawContents">
        <h1>Ruling</h1>
        <script>alert('x')</script>
        <p>Hello.</p>
    </div>
    """
    doc = extract(html)
    assert "alert" not in doc.text
    assert "Hello." in doc.text


def test_compose_title_from_leading_headings() -> None:
    """ATO rulings put h1=doc_type, h2=code, h3=subject consecutively."""
    html = """
    <div id="LawContent">
        <div id="LawFront">
            <h1>Class Ruling</h1>
            <h2>CR 2024/3</h2>
            <h3>Scrip for scrip rollover</h3>
        </div>
        <div id="LawBody">
            <p>The Commissioner rules as follows.</p>
            <h2>Background</h2>
            <p>The scheme is...</p>
        </div>
    </div>
    """
    doc = extract(html)
    assert doc.title == "Class Ruling — CR 2024/3 — Scrip for scrip rollover"
    # Background is a body section, not part of the title.
    assert "Background" not in (doc.title or "")


def test_extract_records_heading_levels_for_each_heading() -> None:
    """Heading levels are emitted in parallel with `headings` so downstream
    rules can distinguish a real h1 from an h2-only chapter shell."""
    html = """
    <div id="LawContent">
        <h1>Explanatory Memorandum</h1>
        <h2>EM 2006/15</h2>
        <h3>Tax Laws Amendment (Loss Recoupment Rules) Bill 2005</h3>
        <p>This Bill amends...</p>
    </div>
    """
    doc = extract(html)
    assert doc.headings == [
        "Explanatory Memorandum",
        "EM 2006/15",
        "Tax Laws Amendment (Loss Recoupment Rules) Bill 2005",
    ]
    assert doc.heading_levels == [1, 2, 3]


def test_extract_h2_only_chapter_keeps_empty_h1_marker_absent() -> None:
    """Subsidiary EM chapters expose only h2/h3; the heading_levels reflect
    that no h1 is present so the title composer can fall back to docid."""
    html = """
    <div id="lawContents">
        <div id="Lawbody">
            <h2></h2>
            <h3>EXPLANATORY STATEMENT</h3>
            <p>Body text.</p>
        </div>
    </div>
    """
    doc = extract(html)
    assert 1 not in doc.heading_levels


def test_extract_unwraps_source_wrapped_inline_fragments() -> None:
    html = """
    <div id="LawContent">
        <p class="text-left">S 355-210(1) amended by No 15 of 2017, s 3 and Sch 4 items 61
            <span>-</span>
            65, by omitting
            <span>"</span>
            <span>or an external Territory</span>
            <span>"</span>
            after
            <span>"</span>
            <span>within Australia</span>
            <span>"</span>
            from para (a) and (e)(i),</p>
    </div>
    """
    doc = extract(html)
    assert (
        'S 355-210(1) amended by No 15 of 2017, s 3 and Sch 4 items 61-65, '
        'by omitting "or an external Territory" after "within Australia" '
        "from para (a) and (e)(i),"
    ) in doc.text


def test_extract_keeps_literal_asterisks_unescaped() -> None:
    html = """
    <div id="LawContent">
        <p>You are entitled to a * tax offset for * foreign income tax.</p>
    </div>
    """
    doc = extract(html)
    assert r"\*" not in doc.text
    assert "a * tax offset for * foreign income tax" in doc.text


def test_extract_ignores_malformed_source_attribute_names() -> None:
    html = """
    <div id="LawContent">
        <p PAC/19010002/Pt8>Malformed source attribute should not break extraction.</p>
    </div>
    """
    doc = extract(html)
    assert "Malformed source attribute should not break extraction." in doc.text
    assert "PAC/19010002/Pt8" not in doc.html


def test_extract_removes_history_noise_and_rewrites_internal_links() -> None:
    html = """
    <div id="LawContent">
        <h1>Income Tax Assessment Act 1997 s 203-50</h1>
        <a href="/law/view/document?LocID=PAC%2F19970038%2F203-50&amp;db=HISTFT">View history reference</a>
        <img src="x.gif" title="View history note">View history note
        <img src="y.gif" title="Hide history note">Hide history note
        <div class="panel panel-default">
          <div class="panel-heading"><a name="LawTimeLine"></a><strong>Section history</strong></div>
          <div id="PiT"><p>S 203-50 inserted by No 48 of 2002.</p></div>
        </div>
        <h2>Operative provisions</h2>
        <p>See <a href="/law/view/document?LocID=%22PAC%2F19970038%2F203-55(1)%22">203-55(1)</a>.</p>
    </div>
    """
    doc = extract(html)
    assert "View history" not in doc.text
    assert "Hide history" not in doc.text
    assert "inserted by No 48" not in doc.text
    assert "203-55(1)" in doc.text
    assert 'data-doc-id="PAC/19970038/203-55(1)"' in doc.html
    assert "/law/view/document" not in doc.html


def test_extract_removes_ato_mini_menu_navigation() -> None:
    html = """
    <div id="lawContents">
      <div id="LawMiniMenuHeader">
        <a href="/law/view/pdf?DocId=TPA%2FTA20253">Download</a>
        <a href="/single-page-applications/legaldatabase/#Law/table-of-contents?docid=TPA/TA20253">Back to browse</a>
      </div>
      <h1>Taxpayer alert</h1>
      <p>Substantive alert text.</p>
    </div>
    """
    doc = extract(html)
    assert "LawMiniMenuHeader" not in doc.html
    assert "Back to browse" not in doc.text
    assert "Substantive alert text." in doc.text


def test_extract_preserves_legislation_inline_text_after_many_doc_links() -> None:
    links = "\n".join(
        f'<a href="/law/view/document?LocID=%22REG%2F20150033%2F91(3)%22">91(3)</a> {idx}'
        for idx in range(70)
    )
    html = f"""
    <div id="lawContents">
      <div id="LawMiniMenuHeader"><a href="/law/view/print">Print</a></div>
      <div id="LawContents">
        <div id="lawBody">
          <strong>SECTION 154</strong>
          <br>A document is taken to continue under this instrument.
          {links}
          <br>Tail text after repeated internal links must survive.
        </div>
      </div>
    </div>
    """
    doc = extract(html)
    assert "Print" not in doc.text
    assert "A document is taken to continue under this instrument." in doc.text
    assert "Tail text after repeated internal links must survive." in doc.text
    assert 'data-doc-id="REG/20150033/91(3)"' in doc.html
    assert "/law/view/document" not in doc.html


def test_extract_rewrites_images_to_asset_refs(tmp_path: Path) -> None:
    payload = tmp_path / "doc.html"
    asset = tmp_path / "assets" / "formula.gif"
    asset.parent.mkdir()
    asset.write_bytes(b"GIF89a-test")
    payload.write_text(
        """
        <div id="LawContent">
            <p>Formula <img src="assets/formula.gif" title="Annual amount formula"></p>
            <p><img src="assets/history.gif" title="View history note">View history note</p>
        </div>
        """,
        encoding="utf-8",
    )

    doc = extract(payload.read_text(encoding="utf-8"), doc_id="A/B/C", source_path=payload)

    assert len(doc.assets) == 1
    assert doc.assets[0].asset_ref == "ato-image://A%2FB%2FC/0"
    assert 'data-asset-ref="ato-image://A%2FB%2FC/0"' in doc.html
    # Plaintext drops the [image: alt] wrapper and emits the asset_ref marker
    # so agents reading chunks can call get_asset directly.
    assert "[asset:ato-image://A%2FB%2FC/0]" in doc.text
    assert "[image: Annual amount formula]" not in doc.text
    assert "Annual amount formula" not in doc.text
    assert "![" not in doc.text
    assert "View history note" not in doc.html


def test_extract_leaves_ambiguous_two_row_table_as_table() -> None:
    html = """
    <div id="LawContent">
        <table>
          <tr><td>Label</td></tr>
          <tr><td>Value</td></tr>
        </table>
    </div>
    """
    doc = extract(html)
    assert "Formula:" not in doc.text
    assert "Label\nValue" in doc.text


def test_extract_leaves_multi_cell_table_without_operator_as_table() -> None:
    html = """
    <div id="LawContent">
        <table>
          <tr><td>Label</td><td>Value</td></tr>
          <tr><td>Total</td></tr>
        </table>
    </div>
    """
    doc = extract(html)
    assert "Formula:" not in doc.text
    assert "Label | Value\nTotal" in doc.text


def test_extract_records_em_front_matter() -> None:
    """Parliamentary EM front-matter signals — chamber, ref, phrase — are
    captured from <div id="Lawfront"> for the title composer."""
    html = """
    <div id="Lawcontents">
        <div id="Lawfront">
            <strong>House of Representatives</strong>
            <p></p><div class="ref"><strong>Some Bill 2024</strong></div><p></p>
            <p><strong>Explanatory Memorandum</strong></p>(circulated by ...)
        </div>
        <div id="Lawbody">
            <h2>Chapter 3</h2>
            <p>Body text.</p>
        </div>
    </div>
    """
    doc = extract(html)
    assert doc.front_matter_chamber == "House of Representatives"
    assert doc.front_matter_refs == ["Some Bill 2024"]
    assert doc.front_matter_phrase == "Explanatory Memorandum"


def test_extract_records_em_front_matter_with_link_in_ref() -> None:
    """When a ref's <strong> wraps an internal-doc <a>, only the visible
    text is collected — the link metadata is not relevant to the citation."""
    html = """
    <div id="Lawcontents">
        <div id="Lawfront">
            <strong>House of Representatives</strong>
            <p></p><div class="ref">
                <strong><a data-doc-id="PAC/20240078">Bill Name</a></strong>
            </div><p></p>
            <p><strong>Explanatory Memorandum</strong></p>
        </div>
        <div id="Lawbody"><h2>Chapter 1</h2></div>
    </div>
    """
    doc = extract(html)
    assert doc.front_matter_refs == ["Bill Name"]
    assert doc.front_matter_phrase == "Explanatory Memorandum"


def test_extract_no_em_front_matter_returns_none() -> None:
    """Pages without an EM front-matter wrapper produce empty front-matter."""
    html = """
    <div id="LawContent">
        <h1>Taxation Ruling</h1>
        <h2>TR 2024/3</h2>
        <p>Body.</p>
    </div>
    """
    doc = extract(html)
    assert doc.front_matter_chamber is None
    assert doc.front_matter_refs == []
    assert doc.front_matter_phrase is None


def test_extract_em_front_matter_no_chamber_for_regulation_es() -> None:
    """Regulation Explanatory Statements (EXN/EXM/...) skip the chamber
    block — front_matter_chamber is None but refs and phrase populate."""
    html = """
    <div id="Lawcontents">
        <div id="Lawfront">
            <p></p><div class="ref">
                <strong><a data-doc-id="REG/20000109">Some Regulations 2000</a></strong>
            </div><p></p>
            <p><strong>Explanatory Statement</strong></p>Issued by ...
        </div>
        <div id="Lawbody"><h2>Explanatory Statement</h2></div>
    </div>
    """
    doc = extract(html)
    assert doc.front_matter_chamber is None
    assert doc.front_matter_refs == ["Some Regulations 2000"]
    assert doc.front_matter_phrase == "Explanatory Statement"


# ---------------------------------------------------------------------------
# W2.2 — currency / supersession extraction


def test_extract_currency_no_markers_returns_all_none() -> None:
    """Pages with no withdrawal markers yield an empty CurrencyInfo."""
    html = """
    <div id="LawContent">
        <div id="LawBody">
            <h1>Taxation Ruling</h1>
            <h2>TR 2024/3</h2>
            <p>The Commissioner rules on income tax matters.</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info == CurrencyInfo()


def test_extract_currency_handles_empty_html() -> None:
    assert extract_currency("") == CurrencyInfo()
    assert extract_currency("   ") == CurrencyInfo()


def test_extract_currency_withdrawal_prose_with_full_date() -> None:
    """Notice-of-Withdrawal page with prose 'withdrawn with effect from <date>'."""
    html = """
    <div id="LawContent">
        <div id="LawBody">
            <h3>Notice of Withdrawal</h3>
            <p class="indentlevel0">Taxation Ruling TR 2022/1 is withdrawn with effect from 31 October 2025.</p>
            <p class="indentlevel0">1. TR 2022/1 discusses the methodology used by the Commissioner.</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2025-10-31"


def test_extract_currency_withdrawal_prose_with_short_form() -> None:
    """Cross-referenced withdrawal date in a 'replaces' sentence MUST NOT
    be attributed to the current ruling.

    Sentence: "This Ruling replaces TR 2021/3, which is withdrawn from
    1 July 2022." The 1 July 2022 date applies to TR 2021/3, the predecessor.
    The current ruling is the REPLACEMENT, so its withdrawn_date is None.
    Sentence-aware extraction in `_extract_self_withdrawn_date` skips the
    fragment because it contains a replacement verb without a self-anchor.
    """
    html = """
    <div id="LawBody">
        <p>This Ruling replaces Taxation Ruling TR 2021/3, which is withdrawn from 1 July 2022.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date is None
    assert info.replaces == "TR 2021/3"


def test_extract_currency_self_withdrawn_clear_sentence() -> None:
    """Plain self-withdrawal sentence — no replacement verb anywhere."""
    html = """
    <div id="LawBody">
        <p>Taxation Ruling TR 2024/1 is withdrawn with effect from 31 December 2024.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2024-12-31"


def test_extract_currency_this_ruling_anchor_overrides_replacement_verb() -> None:
    """A 'this Ruling' subject means the date applies to the current doc."""
    html = """
    <div id="LawBody">
        <p>This Ruling is withdrawn from 1 January 2025.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2025-01-01"


def test_extract_currency_negative_mixed_sentence() -> None:
    """Sentence has a 'withdrawn ... date' clause AND a 'replaces' verb but
    no 'this Ruling' anchor on the withdrawn clause — date belongs to the
    referenced predecessor, not the current doc.
    """
    html = """
    <div id="LawBody">
        <p>This Ruling replaces TR 2021/3 (which was withdrawn from 1 July 2022) and applies from 1 July 2022.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date is None
    assert info.replaces == "TR 2021/3"


def test_extract_currency_replaced_by_in_alert_panel() -> None:
    """Status panel says 'Replaced by TR 98/17'."""
    html = """
    <div id="LawContent">
        <div class="alert alert-block alert-warning" data-icon="w">This document has been Withdrawn.</div>
        <div class="alert alert-block alert-warning" data-icon="w">Replaced by TR 98/17 with effect from 14 April 1994.</div>
        <div id="LawBody">
            <h3>Notice of Withdrawal</h3>
            <p>IT 2607 is withdrawn with effect from 14 April 1994.</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "1994-04-14"
    assert info.superseded_by == "TR 98/17"


def test_extract_currency_superseded_by_phrasing() -> None:
    """'superseded by TR 94/13' synonym."""
    html = """
    <div id="LawBody">
        <p>Taxation Ruling IT 2150 has been superseded by TR 94/13.</p>
        <p>IT 2150 is withdrawn with effect from 14 April 1994.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.superseded_by == "TR 94/13"
    assert info.withdrawn_date == "1994-04-14"


def test_extract_currency_withdrawn_by_sets_superseded_by() -> None:
    html = """
    <div id="LawBody">
        <p>Notice of withdrawal: TR 2024/3 was withdrawn on 5 March 2024 by TR 2025/1.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2024-03-05"
    assert info.superseded_by == "TR 2025/1"


def test_extract_currency_predecessor_withdrawn_by_is_not_self() -> None:
    html = """
    <div id="LawBody">
        <p>This Ruling replaces TR 2021/3, which was withdrawn on 5 March 2024 by TR 2025/1.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date is None
    assert info.superseded_by is None
    assert info.replaces == "TR 2021/3"


def test_extract_currency_replaces_in_alert_panel() -> None:
    """Live ruling page shows 'This Ruling, which applies from ..., replaces TR 2021/3'."""
    html = """
    <div id="LawContent">
        <div class="alert alert-block alert-warning" data-icon="w">
            This Ruling, which applies from 1 July 2022, replaces TR 2021/3.
        </div>
        <div id="LawBody">
            <h3>Ruling</h3>
            <p>The Commissioner rules ...</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info.replaces == "TR 2021/3"
    # Not withdrawn — this is the replacement ruling.
    assert info.withdrawn_date is None


def test_extract_currency_date_format_dd_slash_mm_slash_yyyy() -> None:
    """Australian DD/MM/YYYY format."""
    html = """
    <div id="LawBody">
        <p>The ruling is withdrawn with effect from 31/10/2025.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2025-10-31"


def test_extract_currency_date_format_iso() -> None:
    html = """
    <div id="LawBody">
        <p>This ruling is withdrawn with effect from 2025-10-31.</p>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "2025-10-31"


def test_extract_currency_history_table_fallback() -> None:
    """When prose form is missing, fall back to the timeline table."""
    html = """
    <div id="LawContent">
        <div id="LawBody"><p>Some unrelated text.</p></div>
        <div class="panel">
            <div class="panel-heading"><a name="LawTimeLine"></a>
                <strong>TR 2007/D10W2 - Notice of Withdrawal history</strong>
            </div>
            <div class="panel-body">
                <table>
                    <tr>
                        <td class="date-right2">7 December 2016</td>
                        <td class="main"><a href="/foo">Withdrawal</a></td>
                    </tr>
                    <tr>
                        <td class="date-right2">15 November 2023</td>
                        <td class="main"><a href="/foo">Updated withdrawal</a></td>
                    </tr>
                </table>
            </div>
        </div>
    </div>
    """
    info = extract_currency(html)
    # Latest withdrawal entry wins — 15 November 2023.
    assert info.withdrawn_date == "2023-11-15"


def test_extract_currency_title_suffix_only_uses_sentinel() -> None:
    """`(Withdrawn)` in a heading with no other signal sets the sentinel.

    Many ATO IDs/PSLAs carry their withdrawn status only in an h2 like
    ``ATO ID 2001/746 (Withdrawn)``. Without this path those docs leak
    into default searches as if they were current law.
    """
    html = """
    <div id="LawContent">
        <div id="LawBody">
            <h1>ATO Interpretative Decision</h1>
            <h2>ATO ID 2001/746 (Withdrawn)</h2>
            <p>The Commissioner's view on income tax matters.</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "0001-01-01"


def test_extract_currency_title_suffix_does_not_override_real_date() -> None:
    """When both a heading suffix and a real withdrawal date are present,
    the real date wins — the sentinel is only a last-resort fallback."""
    html = """
    <div id="LawContent">
        <div class="alert alert-block alert-warning" data-icon="w">
            This document has been Withdrawn with effect from 14 April 1994.
        </div>
        <div id="LawBody">
            <h2>ATO ID 2001/746 (Withdrawn)</h2>
            <p>Body prose.</p>
        </div>
    </div>
    """
    info = extract_currency(html)
    assert info.withdrawn_date == "1994-04-14"


def test_doc_id_parser_handles_view_htm_variants() -> None:
    """Live corpus carries pre-SPA `view.htm` URLs in three host shapes —
    all three must resolve to the same doc_id rather than slipping through
    as raw hrefs."""
    cases = [
        ("https://www.ato.gov.au/law/view/view.htm?docid=TXR/TR200416/NAT/ATO", "TXR/TR200416/NAT/ATO"),
        ("http://law.ato.gov.au/view.htm?DocID=PSR/PS19981/NAT/ATO/00001", "PSR/PS19981/NAT/ATO/00001"),
        ("http://law.ato.gov.au/atolaw/view.htm?DocID=TXR/TR200113/NAT/ATO/00001", "TXR/TR200113/NAT/ATO/00001"),
        ("http://ato.gov.au/law/view.htm?DocID=PSR/PS201112/NAT/ATO/00001", "PSR/PS201112/NAT/ATO/00001"),
    ]
    for url, expected in cases:
        assert _doc_id_from_ato_link(url) == expected, url


def test_doc_id_parser_handles_pit_query_alongside_docid() -> None:
    """The legacy `&PiT=…` archived-as-of timestamp must not block parsing —
    only the docid value is taken."""
    url = "http://law.ato.gov.au/atolaw/view.htm?Docid=JUD/2008ATC20-048/00001&PiT=99991231235958"
    assert _doc_id_from_ato_link(url) == "JUD/2008ATC20-048/00001"


def test_doc_id_parser_handles_quoted_urlencoded_value() -> None:
    """Some links wrap the doc id in URL-encoded quotes (`%22…%22`) — those
    quotes must be stripped, mirroring the existing /law/view/document path."""
    url = 'https://www.ato.gov.au/law/view/view.htm?docid=%22OPS%2FLI202520%2F00001%22'
    assert _doc_id_from_ato_link(url) == "OPS/LI202520/00001"


def test_doc_id_parser_handles_spa_fragment_form() -> None:
    """SPA-style URLs encode the doc id in the URL fragment (after `#`),
    which urlparse otherwise hides from query parsing."""
    cases = [
        ("https://www.ato.gov.au/law/#Law/table-of-contents?locid=NEM/EM200558/NAT/ATO", "NEM/EM200558/NAT/ATO"),
        ("https://www.ato.gov.au/single-page-applications/legaldatabase/#Law/table-of-contents?docid=TPA/TA20253", "TPA/TA20253"),
    ]
    for url, expected in cases:
        assert _doc_id_from_ato_link(url) == expected, url


def test_doc_id_parser_rejects_spa_category_links() -> None:
    """Trailing `?` is the SPA category-browser flag (e.g. `docid=tpa?`,
    `locid=rtf/sca?`) — these point at a category, not a specific doc, so
    they must NOT be emitted as cross-references."""
    cases = [
        "https://www.ato.gov.au/law/#Law/table-of-contents?docid=tpa?",
        "https://www.ato.gov.au/law/#Law/table-of-contents?locid=rtf/sca?",
        "https://www.ato.gov.au/law/#Law/table-of-contents?category=ZG",
        "https://www.ato.gov.au/single-page-applications/legaldatabase#Law/table-of-contents",
    ]
    for url in cases:
        assert _doc_id_from_ato_link(url) is None, url


def test_doc_id_parser_rejects_non_doc_ato_pages() -> None:
    """Marketing/info pages on ato.gov.au must not be mistaken for doc links."""
    cases = [
        "http://www.ato.gov.au",
        "https://www.ato.gov.au/About-ATO/Commitments-and-reporting/",
        "https://www.ato.gov.au/Rates/Division-7A---benchmark-interest-rate/",
        "https://www.ato.gov.au/law/view/pdf/inow/inow1234.pdf",
        "https://example.com/foo",
    ]
    for url in cases:
        assert _doc_id_from_ato_link(url) is None, url
