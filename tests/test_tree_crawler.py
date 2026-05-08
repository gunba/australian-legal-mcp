from __future__ import annotations

from typing import Any

from ato_mcp.scraper import tree_crawler
from ato_mcp.scraper.tree_crawler import AtoTreeCrawler


class _ProgressRecorder:
    def __init__(self) -> None:
        self.postfixes: list[str] = []

    def update(self, *_: Any, **__: Any) -> None:
        return None

    def close(self) -> None:
        return None

    def set_postfix_str(self, value: str) -> None:
        self.postfixes.append(value)


class _FlatClient:
    def fetch_nodes(self, query: str) -> list[dict[str, Any]]:
        assert query == "Mode=type&Action=initialise"
        return [{"title": f"Node {i}"} for i in range(1001)]


def test_crawler_progress_reports_frontier_not_queue(monkeypatch) -> None:
    progress = _ProgressRecorder()
    monkeypatch.setattr(tree_crawler, "progress_bar", lambda **_: progress)

    nodes = AtoTreeCrawler(_FlatClient()).crawl()

    assert len(nodes) == 1001
    assert progress.postfixes == ["crawl_frontier=1"]
    assert all(not postfix.startswith("queue=") for postfix in progress.postfixes)
