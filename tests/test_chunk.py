"""Chunker invariants for the boundary-aware, inline-rendering chunker."""
from __future__ import annotations

from ato_mcp.indexer.chunk import EMBED_MAX_TOKENS, approx_tokens, chunk_html


def test_chunk_empty_returns_no_chunks() -> None:
    assert chunk_html("") == []
    assert chunk_html("   \n  ") == []


def test_chunk_inline_renders_headings() -> None:
    html = """
<h1>Top</h1>
<p>intro.</p>
<h2>Sub</h2>
<p>body.</p>
<h3>Inner</h3>
<p>more.</p>
"""
    chunks = chunk_html(html)
    text = "\n".join(c.text for c in chunks)
    assert "# Top" in text
    assert "## Sub" in text
    assert "### Inner" in text


def test_chunk_root_title_heading_is_dropped() -> None:
    """When root_title is provided, an h1 echoing the title gets suppressed
    so the chunk doesn't repeat the front-matter heading."""
    html = "<h1>Income Tax Assessment Act 1997</h1><p>body content here.</p>"
    chunks = chunk_html(html, root_title="Income Tax Assessment Act 1997")
    text = "\n".join(c.text for c in chunks)
    assert "# Income Tax Assessment Act 1997" not in text
    assert "body content here" in text


def test_chunk_inline_renders_emphasis() -> None:
    html = "<p>Plain <strong>bold</strong> and <em>italic</em> text.</p>"
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert "**bold**" in text
    assert "*italic*" in text


def test_chunk_nested_strong_em_emits_clean_triple_asterisk_marker() -> None:
    """Regression: ATO dictionary HTML has ``<strong><em>TERM</em>\\n</strong>``
    with trailing whitespace inside the strong wrapper. Without stripping, the
    chunker emitted ``***TERM* **`` (final asterisks separated by whitespace)
    and the definition_text path double-wrapped to ``****TERM****`` (4
    asterisks each side). Neither matched the ``\\*\\*\\*…\\*\\*\\*`` term
    regex in ``definitions.py`` and the corpus shipped with 13 definitions
    instead of thousands. Both renders must produce ``***TERM***`` cleanly so
    the extractor finds every dictionary entry."""
    html = """<p><strong><em>effective life</em>
    </strong>
    <br>has the meaning given by subsection 40-95(7).</p>"""
    chunks = chunk_html(html)
    assert chunks
    assert "***effective life***" in chunks[0].text
    # And the dedicated definition_text path (the one the extractor reads)
    # carries the same clean marker — pack-side test confirms.
    defn = chunks[0].definition_text or chunks[0].text
    assert "***effective life***" in defn


def test_chunk_blockquote_emits_quote_prefix() -> None:
    html = "<blockquote>Court found that the entity was a resident.</blockquote>"
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert text.startswith("> ")
    assert "resident" in text


def test_chunk_dt_dd_pair_combines_to_one_block() -> None:
    """Adjacent <dt>+<dd> render as `**term**\\nbody` and stay packed together."""
    html = """
<dl>
<dt>resident</dt>
<dd>means a person residing in Australia.</dd>
<dt>non-resident</dt>
<dd>means anyone who is not a resident.</dd>
</dl>
"""
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert "**resident**" in text
    assert "means a person residing in Australia." in text
    # term and body must appear adjacently, not split across blocks.
    idx_term = text.index("**resident**")
    idx_body = text.index("means a person residing in Australia.")
    assert 0 < idx_body - idx_term < 60


def test_chunk_pre_emits_fence() -> None:
    html = "<pre>line one\nline two</pre>"
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert text.startswith("```")
    assert text.endswith("```")
    assert "line one" in text
    assert "line two" in text


def test_chunk_li_emits_dash_prefix() -> None:
    html = "<ul><li>first</li><li>second</li></ul>"
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert "- first" in text
    assert "- second" in text


def test_chunk_packs_within_max_tokens() -> None:
    big = "\n".join([f"<p>para {' '.join(['alpha'] * 40)}</p>" for _ in range(60)])
    html = f"<h1>Heading</h1>{big}"
    chunks = chunk_html(html, max_tokens=200)
    assert len(chunks) > 1
    for c in chunks:
        assert approx_tokens(c.text) <= 200


def test_chunk_packs_small_blocks_without_rounding_drift() -> None:
    """Regression: many small blocks must not produce a chunk over max_tokens.

    The packer previously summed per-block `approx_tokens` (each truncated to
    int), so a chunk of N blocks could land up to N tokens over the cap
    relative to `approx_tokens` of the joined text. Real-world docs hit this
    with 5–30 tokens of drift. Construct a worst-case input where every
    block loses a fractional token to truncation.
    """
    # 30 blocks × 4 words each: per-block approx_tokens=int(4*1.3)=5,
    # sum=150; joined 120 words → approx_tokens=int(120*1.3)=156. With the
    # bug, all 30 blocks pack into one 156-token chunk under max=155.
    html = "".join(f"<p>{' '.join(['word'] * 4)}</p>" for _ in range(30))
    chunks = chunk_html(html, max_tokens=155)
    for c in chunks:
        assert approx_tokens(c.text) <= 155, (
            f"chunk over cap: tokens={approx_tokens(c.text)} cap=155"
        )


def test_chunk_no_chunk_exceeds_embedder_limit() -> None:
    """Default chunker output must never exceed the embedder's truncation point."""
    html = """
<h1>ITAA 1997</h1>
<p>Some intro paragraph with a few words.</p>
<h2>Division 355</h2>
<p>Research and development tax incentive.</p>
<h3>Section 355-25</h3>
<p>Core R&amp;D activities definition.</p>
<p>A core R&amp;D activity is experimental.</p>
"""
    chunks = chunk_html(html, root_title="ITAA 1997")
    for c in chunks:
        assert approx_tokens(c.text) <= EMBED_MAX_TOKENS


def test_chunk_oversize_table_split_keeps_rows_whole() -> None:
    """A single <table> exceeding max_tokens is split between <tr> rows. Each
    row stays intact (cells joined by ' | ')."""
    rows = "".join(
        f"<tr><td>row {i}</td><td>{' '.join(['word'] * 30)}</td></tr>"
        for i in range(40)
    )
    html = f"<table>{rows}</table>"
    chunks = chunk_html(html, max_tokens=200)
    assert len(chunks) > 1
    for c in chunks:
        # Each chunk should be a sequence of complete rows ('a | b' joined by \n).
        for line in c.text.split("\n"):
            assert " | " in line, line


def test_chunk_anchor_marker_emitted_when_referenced() -> None:
    """<a name="X"> emits [anchor:X] only when something <a href="#X"> in
    the same doc references it."""
    html = """
<p>See <a href="#P5">paragraph 5</a> for details.</p>
<p><a name="P5"></a>This is paragraph five.</p>
"""
    chunks = chunk_html(html)
    text = "\n".join(c.text for c in chunks)
    assert "[anchor:P5]" in text


def test_chunk_anchor_marker_skipped_when_unreferenced() -> None:
    """An <a name="X"> that's not referenced anywhere emits no marker."""
    html = "<p>Unrelated body.</p><p><a name=\"unused\"></a>more body.</p>"
    chunks = chunk_html(html)
    text = "\n".join(c.text for c in chunks)
    assert "[anchor:unused]" not in text


def test_chunk_text_is_plain_no_html_attributes_leak() -> None:
    """chunk.text carries no raw HTML attribute syntax — data-doc-id and
    asset alt-text fall back to inline `[doc:X]` / `[asset:X]` markers.
    The dictionary-term marker `***term***` is allowed in chunk.text (it
    falls out naturally from the inline strong+em rendering) and is what
    the definition extractor reads."""
    html = """
<h1>Definitions</h1>
<p><strong><em>corporate tax rate</em></strong> means the rate of tax.</p>
<p>See <a data-doc-id="PAC/19970038/995-1">section 995-1</a>.</p>
<p><span data-asset-ref="ato-image://DOC/0">[image: Formula diagram]</span></p>
"""
    chunks = chunk_html(html)
    text = "\n\n".join(chunk.text for chunk in chunks)

    assert "data-doc-id" not in text
    assert "section 995-1 [doc:PAC/19970038/995-1]" in text
    assert "[asset:ato-image://DOC/0]" in text
    assert "[image: Formula diagram]" not in text
    assert "Formula diagram" not in text
    assert "***corporate tax rate***" in text


def test_br_newline_survives_normalisation() -> None:
    html = "<p>365<br>Number of days in reference period</p>"
    chunks = chunk_html(html)
    assert chunks
    assert "365\nNumber of days in reference period" in chunks[0].text


def test_doc_id_link_emits_marker() -> None:
    html = '<p>see <a data-doc-id="TXR/TR921/NAT/ATO">TR 92/1</a> for detail</p>'
    chunks = chunk_html(html)
    assert chunks
    assert "TR 92/1 [doc:TXR/TR921/NAT/ATO]" in chunks[0].text
    assert "for detail" in chunks[0].text


def test_doc_id_link_with_pit_emits_versioned_marker() -> None:
    """An <a data-doc-id="X" data-pit="Y"> emits '[doc:X@Y]'."""
    html = (
        '<p>see <a data-doc-id="TXR/TR967/NAT/ATO/00001" '
        'data-pit="19960320000001">Original ruling</a></p>'
    )
    chunks = chunk_html(html)
    assert chunks
    assert "Original ruling [doc:TXR/TR967/NAT/ATO/00001@19960320000001]" in chunks[0].text


def test_doc_id_link_with_view_emits_view_qualifier() -> None:
    """ATO `db=HISTFT` URLs render the amendment-trail view of a doc.
    Preserve the qualifier in the inline marker so an agent reading the
    chunk knows the cross-reference pointed at the alternative surface,
    not the live text."""
    html = (
        '<p>see history of '
        '<a data-doc-id="PAC/19970038/Pt3-6" data-view="HISTFT">Part 3-6</a>'
        '</p>'
    )
    chunks = chunk_html(html)
    assert chunks
    assert "Part 3-6 [doc:PAC/19970038/Pt3-6 view=HISTFT]" in chunks[0].text


def test_doc_id_link_with_view_via_href_fallback() -> None:
    """`<a href>` containing `db=HISTFT` falls through the chunker's
    href-parsing path and the qualifier still survives."""
    html = (
        '<p><a href="https://www.ato.gov.au/law/view/document?LocID=PAC%2F19970038%2FPt3-6&db=HISTFT&stylesheet=HIST">'
        "Pt 3-6 history</a></p>"
    )
    chunks = chunk_html(html)
    assert chunks
    assert "Pt 3-6 history [doc:PAC/19970038/Pt3-6 view=HISTFT]" in chunks[0].text


def test_doc_id_link_emits_marker_via_href_fallback() -> None:
    html = (
        '<p><a href="https://www.ato.gov.au/law/view/document?docid=TXR/TR20243/NAT/ATO/00001">'
        "TR 2024/3</a></p>"
    )
    chunks = chunk_html(html)
    assert chunks
    assert "TR 2024/3 [doc:TXR/TR20243/NAT/ATO/00001]" in chunks[0].text


def test_external_link_does_not_emit_marker() -> None:
    html = '<p>see <a href="https://example.com/foo">this page</a> for detail</p>'
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert "this page" in text
    assert "[doc:" not in text


def test_asset_ref_emits_marker_only() -> None:
    html = (
        "<p>caption text "
        '<span data-asset-ref="ato-image://JUD/foo/0" data-media-type="image/gif">'
        "[image: Annexure A]</span>"
        " trailing</p>"
    )
    chunks = chunk_html(html)
    assert chunks
    text = chunks[0].text
    assert "[asset:ato-image://JUD/foo/0]" in text
    assert "[image: Annexure A]" not in text
    assert "Annexure A" not in text
    assert "caption text" in text
    assert "trailing" in text


def test_table_with_br_preserves_two_line_cell() -> None:
    html = (
        "<p>by this fraction:</p>"
        "<table><tbody><tr>"
        "<td></td>"
        "<td><u>365</u><br>Number of days in reference period</td>"
        "<td></td>"
        "</tr></tbody></table>"
    )
    chunks = chunk_html(html)
    assert chunks
    text = "\n".join(c.text for c in chunks)
    assert "365\nNumber of days in reference period" in text
