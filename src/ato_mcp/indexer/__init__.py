"""Maintainer-side build pipeline: metadata parsing, content extraction,
chunking, packing, index build, release.

Submodules are imported directly (e.g. ``from ato_mcp.indexer.metadata import
parse_docid``) rather than from this package root. Keeping ``__init__``
empty avoids dragging ``build``/``pack`` and their numpy dependency into
import graphs that only need the lightweight parsing helpers.
"""
