from __future__ import annotations

from ato_mcp.indexer.definitions import DefinitionChunk, extract_definitions


def test_extract_definitions_cuts_single_entry() -> None:
    defs = extract_definitions(
        doc_id="PAC/19970038/995-1",
        source_title="Income Tax Assessment Act 1997 s 995-1",
        source_type="Legislation_and_supporting_material",
        chunks=[
            DefinitionChunk(
                ord=1,
                heading_path="Note 2:",
                anchor=None,
                text=(
                    "***corporate tax gross-up rate***\n\n"
                    ", of an entity for an income year, means the amount worked out using "
                    "the following formula:\n\n"
                    "Formula: (100% - corporate tax rate) / corporate tax rate\n\n"
                    "***corporate tax rate***\n\nmeans the rate of tax."
                ),
            )
        ],
    )
    assert [d.term for d in defs] == ["corporate tax gross-up rate", "corporate tax rate"]
    assert defs[0].norm_term == "corporate tax gross-up rate"
    assert "Formula:" in defs[0].body


def test_definition_ids_do_not_collide_on_shared_prefix() -> None:
    defs = extract_definitions(
        doc_id="PAC/19970038/995-1",
        source_title="Income Tax Assessment Act 1997 s 995-1",
        source_type="Legislation_and_supporting_material",
        chunks=[
            DefinitionChunk(
                ord=1,
                heading_path="Note 2:",
                anchor=None,
                text=(
                    "***example term***\n\nmeans "
                    + ("same opening " * 30)
                    + "first tail.\n\n"
                    "***example term***\n\nmeans "
                    + ("same opening " * 30)
                    + "second tail."
                ),
            )
        ],
    )
    assert len(defs) == 2
    assert defs[0].definition_id != defs[1].definition_id
