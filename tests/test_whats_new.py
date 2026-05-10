"""Tests for ``ato_mcp.scraper.whats_new.normalize_doc_href``."""
from __future__ import annotations

from ato_mcp.scraper.whats_new import normalize_doc_href


def test_normalize_strips_host_and_keeps_docid() -> None:
    assert (
        normalize_doc_href("https://www.ato.gov.au/law/view/document?docid=ABC")
        == "/law/view/document?docid=ABC"
    )


def test_normalize_returns_relative_form_unchanged() -> None:
    assert (
        normalize_doc_href("/law/view/document?docid=TXR/TR20243/NAT/ATO/00001")
        == "/law/view/document?docid=TXR/TR20243/NAT/ATO/00001"
    )


def test_normalize_handles_empty_input() -> None:
    assert normalize_doc_href("") == ""


def test_normalize_pit_query_becomes_at_suffix() -> None:
    """A ``?PiT=<timestamp>`` query is folded into the docid as ``@<PiT>``."""
    assert (
        normalize_doc_href(
            "https://www.ato.gov.au/law/view/document?docid=TXR/TR967/NAT/ATO/00001&PiT=19960320000001"
        )
        == "/law/view/document?docid=TXR/TR967/NAT/ATO/00001@19960320000001"
    )


def test_normalize_pit_uppercase_query_param() -> None:
    """PiT query parameter lookup is case-insensitive."""
    assert (
        normalize_doc_href(
            "/law/view/document?docid=ABC&pit=20120418000001"
        )
        == "/law/view/document?docid=ABC@20120418000001"
    )


def test_normalize_pit_empty_value_is_ignored() -> None:
    """Empty PiT value falls back to the no-suffix canonical form."""
    assert (
        normalize_doc_href("/law/view/document?docid=ABC&PiT=")
        == "/law/view/document?docid=ABC"
    )
