"""Chunker invariants."""
from __future__ import annotations

from ato_mcp.indexer.chunk import approx_tokens, chunk_html, strip_title_prefix


def test_chunk_keeps_headings_in_path() -> None:
    html = """
<h1>ITAA 1997</h1>
<p>Some intro.</p>
<h2>Division 355</h2>
<p>Research and development tax incentive.</p>
<h3>Section 355-25</h3>
<p>Core R&amp;D activities definition.</p>
<p>A core R&amp;D activity is experimental.</p>
"""
    chunks = chunk_html(html, root_title="ITAA 1997")
    # Every chunk must reflect the heading it lives under.
    assert any("Division 355" in c.heading_path for c in chunks)
    assert any("Section 355-25" in c.heading_path for c in chunks)
    # No chunk exceeds the max-token budget.
    for c in chunks:
        assert approx_tokens(c.text) <= 1200


def test_chunk_splits_oversize_section() -> None:
    big = "\n".join([f"<p>para {' '.join(['alpha'] * 40)}</p>" for _ in range(60)])
    html = f"<h1>Heading</h1>{big}"
    chunks = chunk_html(html, max_tokens=200, overlap_tokens=40)
    assert len(chunks) > 1
    for c in chunks:
        # 200 max + overlap bridge + small slack
        assert approx_tokens(c.text) <= 280


def test_chunk_stable_under_small_edit() -> None:
    html1 = "<h1>Title</h1><p>" + "Alpha beta gamma delta. " * 40 + "</p><h2>Section</h2><p>Content.</p>"
    html2 = html1.replace("delta", "deltax", 1)  # one-word change
    c1 = [c.text for c in chunk_html(html1)]
    c2 = [c.text for c in chunk_html(html2)]
    changed = sum(1 for a, b in zip(c1, c2) if a != b)
    assert changed <= 2, "a one-word change should flip at most two chunks"


def test_chunk_empty_returns_no_chunks() -> None:
    assert chunk_html("") == []
    assert chunk_html("   \n  ") == []


def test_strip_title_prefix_drops_root_and_components() -> None:
    title = ("Taxation Ruling — TR 2024/3 — Income tax: deductibility of "
             "self-education expenses incurred by an individual")
    hp = (f"{title} › Taxation Ruling › TR 2024/3 › Ruling")
    assert strip_title_prefix(hp) == "Ruling"
    hp_with_subtree = f"{title} › Taxation Ruling › TR 2024/3 › Ruling › Footnotes"
    assert strip_title_prefix(hp_with_subtree) == "Ruling › Footnotes"


def test_strip_title_prefix_collapses_whitespace_in_components() -> None:
    # Source title sometimes has double spaces around colons; the path
    # component is single-spaced. The dedup must still treat them as equal.
    title = "Taxation Ruling — TR 2024/3 — Income tax:  deductibility"
    hp = f"{title} › Taxation Ruling › TR 2024/3 › Income tax: deductibility"
    assert strip_title_prefix(hp) == ""


def test_strip_title_prefix_drops_url_front_segment() -> None:
    hp = ("/law/view/document?docid=PAC/19970038/25-25 › "
          "Income Tax Assessment Act 1997 › Note:")
    assert strip_title_prefix(hp) == "Note:"


def test_strip_title_prefix_keeps_real_body_headings() -> None:
    hp = "Income Tax Assessment Act 1997 › Division 355 › Section 355-25"
    assert strip_title_prefix(hp) == "Division 355 › Section 355-25"


def test_chunk_pops_same_level_siblings() -> None:
    # Two h2s in a row should be siblings, not nested. The chunker used to
    # only cap stack depth, leaving "## A → ## B" as A › B.
    html = """
<h2>Section A</h2>
<p>Body of A.</p>
<h2>Section B</h2>
<p>Body of B.</p>
<h3>Sub of B</h3>
<p>Body of sub.</p>
"""
    chunks = chunk_html(html)
    paths = [c.heading_path for c in chunks]
    assert "Section A" in paths
    assert "Section B" in paths
    assert "Section B › Sub of B" in paths
    # The buggy pre-fix path would have been "Section A › Section B".
    assert not any(p.startswith("Section A › Section B") for p in paths)


def test_chunk_handles_skipped_heading_levels() -> None:
    html = """
<h1>Top</h1>
<p>intro.</p>
<h3>Deep heading</h3>
<p>Body.</p>
"""
    chunks = chunk_html(html)
    # Going from h1 directly to h3 should not invent placeholder ancestors.
    paths = [c.heading_path for c in chunks]
    assert any("Top › Deep heading" == p for p in paths)


def test_chunk_h5_siblings_no_false_nesting() -> None:
    # Mirrors the ATO ITAA 1997 layout: a single h1 then a flat run of h5
    # "Note" annotations.
    html = """
<h1>Income Tax Assessment Act 1997</h1>
<p>intro paragraph.</p>
<h5>Note 1:</h5>
<p>note one body.</p>
<h5>Note 2:</h5>
<p>note two body.</p>
<h5>Note 3:</h5>
<p>note three body.</p>
"""
    chunks = chunk_html(html, root_title="Income Tax Assessment Act 1997")
    paths = [c.heading_path for c in chunks]
    assert "Note 1:" in paths
    assert "Note 2:" in paths
    assert "Note 3:" in paths
    # No false nesting like "Note 1: › Note 2:".
    for p in paths:
        assert " › Note " not in p, f"falsely nested: {p!r}"


def test_chunk_emits_clean_heading_path() -> None:
    html = """
<h1>Taxation Ruling</h1>
<h2>TR 2024/3</h2>
<h3>Subject heading</h3>
<p>intro paragraph.</p>
<h2>Ruling</h2>
<p>Body content for the ruling section.</p>
"""
    title = "Taxation Ruling — TR 2024/3 — Subject heading"
    chunks = chunk_html(html, root_title=title)
    paths = [c.heading_path for c in chunks]
    # Front-matter title segments should not appear in any chunk's path.
    for p in paths:
        for component in ("Taxation Ruling", "TR 2024/3", "Subject heading"):
            assert not p.startswith(component), f"front-matter echo: {p!r}"
    # Real body section is preserved.
    assert "Ruling" in paths


def test_chunk_text_is_plain_and_definition_markers_are_build_only() -> None:
    html = """
<h1>Definitions</h1>
<p><strong><em>corporate tax rate</em></strong> means the rate of tax.</p>
<p>See <a data-doc-id="PAC/19970038/995-1">section 995-1</a>.</p>
<p><span data-asset-ref="ato-image://DOC/0">[image: Formula diagram]</span></p>
"""
    chunks = chunk_html(html)
    text = "\n\n".join(chunk.text for chunk in chunks)
    definition_text = "\n\n".join(chunk.definition_text or "" for chunk in chunks)

    assert "***" not in text
    assert "data-doc-id" not in text
    assert "ato-image://" not in text
    assert "corporate tax rate means the rate of tax." in text
    assert "section 995-1" in text
    assert "[image: Formula diagram]" in text
    assert "***corporate tax rate*** means the rate of tax." in definition_text
