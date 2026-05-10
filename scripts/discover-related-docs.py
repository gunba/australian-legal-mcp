#!/usr/bin/env python3
"""Walk a scraped ato_pages/ tree and emit references that we never fetched.

For each scraped doc, parse its cleaned HTML with
``ato_mcp.indexer.anchors.extract_anchors`` and collect every ``sister`` /
``history`` reference. Anything whose canonical_id is not already in
``index.jsonl`` is written to a JSONL file the scraper can consume via
``ato-mcp refresh-source --mode retry_missing --explicit-links-file ...``.

This is a pure local computation — no HTTP. The scraper does the actual
fetching when invoked with the produced JSONL.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Make the in-tree package importable when running from a checkout without
# `pip install -e .`.
_REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(_REPO_ROOT / "src"))

from ato_mcp.indexer.anchors import extract_anchors  # noqa: E402
from ato_mcp.indexer.metadata import doc_id_for  # noqa: E402


def _iter_index_records(index_path: Path):
    with index_path.open("r", encoding="utf-8") as fh:
        for line in fh:
            text = line.strip()
            if not text:
                continue
            try:
                yield json.loads(text)
            except json.JSONDecodeError as exc:
                print(
                    f"warning: skipping malformed index line: {exc}",
                    file=sys.stderr,
                )


def _load_existing_canonical_ids(index_path: Path) -> set[str]:
    out: set[str] = set()
    for rec in _iter_index_records(index_path):
        cid = rec.get("canonical_id")
        if isinstance(cid, str) and cid:
            out.add(cid)
    return out


def _build_candidate(target_doc_id: str, pit: str | None) -> tuple[str, str, str | None]:
    """Return ``(canonical_id, href, pit)`` for a sister/history reference.

    ``target_doc_id`` for history refs already carries an ``@<PiT>`` suffix
    (per ``anchors.extract_anchors``); we strip it back off to build the raw
    ``?PiT=...`` fetch URL while keeping the ``@``-suffixed canonical_id.
    """
    if pit:
        # Defensive: the @PiT suffix may or may not be present depending on how
        # the caller constructed target_doc_id. anchors.py emits the suffix;
        # honour it if present and rebuild deterministically either way.
        if "@" in target_doc_id:
            base, _sep, _pit = target_doc_id.rpartition("@")
        else:
            base = target_doc_id
        canonical_id = f"/law/view/document?docid={base}@{pit}"
        href = f"/law/view/document?docid={base}&PiT={pit}"
        return canonical_id, href, pit
    canonical_id = f"/law/view/document?docid={target_doc_id}"
    href = canonical_id
    return canonical_id, href, None


def discover(
    *,
    pages_dir: Path,
    output: Path,
) -> dict:
    index_path = pages_dir / "index.jsonl"
    if not index_path.exists():
        raise SystemExit(
            f"index.jsonl not found at {index_path}. Provide --pages-dir pointing at a "
            "populated scrape directory."
        )

    existing = _load_existing_canonical_ids(index_path)

    output.parent.mkdir(parents=True, exist_ok=True)

    walked = 0
    skipped_payload = 0
    skipped_read = 0
    sister_found = 0
    history_found = 0
    emitted = 0
    seen_emitted: set[str] = set()

    with output.open("w", encoding="utf-8") as out_fh:
        for rec in _iter_index_records(index_path):
            walked += 1
            if rec.get("status") != "success":
                continue
            payload_rel = rec.get("payload_path")
            if not payload_rel:
                skipped_payload += 1
                continue
            payload_abs = pages_dir / payload_rel
            if not payload_abs.exists():
                skipped_payload += 1
                continue
            try:
                html = payload_abs.read_text(encoding="utf-8")
            except OSError as exc:
                print(
                    f"warning: cannot read {payload_abs}: {exc}", file=sys.stderr
                )
                skipped_read += 1
                continue
            cid = rec.get("canonical_id") or ""
            try:
                source_doc_id = doc_id_for(cid)
            except Exception as exc:  # pragma: no cover - defensive
                print(
                    f"warning: cannot derive doc_id for {cid!r}: {exc}",
                    file=sys.stderr,
                )
                continue

            try:
                refs = extract_anchors(html, source_doc_id=source_doc_id)
            except Exception as exc:
                print(
                    f"warning: extract_anchors failed for {payload_abs}: {exc}",
                    file=sys.stderr,
                )
                skipped_read += 1
                continue

            for ref in refs:
                if ref.kind == "sister":
                    if not ref.target_doc_id:
                        continue
                    sister_found += 1
                    canonical_id, href, pit = _build_candidate(
                        ref.target_doc_id, None
                    )
                elif ref.kind == "history":
                    if not ref.target_doc_id:
                        continue
                    history_found += 1
                    canonical_id, href, pit = _build_candidate(
                        ref.target_doc_id, ref.target_pit
                    )
                else:
                    continue

                if canonical_id in existing:
                    continue
                if canonical_id in seen_emitted:
                    continue
                seen_emitted.add(canonical_id)
                out_fh.write(
                    json.dumps(
                        {
                            "canonical_id": canonical_id,
                            "href": href,
                            "pit": pit,
                        },
                        ensure_ascii=False,
                    )
                    + "\n"
                )
                emitted += 1

    summary = {
        "pages_dir": str(pages_dir),
        "output": str(output),
        "records_walked": walked,
        "skipped_no_payload": skipped_payload,
        "skipped_read_errors": skipped_read,
        "sister_refs_seen": sister_found,
        "history_refs_seen": history_found,
        "missing_emitted": emitted,
    }
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pages-dir",
        type=Path,
        default=Path("../ato_pages"),
        help="Path to a populated ato_pages/ directory (must contain index.jsonl).",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("logs/related-docs-to-fetch.jsonl"),
        help="JSONL file to write missing sister/history references to.",
    )
    args = parser.parse_args()

    summary = discover(pages_dir=args.pages_dir, output=args.output)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
