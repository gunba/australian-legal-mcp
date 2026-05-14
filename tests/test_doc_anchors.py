"""Tests for the anchor extractor (in-doc, sister, history navigation refs)."""
from __future__ import annotations

from ato_mcp.indexer.anchors import (
    AnchorRef,
    anchor_target_to_chunk,
    extract_anchors,
)


def test_in_doc_anchor_with_table_label() -> None:
    """A <tr> with a label cell + an anchor cell yields an in_doc ref labelled
    by the sibling cell text."""
    html = """
    <div>
        <table>
            <tr><td>Keep</td><td><a href="#P18">18</a></td></tr>
            <tr><td>Discard</td><td><a href="#P22">22</a></td></tr>
        </table>
        <p><a name="P18"></a>Paragraph eighteen body.</p>
        <p><a name="P22"></a>Paragraph twenty-two body.</p>
    </div>
    """
    refs = extract_anchors(html, source_doc_id="TXR/TR/00001")
    in_doc = [r for r in refs if r.kind == "in_doc"]
    labels = sorted(r.label for r in in_doc)
    assert labels == ["Discard 22", "Keep 18"]
    targets = sorted(r.target_anchor for r in in_doc)
    assert targets == ["P18", "P22"]


def test_in_doc_anchor_inline_falls_back_to_text() -> None:
    """Inline body anchors (no table) fall back to the anchor's own visible text."""
    html = """
    <p>See <a href="#sec5">Section 5</a> for the full discussion.</p>
    <h2><a name="sec5"></a>Section 5</h2>
    <p>Body of section five.</p>
    """
    refs = extract_anchors(html, source_doc_id="OPS/X/00001")
    in_doc = [r for r in refs if r.kind == "in_doc"]
    assert len(in_doc) == 1
    assert in_doc[0].label == "Section 5"
    assert in_doc[0].target_anchor == "sec5"


def test_in_doc_anchor_skipped_when_target_missing() -> None:
    """An <a href="#X"> without a matching anchor target is silently dropped."""
    html = '<p><a href="#nowhere">link</a></p>'
    refs = extract_anchors(html, source_doc_id="X/Y/0")
    assert [r for r in refs if r.kind == "in_doc"] == []


def test_sister_doc_anchor() -> None:
    """A link to a different doc_id (no PiT) is a sister-doc reference."""
    html = (
        '<p>See also '
        '<a href="/law/view/document?docid=TXR/TR967ER/NAT/ATO/00001">Erratum</a>'
        '.</p>'
    )
    refs = extract_anchors(html, source_doc_id="TXR/TR967/NAT/ATO/00001")
    sisters = [r for r in refs if r.kind == "sister"]
    assert len(sisters) == 1
    assert sisters[0].target_doc_id == "TXR/TR967ER/NAT/ATO/00001"
    assert sisters[0].label == "Erratum"


def test_history_anchor_with_pit_derives_date() -> None:
    """A link with a PiT param is a historical-version reference. The label
    gets a YYYY-MM-DD date appended derived from the timestamp. The
    target_doc_id is the BASE doc_id; the timestamp travels separately in
    target_pit so the anchor row records the existence of an older version
    without our store treating it as a separate row."""
    html = """
    <p><a href="/law/view/document?docid=TXR/TR967/NAT/ATO/00001&PiT=19960320000001">Original ruling</a></p>
    """
    refs = extract_anchors(html, source_doc_id="TXR/TR967/NAT/ATO/00001")
    hist = [r for r in refs if r.kind == "history"]
    assert len(hist) == 1
    assert hist[0].target_doc_id == "TXR/TR967/NAT/ATO/00001"
    assert hist[0].target_pit == "19960320000001"
    assert "1996-03-20" in hist[0].label
    assert "Original ruling" in hist[0].label


def test_self_reference_without_pit_dropped() -> None:
    """A link from a doc to itself (same doc_id, no PiT) is not navigation —
    drop it. Otherwise every doc would surface its own canonical_url as a
    'sister'."""
    html = (
        '<p><a href="/law/view/document?docid=TXR/TR967/NAT/ATO/00001">'
        'Same doc</a></p>'
    )
    refs = extract_anchors(html, source_doc_id="TXR/TR967/NAT/ATO/00001")
    assert refs == []


def test_history_table_with_three_versions() -> None:
    """The TR 96/7 history table shape: row labels + version anchors. Each
    history anchor stores the BASE doc_id plus the PiT timestamp separately;
    different PiTs of the same base doc are distinct anchors."""
    html = """
    <div>
        <table>
            <tr>
                <td>20 March 1996</td>
                <td><a href="/law/view/document?docid=TXR/TR967/NAT/ATO/00001&PiT=19960320000001">Original ruling</a></td>
            </tr>
            <tr>
                <td>18 April 2012</td>
                <td><a href="/law/view/document?docid=TXR/TR967/NAT/ATO/00001&PiT=20120418000001">Consolidated ruling</a></td>
                <td><a href="/law/view/document?docid=TXR/TR967ER/NAT/ATO/00001&PiT=20120418000001">Erratum</a></td>
            </tr>
        </table>
    </div>
    """
    refs = extract_anchors(html, source_doc_id="TXR/TR967/NAT/ATO/00001")
    hist = [r for r in refs if r.kind == "history"]
    assert {(r.target_doc_id, r.target_pit) for r in hist} == {
        ("TXR/TR967/NAT/ATO/00001", "19960320000001"),
        ("TXR/TR967/NAT/ATO/00001", "20120418000001"),
        ("TXR/TR967ER/NAT/ATO/00001", "20120418000001"),
    }
    # Erratum has its own row label "18 April 2012" + own text "Erratum".
    erratum = next(r for r in hist if "Erratum" in r.label)
    assert "1996" not in erratum.label  # not cross-contaminated with first row


def test_pit_to_date_in_label_for_unlabelled_history_link() -> None:
    """When the anchor text is empty, the PiT-derived date stands alone as
    the label."""
    html = (
        '<p><a href="/law/view/document?docid=ABC/X/00001&PiT=20200101000000">'
        '</a></p>'
    )
    refs = extract_anchors(html, source_doc_id="OTHER/Y/00001")
    hist = [r for r in refs if r.kind == "history"]
    assert len(hist) == 1
    # Label is the date, since anchor text was empty.
    assert hist[0].label == "2020-01-01"
    # target_doc_id is the BASE; the timestamp lives in target_pit.
    assert hist[0].target_doc_id == "ABC/X/00001"
    assert hist[0].target_pit == "20200101000000"


def test_dedup_of_repeated_references() -> None:
    """Two <a href="#X"> links with the same label resolve to one in_doc ref."""
    html = """
    <p>See <a href="#P5">paragraph 5</a> earlier.</p>
    <p>And again, <a href="#P5">paragraph 5</a>.</p>
    <p><a name="P5"></a>Body of P5.</p>
    """
    refs = extract_anchors(html, source_doc_id="X/Y/0")
    in_doc = [r for r in refs if r.kind == "in_doc"]
    assert len(in_doc) == 1


def test_anchor_target_to_chunk_finds_marker() -> None:
    """Helper that scans chunk text for [anchor:X] markers."""
    chunks = [
        (10, "intro paragraph"),
        (11, "body with [anchor:P5] marker inside"),
        (12, "trailer"),
    ]
    assert anchor_target_to_chunk("P5", chunks) == 11
    assert anchor_target_to_chunk("missing", chunks) is None


def test_anchor_ref_immutable_dataclass() -> None:
    """AnchorRef is frozen — agents/build code can rely on hashability."""
    a = AnchorRef(kind="sister", label="x", target_doc_id="X/Y/0")
    s = {a}
    assert a in s
