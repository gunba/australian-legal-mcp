"""Tests for the template-based rule engine (v5 output: title + date)."""
from __future__ import annotations

import pytest

from ato_mcp.indexer.rules import (
    RuleInputs,
    Template,
    _compose_from_em_front_matter,
    classify,
    derive_metadata,
)


# ---------------------------------------------------------------------------
# OFFICIAL_PUB — rulings, PCG, TA, PS LA, ATO ID


@pytest.mark.parametrize(
    ("doc_id", "headings", "expected_prefix", "expected_year"),
    [
        ("TXR/TR20243/NAT/ATO/00001",
         ("Taxation Ruling", "TR 2024/3", "R&D tax incentive"),
         "TR 2024/3", "2024"),
        ("CLR/CR20171/NAT/ATO/00001",
         ("Class Ruling", "CR 2017/1", "Income tax: demerger"),
         "CR 2017/1", "2017"),
        ("DPC/PCG2025D6/NAT/ATO/00001",
         ("Practical Compliance Guideline", "PCG 2025/D6", "Draft topic"),
         "PCG 2025/D6", "2025"),
        ("TPA/TA20151/NAT/ATO/00001",
         ("Taxpayer Alert", "TA 2015/1", "Amendment history"),
         "TA 2015/1", "2015"),
        ("PSR/PS20134/NAT/ATO/00001",
         ("Practice Statement Law Administration", "PS LA 2013/4", "STATEMENT"),
         "PS LA 2013/4", "2013"),
        ("AID/AID200258/NAT/ATO/00001",
         ("ATO Interpretative Decision", "ATO ID 2002/58", "Income Tax"),
         "ATO ID 2002/58", "2002"),
        ("COC/LCR2019EC2/NAT/ATO/00001",
         ("Ruling Compendium", "LCR 2019/2EC", "Compendium"),
         "LCR 2019/2EC", "2019"),
    ],
)
def test_official_pub_template(doc_id, headings, expected_prefix, expected_year):
    ins = RuleInputs(doc_id=doc_id, headings=headings)
    assert classify(ins) == Template.OFFICIAL_PUB
    d = derive_metadata(ins)
    assert d.title is not None
    assert d.title.startswith(expected_prefix)
    assert d.date is not None and d.date.startswith(expected_year)


def test_official_pub_title_includes_subtitle():
    ins = RuleInputs(
        doc_id="TXR/TR20243/NAT/ATO/00001",
        headings=("Taxation Ruling", "TR 2024/3", "R&D tax incentive"),
    )
    d = derive_metadata(ins)
    assert d.title == "TR 2024/3 — R&D tax incentive"


def test_official_pub_withdrawn_marker_preserved():
    ins = RuleInputs(
        doc_id="AID/AID200258/NAT/ATO/00001",
        headings=(
            "ATO Interpretative Decision",
            "ATO ID 2002/58 (Withdrawn)",
            "Income Tax",
        ),
    )
    d = derive_metadata(ins)
    assert "(Withdrawn)" in (d.title or "")


def test_official_pub_precise_date_wins():
    ins = RuleInputs(
        doc_id="TXR/TR20243/NAT/ATO/00001",
        headings=("Taxation Ruling", "TR 2024/3", "Ruling"),
        body_head="Date of issue: 3 July 2024\n\nBody follows...",
    )
    d = derive_metadata(ins)
    assert d.date == "2024-07-03"


# ---------------------------------------------------------------------------
# DIS — Decision Impact Statement


def test_dis_title_prefixes_case_name():
    ins = RuleInputs(
        doc_id="LIT/ICD_NSD1162of2022/00001",
        headings=("Decision Impact Statement", "Commissioner of Taxation v Wood"),
        category="Decision_impact_statements",
    )
    assert classify(ins) == Template.DIS
    d = derive_metadata(ins)
    assert d.title == "DIS: Commissioner of Taxation v Wood"


# ---------------------------------------------------------------------------
# CASE_H1 — court case with name in h1


def test_case_name_in_h1():
    ins = RuleInputs(
        doc_id="JUD/2008_AATA934/00002",
        headings=("PepsiCo Inc v Commissioner of Taxation",),
        category="Cases",
    )
    assert classify(ins) == Template.CASE_H1
    d = derive_metadata(ins)
    assert d.title == "PepsiCo Inc v Commissioner of Taxation"


def test_case_neutral_citation_in_h1():
    ins = RuleInputs(
        doc_id="JUD/2024HCA41/00001",
        headings=("[2024] HCA 41",),
        category="Cases",
    )
    d = derive_metadata(ins)
    assert d.title == "[2024] HCA 41"
    assert d.date is not None and d.date.startswith("2024")


def test_case_body_cite_does_not_hijack_year():
    ins = RuleInputs(
        doc_id="JUD/somedocid/00001",
        headings=("PepsiCo Inc v Commissioner of Taxation",),
        body_head="See Smith v Commissioner [1999] HCA 12. Filed 2024.",
        category="Cases",
    )
    d = derive_metadata(ins)
    assert d.title == "PepsiCo Inc v Commissioner of Taxation"


def test_case_name_with_parenthesised_qualifier():
    """Party name with '(NZ)' and embedded newlines — decomposed heading
    shape still matches NAME_V_NAME and yields a clean title."""
    ins = RuleInputs(
        doc_id="JUD/11ATR171/00001",
        headings=(
            "COURT\nOF APPEAL OF NEW ZEALAND",
            "COMMISSIONER OF INLAND REVENUE (NZ) v\nHANNIGAN and LEVET",
            "WOODHOUSE, Richardson and McMullin JJ",
        ),
        category="Cases",
    )
    d = derive_metadata(ins)
    assert d.title == "COMMISSIONER OF INLAND REVENUE (NZ) v HANNIGAN and LEVET"


# ---------------------------------------------------------------------------
# ACT — legislation Act title


def test_act_title_is_the_title():
    ins = RuleInputs(
        doc_id="PAC/19970038_355-25/00001",
        headings=("Income Tax Assessment Act 1997",),
        category="Legislation_and_supporting_material",
    )
    assert classify(ins) == Template.ACT
    d = derive_metadata(ins)
    assert d.title == "Income Tax Assessment Act 1997"
    assert d.date == "1997-01-01"


# ---------------------------------------------------------------------------
# LEGISLATION_SECTION — PAC / REG docids


def test_legislation_section_pac_section():
    ins = RuleInputs(
        doc_id="PAC/19210026/1",
        headings=("EXCISE TARIFF ACT 1921",),
        category="Legislation_and_supporting_material",
    )
    assert classify(ins) == Template.LEGISLATION_SECTION
    d = derive_metadata(ins)
    assert d.title == "EXCISE TARIFF ACT 1921 s 1"
    assert d.date == "1921-01-01"


def test_legislation_section_reg_number():
    ins = RuleInputs(
        doc_id="REG/19560090/10",
        headings=("Customs (Prohibited Imports) Regulations 1956",),
        category="Legislation_and_supporting_material",
    )
    d = derive_metadata(ins)
    assert d.title == "Customs (Prohibited Imports) Regulations 1956 reg 10"


# ---------------------------------------------------------------------------
# HIST_CASE — JUD/*YYYY*REPORT/...


def test_historical_case_name_from_body():
    ins = RuleInputs(
        doc_id="JUD/*1881*17chd746/00001",
        headings=(),
        body_head="*## Ex parte Walton, In re Levy* | **(1881) 17 Ch.D. 746** |",
        category="Cases",
    )
    assert classify(ins) == Template.HIST_CASE
    d = derive_metadata(ins)
    assert d.title == "Ex parte Walton, In re Levy"
    assert d.date == "1881-01-01"


# ---------------------------------------------------------------------------
# Un-slashed legacy rulings


def test_it_legacy_unslashed_citation():
    ins = RuleInputs(
        doc_id="ITR/IT1/NAT/ATO/00001",
        headings=("Taxation Ruling", "IT 1", "Taxation Ruling system: explanation and status"),
        category="Public_rulings",
    )
    assert classify(ins) == Template.OFFICIAL_PUB
    d = derive_metadata(ins)
    assert d.title == "IT 1 — Taxation Ruling system: explanation and status"


# ---------------------------------------------------------------------------
# EPA — edited private advice


def test_epa_auth_number_as_title():
    ins = RuleInputs(
        doc_id="EV/1012378745518",
        headings=(),
        category="Edited_private_advice",
    )
    assert classify(ins) == Template.EPA
    d = derive_metadata(ins)
    assert d.title == "EV 1012378745518"


def test_epa_date_of_advice_from_body():
    ins = RuleInputs(
        doc_id="EV/1051375298526",
        headings=(),
        body_head="**Date of advice: 22 May 2018** | ...",
        category="Edited_private_advice",
    )
    d = derive_metadata(ins)
    assert d.date == "2018-05-22"


# ---------------------------------------------------------------------------
# SMSFRB


def test_smsfrb_citation_in_h3():
    ins = RuleInputs(
        doc_id="SRB/SRB20201/NAT/ATO",
        headings=("SMSF Regulator's Bulletin", "Appendix", "SMSFRB 2020/1"),
        category="SMSF_Regulator_s_Bulletins",
    )
    assert classify(ins) == Template.SMSFRB
    d = derive_metadata(ins)
    assert d.title is not None
    assert d.title.startswith("SMSFRB 2020/1")
    assert d.date == "2020-01-01"


# ---------------------------------------------------------------------------
# Universal fallback — every doc gets a title


def test_universal_fallback_populates_title():
    ins = RuleInputs(
        doc_id="NOTAPREFIX/XYZ/NAT/ATO/00001",
        headings=(),
        category="Unknown",
    )
    d = derive_metadata(ins)
    assert d.title == "NOTAPREFIX XYZ"


def test_unmapped_prefix_with_h1_uses_h1_h2_h3_composition():
    """Docs from prefixes the engine has no specific template for (NEM/EXN/
    RTF/ESI/...) still produce readable titles when their HTML carries an
    h1 plus h2 (and h3). The composition mirrors the ruling format
    ``<h1> — <h2> — <h3>`` and replaces the docid-derived form."""
    ins = RuleInputs(
        doc_id="NEM/EM200615/00001",
        headings=(
            "Explanatory Memorandum",
            "EM 2006/15",
            "Tax Laws Amendment (Loss Recoupment Rules) Bill 2005",
        ),
        heading_levels=(1, 2, 3),
        body_head="This Bill amends the Income Tax Assessment Act 1997...",
    )
    d = derive_metadata(ins)
    assert d.title == (
        "Explanatory Memorandum — EM 2006/15 — "
        "Tax Laws Amendment (Loss Recoupment Rules) Bill 2005"
    )
    assert d.title is not None and not d.title.startswith("NEM ")


def test_unmapped_prefix_h1_only_still_composes():
    """When only h1 is present, the title is just that h1 — better than
    the bare docid form even without h2/h3."""
    ins = RuleInputs(
        doc_id="ELD/EBL199702/00001",
        headings=("EBL 1997/2",),
        heading_levels=(1,),
    )
    d = derive_metadata(ins)
    assert d.title == "EBL 1997/2"


def test_unmapped_prefix_no_h1_falls_back_to_docid():
    """Subsidiary EM chapters with empty/missing h1 keep the deterministic
    docid form rather than drifting onto a generic body heading."""
    ins = RuleInputs(
        doc_id="NEM/EM970014/00002",
        headings=("", "Overview", "Summary of the amendments"),
        heading_levels=(2, 3, 3),
    )
    d = derive_metadata(ins)
    assert d.title == "NEM EM970014"


def test_em_section_page_uses_front_matter_composition():
    """EM section pages compose <phrase> — <ref> — <body h2> from the
    Lawfront front-matter even though they have no h1 of their own."""
    ins = RuleInputs(
        doc_id="NEM/EM201018/NAT/ATO/00003",
        headings=(
            "Chapter 1 - Introduction to the new R&D tax incentive",
            "Outline of chapter",
        ),
        heading_levels=(2, 3),
        front_matter_chamber="House of Representatives",
        front_matter_refs=(
            "Tax Laws Amendment (Research and Development) Bill 2010",
        ),
        front_matter_phrase="Explanatory Memorandum",
    )
    d = derive_metadata(ins)
    assert d.title == (
        "Explanatory Memorandum — "
        "Tax Laws Amendment (Research and Development) Bill 2010 — "
        "Chapter 1 - Introduction to the new R&D tax incentive"
    )


def test_em_explanatory_statement_redundancy_collapsed():
    """Regulation Explanatory Statements often have the body h2 == phrase
    ("Explanatory Statement"). The composer drops the redundant section."""
    ins = RuleInputs(
        doc_id="EXN/EN2000109/NAT/ATO/00001",
        headings=("Explanatory Statement",),
        heading_levels=(2,),
        front_matter_refs=(
            "Taxation Administration Amendment Regulations 2000 (No. 2)",
        ),
        front_matter_phrase="Explanatory Statement",
    )
    d = derive_metadata(ins)
    assert d.title == (
        "Explanatory Statement — "
        "Taxation Administration Amendment Regulations 2000 (No. 2)"
    )


def test_em_no_phrase_falls_through_to_h1_composer():
    """No front-matter phrase means the EM composer returns None; the
    h1+h2+h3 composer takes over."""
    ins = RuleInputs(
        doc_id="NEM/EM200615/00001",
        headings=(
            "Explanatory Memorandum",
            "EM 2006/15",
            "Tax Laws Amendment (Loss Recoupment Rules) Bill 2005",
        ),
        heading_levels=(1, 2, 3),
    )
    assert _compose_from_em_front_matter(ins) is None
    d = derive_metadata(ins)
    assert d.title == (
        "Explanatory Memorandum — EM 2006/15 — "
        "Tax Laws Amendment (Loss Recoupment Rules) Bill 2005"
    )


# ---------------------------------------------------------------------------
# Date waterfall — ITAA 1997 mention in body should not hijack a 2024 TR


def test_itaa_1997_does_not_hijack_year():
    ins = RuleInputs(
        doc_id="TXR/TR20243/NAT/ATO/00001",
        headings=("Taxation Ruling", "TR 2024/3", "R&D tax incentive"),
        body_head=(
            "This Ruling concerns the Income Tax Assessment Act 1997 as amended. "
            "ITAA 1997 applies. ITAA 1997 section 355-25. "
        ) * 10,
    )
    d = derive_metadata(ins)
    assert d.date == "2024-01-01"


# ---------------------------------------------------------------------------
# Body-h2 and first-ref fallbacks


def test_body_h2_fallback_when_no_h1():
    """AFS / GDN / similar — page has no h1, no front-matter signals, just a
    self-contained body h2. Title is the h2 text."""
    ins = RuleInputs(
        doc_id="GDN/GDN20241/NAT/ATO/00001",
        headings=("First home super saver scheme", "GN 2024/1"),
        heading_levels=(2, 3),
    )
    d = derive_metadata(ins)
    assert d.title == "First home super saver scheme"


def test_ref_fallback_when_only_refs():
    """SRS — page has no headings and no Explanatory phrase, only the bill /
    act ref pair from <div id="Lawfront">. Title is the first ref (the
    bill being explained)."""
    ins = RuleInputs(
        doc_id="SRS/19770128/00001",
        headings=(),
        heading_levels=(),
        front_matter_chamber="House of Representatives",
        front_matter_refs=(
            "Income Tax (Rates) Amendment Bill (No. 2) 1977",
            "Income Tax (Rates) Amendment Act (No. 2) 1977",
        ),
    )
    d = derive_metadata(ins)
    assert d.title == "Income Tax (Rates) Amendment Bill (No. 2) 1977"


def test_em_phrase_still_wins_over_h2():
    """Priority — when the EM front-matter phrase is present alongside a body
    h2, the EM composer fires first."""
    ins = RuleInputs(
        doc_id="NEM/EM201018/NAT/ATO/00003",
        headings=(
            "Chapter 1 - Introduction to the new R&D tax incentive",
            "Outline of chapter",
        ),
        heading_levels=(2, 3),
        front_matter_chamber="House of Representatives",
        front_matter_refs=(
            "Tax Laws Amendment (Research and Development) Bill 2010",
        ),
        front_matter_phrase="Explanatory Memorandum",
    )
    d = derive_metadata(ins)
    assert d.title == (
        "Explanatory Memorandum — "
        "Tax Laws Amendment (Research and Development) Bill 2010 — "
        "Chapter 1 - Introduction to the new R&D tax incentive"
    )


def test_h1_still_wins_over_h2_fallback():
    """Priority — when h1 is present, the leading-headings composer fires
    and the body-h2 fallback never runs."""
    ins = RuleInputs(
        doc_id="ELD/EBL199702/00001",
        headings=("EBL 1997/2", "Some body section"),
        heading_levels=(1, 2),
    )
    d = derive_metadata(ins)
    assert d.title == "EBL 1997/2 — Some body section"
