"""A machine-wide cap on concurrent requests to the LiteLLM router, shared by every process.

The cfs-pr-review key allows 2 parallel requests (max_parallel_requests), and that is deliberate:
the nodes are at capacity. Past the cap the router answers 429, and PR-Agent, after its own
retries, drops the chunk and still reports the job as done. So every caller takes a slot here
first: N lock files under out/.router_slots, held with flock for the duration of one request (or
one Claude Code session, which sends its requests one at a time). A crashed process releases its
lock with its file descriptor, so a slot is never lost.

ROUTER_MAX_PARALLEL sets N (default 2, the key's limit).
"""

from __future__ import annotations

import asyncio
import contextlib
import fcntl
import os

from pr_bench.paths import OUT

SLOTS_DIR = OUT / ".router_slots"
RATE_LIMIT_MARKERS = ("429", "max_parallel_requests", "rate limit", "ratelimit")


def slot_count() -> int:
    return int(os.environ.get("ROUTER_MAX_PARALLEL", "2"))


@contextlib.asynccontextmanager
async def router_slot(poll_s: float = 2.0):
    SLOTS_DIR.mkdir(parents=True, exist_ok=True)
    while True:
        for i in range(slot_count()):
            fh = open(SLOTS_DIR / f"slot{i}", "a")
            try:
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                fh.close()
                continue
            try:
                yield i
            finally:
                fcntl.flock(fh, fcntl.LOCK_UN)
                fh.close()
            return
        await asyncio.sleep(poll_s)


def is_rate_limited(error: BaseException | str) -> bool:
    text = str(error).lower()
    return any(m in text for m in RATE_LIMIT_MARKERS)
