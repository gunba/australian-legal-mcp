"""Tests for ``human_code_for_doc_id`` — the short-citation parser.

Covers the main ATO series that the v1 rule set supports, plus unmatched
formats (legacy un-yeared rulings, consolidated/addendum suffixes) that
correctly return ``None`` for later rule-set expansion.
"""
from __future__ import annotations

import pytest

from ato_mcp.indexer.metadata import human_code_for_doc_id


@pytest.mark.parametrize(
    ("doc_id", "expected"),
    [
        # Canonical modern 4-digit-year rulings, single-digit number.
        ("TXR/TR20243/NAT/ATO/00001", "TR 2024/3"),
        # Multi-digit number.
        ("CLR/CR200117/NAT/ATO/00001", "CR 2001/17"),
        ("PRR/PR1999100/NAT/ATO/00001", "PR 1999/100"),
        # GSTR, LCR, SGR, TD, MT, FBT, TA, LCG, LI, WT — series breadth.
        ("CGR/GSTR20243/NAT/ATO/00001", "GSTR 2024/3"),
        ("COC/LCR20181/NAT/ATO/00001", "LCR 2018/1"),
        ("SGR/SGR20091/NAT/ATO/00001", "SGR 2009/1"),
        ("OPS/LI20181/NAT/ATO/00001", "LI 2018/1"),
        ("WTR/WT20091/NAT/ATO/00001", "WT 2009/1"),
        # PCG — final and draft. Draft marker renders as "/D<n>".
        ("COG/PCG20241/NAT/ATO/00001", "PCG 2024/1"),
        ("DPC/PCG2025D6/NAT/ATO/00001", "PCG 2025/D6"),
        # TR draft.
        ("DTR/TR2023D2/NAT/ATO/00001", "TR 2023/D2"),
        # LCR draft.
        ("COD/LCR2026D1/NAT/ATO/00001", "LCR 2026/D1"),
        # PS LA final + draft.
        ("PSLA/PSLA202414/NAT/ATO/00001", "PS LA 2024/14"),
        ("DPS/PSD20191/NAT/ATO/00001", "PS LA 2019/D1"),
        # ATO ID — both inner prefixes collapse to the same citation.
        ("ATOID/ATOID200114/NAT/ATO/00001", "ATO ID 2001/14"),
        ("AID/AID20011/NAT/ATO/00001", "ATO ID 2001/1"),
        ("AID/AID2001100/NAT/ATO/00001", "ATO ID 2001/100"),
        # Pre-2000 legacy 2-digit year.
        ("TXR/TR9725/NAT/ATO/00001", "TR 97/25"),
        ("CGD/TD931/NAT/ATO/00001", "TD 93/1"),
        ("CGD/TD9310/NAT/ATO/00001", "TD 93/10"),
        # Disambiguation: 4-digit-year must beat 2-digit-year so TR20081
        # renders as "TR 2008/1", not "TR 20/081".
        ("TXR/TR20081/NAT/ATO/00001", "TR 2008/1"),
    ],
)
def test_recognised_formats(doc_id: str, expected: str) -> None:
    assert human_code_for_doc_id(doc_id) == expected


@pytest.mark.parametrize(
    "doc_id",
    [
        # Legacy un-yeared IT and TD rulings — not in v1 scope.
        "ITR/IT1/NAT/ATO/00001",
        "ITR/IT117/NAT/ATO/00001",
        "CGD/TD1/NAT/ATO/00001",
        # Consolidated "EC" suffix — not handled.
        "CTR/TR2008EC5/NAT/ATO/00001",
        "CGR/GSTR2001EC8/NAT/ATO/00001",
        # Addendum / errata suffix.
        "CLR/CR20011A6/NAT/ATO/00001",
        "ITR/IT131ER/NAT/ATO/00001",
        # Draft compendium — not yet handled.
        "DCC/LCR20169DC1/NAT/ATO/00001",
        "DTC/TR200611DC2/NAT/ATO/00001",
        # Legacy un-yeared MT rulings (MT2005 = "MT 2005", not "MT 20/05").
        "MTR/MT2005/NAT/ATO/00001",
        "MTR/MT2016/NAT/ATO/00001",
        # Malformed / too few segments.
        "",
        "singleton",
        "/leading/slash",  # first segment empty, second is "leading"
        # Unknown series prefix.
        "FOO/BAR20241/NAT/ATO/00001",
    ],
)
def test_unrecognised_returns_none(doc_id: str) -> None:
    assert human_code_for_doc_id(doc_id) is None
