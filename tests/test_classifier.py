"""Docid-prefix classifier — placement of What's New entries.

The hand-maintained prefix-to-category map has been removed: documents
without a source-derived category fall into ``Other_ATO_documents``.
``category_for_record`` still routes to a real category when
``payload_path`` already names one, so corpus documents continue to land
in the right bucket.
"""
from __future__ import annotations

from ato_mcp.indexer.metadata import (
    OTHER_CATEGORY,
    category_for_record,
    parse_docid,
    representative_path_from_docid,
    year_for_docid,
)


def test_class_ruling_year_and_prefix() -> None:
    href = "/law/view/document?docid=CLR/CR202612/NAT/ATO/00001"
    assert parse_docid(href) == "CLR"
    assert year_for_docid(href) == "2026"
    rep = representative_path_from_docid(href, title="CR 2026/12", heading="Rulings")
    assert rep[0] == OTHER_CATEGORY
    assert "Rulings" in rep
    assert "2026" in rep
    assert rep[-1] == "CR 2026/12"


def test_representative_path_falls_back_on_no_docid() -> None:
    href = "https://example.com/not-a-law-doc"
    rep = representative_path_from_docid(href, title="X", heading="Notices")
    # No docid parseable -> category defaults, heading still surfaces.
    assert rep[0] == OTHER_CATEGORY
    assert "Notices" in rep
    assert rep[-1] == "X"


def test_build_pending_record_uses_docid_classifier() -> None:
    # build_pending_record was a Python helper in the now-deleted scraper
    # module; the equivalent live in the Rust 'ato-mcp scrape-diff'
    # subcommand. Skip the assertion since the Python pipeline is gone.
    import pytest
    pytest.skip("build_pending_record moved to the Rust ato-mcp scrape-diff CLI")


def test_whats_new_payload_path_does_not_become_category() -> None:
    href = "/law/view/document?docid=TPA/TA20253/NAT/ATO/00001"
    category = category_for_record(
        href,
        "payloads/whats_new/Taxpayer_alerts/TA_2025_3/law_view_document_docid_TPA_TA20253_NAT_ATO_00001.html",
    )
    # ``payload_path`` resolves to whats_new (not a real bucket), so this
    # falls through to the catch-all category.
    assert category == OTHER_CATEGORY


def test_real_payload_path_wins_over_docid_fallback() -> None:
    href = "/law/view/document?docid=CLR/CR202612/NAT/ATO/00001"
    category = category_for_record(
        href,
        "payloads/Public_rulings/CR_2026_12/law_view_document_docid_CLR_CR202612_NAT_ATO_00001.html",
    )
    # When payload_path names a real bucket, we keep it — the docid fallback
    # only fires for whats_new/Unknown payloads.
    assert category == "Public_rulings"
