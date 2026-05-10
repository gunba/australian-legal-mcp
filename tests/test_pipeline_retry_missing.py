"""Tests for the ``retry_missing`` refresh-source mode."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from ato_mcp.scraper import pipeline


_FAT_BODY = "<p>" + ("body content " * 200) + "</p>"


def _write_index(path: Path, records: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for rec in records:
            fh.write(json.dumps(rec) + "\n")


def _read_index(path: Path) -> dict[str, dict[str, Any]]:
    out: dict[str, dict[str, Any]] = {}
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            text = line.strip()
            if not text:
                continue
            rec = json.loads(text)
            out[rec["canonical_id"]] = rec
    return out


def _run_retry(tmp_path: Path, fetcher) -> pipeline.RetryMissingSummary:
    summary = pipeline._run_retry_missing(
        output_dir=tmp_path,
        base_url="https://www.ato.gov.au",
        parser_run_date="2026-05-10T00:00:00Z",
        max_workers=1,
        request_interval=0.0,
        verbose_progress=False,
        page_fetcher=fetcher,
    )
    return summary


def test_retry_missing_classifies_outcomes(tmp_path: Path) -> None:
    index_path = tmp_path / "index.jsonl"
    initial = [
        {
            "canonical_id": "/law/view/document?docid=DEAD/EV1",
            "href": "/law/view/document?docid=DEAD/EV1",
            "status": "missing_content",
            "payload_path": None,
            "assets": [],
            "error": "lawContents div not found",
            "http_status": 200,
            "downloaded_at": "2025-11-15T00:00:00Z",
        },
        {
            "canonical_id": "/law/view/document?docid=STUB/JUD1",
            "href": "/law/view/document?docid=STUB/JUD1",
            "status": "missing_content",
            "payload_path": None,
            "assets": [],
            "error": "lawContents div not found",
            "http_status": 200,
            "downloaded_at": "2025-11-15T00:00:00Z",
        },
        {
            "canonical_id": "/law/view/document?docid=GOOD/RECOV1",
            "href": "/law/view/document?docid=GOOD/RECOV1",
            "status": "missing_content",
            "payload_path": None,
            "assets": [],
            "error": "lawContents div not found",
            "http_status": 200,
            "downloaded_at": "2025-11-15T00:00:00Z",
        },
        {
            "canonical_id": "/law/view/document?docid=ALREADY/OK",
            "href": "/law/view/document?docid=ALREADY/OK",
            "status": "success",
            "payload_path": "payloads/Other_ATO_documents/already/already_ok.html",
            "assets": [],
            "error": None,
            "http_status": 200,
            "downloaded_at": "2025-11-15T00:00:00Z",
        },
    ]
    _write_index(index_path, initial)

    untouched_payload = (
        tmp_path / "payloads" / "Other_ATO_documents" / "already" / "already_ok.html"
    )
    untouched_payload.parent.mkdir(parents=True, exist_ok=True)
    untouched_payload.write_text("<div>untouched</div>", encoding="utf-8")

    def fake_fetcher(href: str) -> tuple[int, str]:
        if "DEAD" in href:
            return 404, ""
        if "STUB" in href:
            return 200, "<html><body><article><p>tiny</p></article></body></html>"
        if "GOOD" in href:
            return 200, f"<html><body><article>{_FAT_BODY}</article></body></html>"
        raise AssertionError(f"unexpected href requested: {href}")

    summary = _run_retry(tmp_path, fake_fetcher)

    assert summary.eligible == 3
    assert summary.recovered == 1
    assert summary.confirmed_404 == 1
    assert summary.confirmed_stub == 1
    assert summary.still_missing == 0

    rewritten = _read_index(tmp_path / "index.jsonl")
    assert len(rewritten) == 4

    dead = rewritten["/law/view/document?docid=DEAD/EV1"]
    assert dead["status"] == "confirmed_404"
    assert dead["payload_path"] is None

    stub = rewritten["/law/view/document?docid=STUB/JUD1"]
    assert stub["status"] == "confirmed_stub"
    assert stub["payload_path"] is None

    good = rewritten["/law/view/document?docid=GOOD/RECOV1"]
    assert good["status"] == "success"
    assert good["payload_path"]
    payload_abs = tmp_path / good["payload_path"]
    assert payload_abs.exists()
    assert payload_abs.is_relative_to(
        tmp_path / "payloads" / "Other_ATO_documents" / "recovered"
    )
    assert payload_abs.stat().st_size >= 1024

    untouched = rewritten["/law/view/document?docid=ALREADY/OK"]
    assert untouched["status"] == "success"
    assert untouched["payload_path"] == initial[3]["payload_path"]
    assert untouched_payload.exists()


def test_retry_missing_keeps_confirmed_dead_records(tmp_path: Path) -> None:
    index_path = tmp_path / "index.jsonl"
    initial = [
        {
            "canonical_id": "/law/view/document?docid=DEAD/EV2",
            "href": "/law/view/document?docid=DEAD/EV2",
            "status": "confirmed_404",
            "payload_path": None,
            "http_status": 404,
        },
        {
            "canonical_id": "/law/view/document?docid=STUB/JUD2",
            "href": "/law/view/document?docid=STUB/JUD2",
            "status": "confirmed_stub",
            "payload_path": None,
            "http_status": 200,
        },
    ]
    _write_index(index_path, initial)

    def fail_fetcher(href: str) -> tuple[int, str]:
        raise AssertionError("must not refetch confirmed-dead records")

    summary = _run_retry(tmp_path, fail_fetcher)
    assert summary.eligible == 0
    assert summary.recovered == 0
    assert summary.confirmed_404 == 0
    assert summary.confirmed_stub == 0


def test_retry_missing_failed_status_remains_missing_for_next_run(tmp_path: Path) -> None:
    index_path = tmp_path / "index.jsonl"
    initial = [
        {
            "canonical_id": "/law/view/document?docid=NET/ERR1",
            "href": "/law/view/document?docid=NET/ERR1",
            "status": "failed",
            "payload_path": None,
            "http_status": None,
            "error": "previous network failure",
        },
    ]
    _write_index(index_path, initial)

    def flaky_fetcher(href: str) -> tuple[int, str]:
        raise RuntimeError("simulated network error")

    summary = _run_retry(tmp_path, flaky_fetcher)
    assert summary.eligible == 1
    assert summary.still_missing == 1
    assert summary.recovered == 0
    assert summary.confirmed_404 == 0
    assert summary.confirmed_stub == 0

    rewritten = _read_index(tmp_path / "index.jsonl")
    rec = rewritten["/law/view/document?docid=NET/ERR1"]
    assert rec["status"] == "missing_content"
    assert rec["payload_path"] is None


def test_retry_missing_via_refresh_source_entry_point(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    index_path = tmp_path / "index.jsonl"
    _write_index(
        index_path,
        [
            {
                "canonical_id": "/law/view/document?docid=GOOD/E2E",
                "href": "/law/view/document?docid=GOOD/E2E",
                "status": "missing_content",
                "payload_path": None,
                "http_status": 200,
                "error": "lawContents div not found",
            },
        ],
    )

    captured: dict[str, Any] = {}

    class _Resp:
        def __init__(self, status: int, text: str) -> None:
            self.status_code = status
            self.text = text

        def raise_for_status(self) -> None:
            if self.status_code >= 400:
                raise AssertionError("404 should be intercepted before raise_for_status")

    def fake_get(url: str, timeout: int = 30) -> _Resp:  # noqa: ARG001
        captured["url"] = url
        return _Resp(200, f"<html><body><article>{_FAT_BODY}</article></body></html>")

    monkeypatch.setattr(pipeline.requests, "get", fake_get)

    result = pipeline.refresh_source(
        mode="retry_missing",
        output_dir=tmp_path,
        request_interval=0.0,
    )
    assert result.mode == "retry_missing"
    assert result.retry_missing_summary is not None
    assert result.retry_missing_summary.recovered == 1
    assert captured["url"] == "https://www.ato.gov.au/law/view/document?docid=GOOD/E2E"


def test_retry_missing_raises_when_index_missing(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        pipeline._run_retry_missing(
            output_dir=tmp_path,
            base_url="https://www.ato.gov.au",
            parser_run_date="2026-05-10T00:00:00Z",
            max_workers=1,
            request_interval=0.25,
            verbose_progress=False,
            page_fetcher=lambda _href: (200, ""),
        )


def test_retry_missing_default_interval_is_quarter_second_via_orchestrator(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    index_path = tmp_path / "index.jsonl"
    _write_index(
        index_path,
        [
            {
                "canonical_id": "/law/view/document?docid=PACE/CHK",
                "href": "/law/view/document?docid=PACE/CHK",
                "status": "missing_content",
                "payload_path": None,
                "http_status": 200,
            },
        ],
    )

    seen_intervals: list[float] = []

    def stub_run(**kwargs: Any) -> pipeline.RetryMissingSummary:
        seen_intervals.append(kwargs["request_interval"])
        return pipeline.RetryMissingSummary(
            eligible=0, recovered=0, confirmed_404=0, confirmed_stub=0, still_missing=0,
        )

    monkeypatch.setattr(pipeline, "_run_retry_missing", stub_run)

    pipeline.refresh_source(mode="retry_missing", output_dir=tmp_path)
    pipeline.refresh_source(mode="retry_missing", output_dir=tmp_path, request_interval=1.0)

    assert seen_intervals == [0.25, 1.0]
