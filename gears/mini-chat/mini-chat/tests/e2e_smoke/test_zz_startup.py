"""Server start while clients already send requests.

`/health` answers 200 and mini-chat routes serve requests before the mini-chat gear has finished
starting (its outbox and workers). A write issued in that window can make the outbox start fail
with SQLite `database is locked` (code 517, SQLITE_BUSY_SNAPSHOT) and the whole process exits with
`start failed for 'mini-chat'`. Restarts the server a few times while creating chats in a loop.
"""

import time

import pytest
import requests

from conftest import BASE, TOKEN_A

VARIANT = "default"


def test_requests_during_startup_do_not_crash_server(server):
    outcomes = []
    try:
        for _ in range(8):
            server.stop_server()
            server.start_server("default", wait=False)
            proc = server.server_proc
            deadline = time.time() + 20
            while time.time() < deadline and proc.poll() is None:
                try:
                    requests.post(f"{BASE}/chats", json={}, timeout=2,
                                  headers={"Authorization": f"Bearer {TOKEN_A}"})
                except requests.RequestException:
                    pass
                if "mini-chat gear started" in server.server_log.read_text(errors="replace"):
                    break
                time.sleep(0.02)
            time.sleep(0.5)
            log = server.server_log.read_text(errors="replace")
            outcomes.append({"alive": proc.poll() is None, "start_failed": "start failed" in log,
                             "log": str(server.server_log)})
    finally:
        server.stop_server()
        server.ensure("default")
    crashed = [o for o in outcomes if not o["alive"] or o["start_failed"]]
    assert crashed == [], crashed
