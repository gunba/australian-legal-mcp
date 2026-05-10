"""Docid-prefix classifier — placement of What's New entries.

The hand-maintained prefix-to-category map has been removed; ``category_for_docid``
now always returns ``Other_ATO_documents`` and ``representative_path_from_docid``
relies on the source-derived heading from the What's New page for grouping.
``category_for_record`` still routes to a real category when ``payload_path``
already names one, so corpus documents continue to land in the right bucket.
"""
from __future__ import annotations

from ato_mcp.indexer.metadata import (
    category_for_docid,
    category_for_record,
    parse_docid,
    representative_path_from_docid,
    year_for_docid,
)


def test_class_ruling_year_and_prefix() -> None:
    href = "/law/view/document?docid=CLR/CR202612/NAT/ATO/00001"
    prefix, name = parse_docid(href)
    assert prefix == "CLR"
    # Doc-type names are no longer derived in Python; the Rust stats tool
    # surfaces source-derived prefix descriptions instead.
    assert name is None
    assert category_for_docid(href) == "Other_ATO_documents"
    assert year_for_docid(href) == "2026"
    rep = representative_path_from_docid(href, title="CR 2026/12", heading="Rulings")
    assert rep[0] == "Other_ATO_documents"
    assert "Rulings" in rep
    assert "2026" in rep
    assert rep[-1] == "CR 2026/12"


def test_unknown_docid_falls_back_to_other_ato() -> None:
    href = "/law/view/document?docid=ZZZ/ZZZ0000/NAT/ATO/00001"
    assert category_for_docid(href) == "Other_ATO_documents"


def test_representative_path_falls_back_on_no_docid() -> None:
    href = "https://example.com/not-a-law-doc"
    rep = representative_path_from_docid(href, title="X", heading="Notices")
    # No docid parseable -> category defaults, heading still surfaces.
    assert rep[0] == "Other_ATO_documents"
    assert "Notices" in rep
    assert rep[-1] == "X"


def test_build_pending_record_uses_docid_classifier() -> None:
    from ato_mcp.scraper.whats_new import WhatsNewEntry, build_pending_record

    entry = WhatsNewEntry(
        href="/law/view/document?docid=CLR/CR202612/NAT/ATO/00001",
        title="CR 2026/12",
        heading="Rulings",
    )
    record = build_pending_record(entry)
    rep = record["representative_path"]
    # First segment is the catch-all bucket; heading carries the source-derived
    # grouping. The legacy ``payloads/whats_new`` bucket is not used.
    assert rep[0] == "Other_ATO_documents"
    assert "whats_new" not in rep
    assert "Rulings" in rep


def test_whats_new_payload_path_does_not_become_category() -> None:
    href = "/law/view/document?docid=TPA/TA20253/NAT/ATO/00001"
    category = category_for_record(
        href,
        "payloads/whats_new/Taxpayer_alerts/TA_2025_3/law_view_document_docid_TPA_TA20253_NAT_ATO_00001.html",
    )
    # ``payload_path`` resolves to whats_new (not a real bucket), so this
    # falls through to ``category_for_docid`` -> Other_ATO_documents.
    assert category == "Other_ATO_documents"


def test_real_payload_path_wins_over_docid_fallback() -> None:
    href = "/law/view/document?docid=CLR/CR202612/NAT/ATO/00001"
    category = category_for_record(
        href,
        "payloads/Public_rulings/CR_2026_12/law_view_document_docid_CLR_CR202612_NAT_ATO_00001.html",
    )
    # When payload_path names a real bucket, we keep it — the docid fallback
    # only fires for whats_new/Unknown payloads.
    assert category == "Public_rulings"
