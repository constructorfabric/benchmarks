"""Replay, request_id conflicts and the one-running-turn-per-chat guard."""

from __future__ import annotations

import threading
import time
import uuid

from conftest import assert_problem, ok_stream, ub, wait_until


def _quota_rows(server, user_id):
    rows = server.query(
        "SELECT period_type, bucket, spent_credits_micro, reserved_credits_micro, calls FROM quota_usage WHERE user_id = ? "
        "ORDER BY period_type, bucket",
        ub(user_id),
    )
    return [tuple(r) for r in rows]


class Background:
    """Runs a streaming send in a thread (used to hold a running turn)."""

    def __init__(self, api, chat_id, content="hold", **extra):
        self.result = None
        self.thread = threading.Thread(target=self._run, args=(api, chat_id, content, extra), daemon=True)
        self.thread.start()

    def _run(self, api, chat_id, content, extra):
        self.result = api.send(chat_id, content, **extra)

    def join(self, timeout=60):
        self.thread.join(timeout)
        return self.result


def _wait_running(server, chat_id):
    return wait_until(
        lambda: server.query("SELECT request_id FROM chat_turns WHERE chat_id = ? AND state = 'running'", ub(chat_id)),
        msg="running turn",
    )


def test_replay_completed_turn_is_side_effect_free(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"text": "original answer"})
    first = ok_stream(api.send(chat["id"], "q", request_id=rid))
    wait_until(lambda: server.usage_events(request_id=rid), msg="usage")
    quota_before = _quota_rows(server, api.user_id)
    provider_calls = len(mock.chat_requests(chat["id"]))
    msgs_before = api.messages(chat["id"])

    replay = ok_stream(api.send(chat["id"], "q", request_id=rid))
    started = replay.first("stream_started")
    assert started["is_new_turn"] is False
    assert started["request_id"] == rid
    assert started["message_id"] == first.first("stream_started")["message_id"]
    assert replay.names == ["stream_started", "delta", "done"]
    assert replay.text == "original answer"
    done = replay.first("done")
    assert done["usage"] == {"input_tokens": 42, "output_tokens": 7}
    assert done["effective_model"] == "gpt-premium"
    assert "quota_warnings" not in done
    assert "citations" not in replay.names

    time.sleep(1.0)
    assert len(mock.chat_requests(chat["id"])) == provider_calls
    assert _quota_rows(server, api.user_id) == quota_before
    assert api.messages(chat["id"]) == msgs_before
    assert len(server.usage_events(request_id=rid)) == 1
    assert len(server.audit_events(request_id=rid)) == 1
    assert len(server.query("SELECT id FROM chat_turns WHERE chat_id = ?", ub(chat["id"]))) == 1


def test_request_id_conflict_failed_turn(api, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"http_status": 500})
    r = api.send(chat["id"], "q", request_id=rid)
    assert r.names[-1] == "error"
    r = api.send(chat["id"], "q", request_id=rid)
    assert_problem(r, 409, category="aborted", reason="request_id_conflict")


def test_request_id_conflict_running_and_parallel_guard(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 4})
    bg = Background(api, chat["id"], request_id=rid)
    _wait_running(server, chat["id"])
    # same request id while running
    r = api.send(chat["id"], "q", request_id=rid)
    assert_problem(r, 409, category="aborted", reason="request_id_conflict")
    # another request id while a turn is running
    other = str(uuid.uuid4())
    r = api.send(chat["id"], "q2", request_id=other)
    assert_problem(r, 409, category="aborted", reason="turn_already_running")
    assert api.turn(chat["id"], other).status_code == 404
    bg.join()
    # once terminal, a new turn is accepted
    ok_stream(api.send(chat["id"], "after", request_id=other))


def test_parallel_guard_is_per_chat(api, server, mock):
    a = api.create_chat()
    b = api.create_chat()
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 3})
    bg = Background(api, a["id"])
    _wait_running(server, a["id"])
    ok_stream(api.send(b["id"], "other chat"))
    bg.join()


def test_replay_checked_before_parallel_guard(api, server, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    mock.script({"text": "done before"})
    ok_stream(api.send(chat["id"], "first", request_id=rid))
    mock.script({"chunks": [], "terminal": "hang", "hang_secs": 4})
    bg = Background(api, chat["id"], "second")
    _wait_running(server, chat["id"])
    r = ok_stream(api.send(chat["id"], "first", request_id=rid))
    assert r.first("stream_started")["is_new_turn"] is False
    assert r.text == "done before"
    bg.join()


def test_concurrent_sends_one_wins(api, server, mock):
    chat = api.create_chat()
    mock.script(*[{"chunks": ["slow"], "chunk_delay_ms": 1500} for _ in range(4)])
    results = []
    barrier = threading.Barrier(4)

    def go(i):
        barrier.wait()
        results.append(api.send(chat["id"], f"race {i}"))

    threads = [threading.Thread(target=go, args=(i,)) for i in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(60)
    ok = [r for r in results if r.status == 200]
    rejected = [r for r in results if r.status == 409]
    assert len(ok) + len(rejected) == 4, [r.status for r in results]
    assert len(ok) >= 1
    for r in rejected:
        assert_problem(r, 409, reason="turn_already_running")
    assert len(server.query("SELECT id FROM chat_turns WHERE chat_id = ?", ub(chat["id"]))) == len(ok)


def test_request_id_conflict_after_retry_replaced_turn(api, mock):
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    ok_stream(api.send(chat["id"], "q", request_id=rid))
    ok_stream(api.retry(chat["id"], rid))
    r = api.send(chat["id"], "q", request_id=rid)
    assert_problem(r, 409, reason="request_id_conflict")
