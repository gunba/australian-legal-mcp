"""Docid-prefix classifier — placement of What's New entries."""
from __future__ import annotations

from ato_mcp.indexer.metadata import (
    category_for_docid,
    representative_path_from_docid,
    year_for_docid,
)


def test_class_ruling_goes_to_public_rulings() -> None:
    href = "/law/view/document?docid=CLR/CR202612/NAT/ATO/00001"
    assert category_for_docid(href) == "Public_rulings"
    assert year_for_docid(href) == "2026"
    rep = representative_path_from_docid(href, title="CR 2026/12", heading="Rulings")
    assert rep[0] == "Public_rulings"
    assert "2026" in rep
    assert rep[-1] == "CR 2026/12"


def test_legislative_instrument_goes_to_legislation() -> None:
    href = "/law/view/document?docid=OPS/LI202615/00001"
    assert category_for_docid(href) == "Legislation_and_supporting_material"


def test_taxation_ruling_update_goes_to_public_rulings() -> None:
    # TXR prefix — a re-issued/updated Taxation Ruling.
    href = "/law/view/document?docid=TXR/TR20171A1/NAT/ATO/00001"
    assert category_for_docid(href) == "Public_rulings"


def test_practice_statement_goes_to_law_admin() -> None:
    href = "/law/view/document?docid=PSR/PS201114/NAT/ATO/00001"
    assert category_for_docid(href) == "Law_administration_practice_statements"


def test_unknown_prefix_falls_back_to_other_ato() -> None:
    href = "/law/view/document?docid=ZZZ/ZZZ0000/NAT/ATO/00001"
    assert category_for_docid(href) == "Other_ATO_documents"


def test_representative_path_falls_back_on_no_docid() -> None:
    href = "https://example.com/not-a-law-doc"
    rep = representative_path_from_docid(href, title="X", heading="Notices")
    # No docid parseable -> category defaults, uses heading.
    assert rep[0] == "Other_ATO_documents"
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
    # First segment must be the real category, NOT 'whats_new'.
    assert rep[0] == "Public_rulings"
    assert "whats_new" not in rep
