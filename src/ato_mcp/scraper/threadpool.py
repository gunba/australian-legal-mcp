from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from typing import Iterator, Optional

_DEFAULT_MAX_WORKERS = 4
# [SS-08] 4-worker default for fan-out; concurrent fetches still serialise on AtoBrowseClient's rate lock — worker count caps parsing parallelism, not network throughput.


@contextmanager
def thread_pool(max_workers: Optional[int] = None) -> Iterator[ThreadPoolExecutor]:
	executor = ThreadPoolExecutor(max_workers=max_workers or _DEFAULT_MAX_WORKERS)
	try:
		yield executor
	finally:
		executor.shutdown(wait=True)
