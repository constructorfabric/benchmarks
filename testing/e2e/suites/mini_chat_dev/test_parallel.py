"""Parallel turn guard (DESIGN section 4 "Parallel Turn Policy")."""

import threading
import uuid

import pytest

from . import mock_provider as mp
from .helpers import PREFIX, api, assert_problem, create_chat, stream, wait_until

pytestmark = pytest.mark.usefixtures("server")


def test_second_send_while_running_is_409_then_allowed(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("slow"),
                                                mp.ev_completed()], "delay_ms": 1500})
    s = api()
    chat = create_chat(s)
    rid = str(uuid.uuid4())
    out = {}
    t = threading.Thread(target=lambda: out.update(res=stream(api(), chat["id"],
                                                              {"content": "one", "request_id": rid})))
    t.start()
    try:
        wait_until(lambda: s.get(f"{PREFIX}/chats/{chat['id']}/turns/{rid}").status_code == 200,
                   message="first turn running")
        r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "two"})
        assert_problem(r, 409, "aborted", reason="turn_already_running")
        # Idempotency is checked first: the running turn's own request id is a conflict.
        r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "one", "request_id": rid})
        assert_problem(r, 409, "aborted", reason="request_id_conflict")
    finally:
        t.join(timeout=60)
    assert out["res"].terminal[0] == "done"
    third = stream(s, chat["id"], {"content": "three"})
    assert third.terminal[0] == "done"


def test_concurrent_sends_one_wins(reset_mock):
    reset_mock.enqueue("responses", {"start_delay_ms": 1000}, repeat=2)
    chat = create_chat(api())
    results = []
    barrier = threading.Barrier(2)

    def send(i):
        barrier.wait()
        r = api().post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": f"c{i}"},
                       stream=True)
        try:
            results.append((r.status_code, r.content.decode()))
        finally:
            r.close()

    threads = [threading.Thread(target=send, args=(i,)) for i in range(2)]
    for th in threads:
        th.start()
    for th in threads:
        th.join(timeout=60)
    statuses = sorted(code for code, _ in results)
    assert statuses == [200, 409], results
    conflict = next(body for code, body in results if code == 409)
    assert "turn_already_running" in conflict
