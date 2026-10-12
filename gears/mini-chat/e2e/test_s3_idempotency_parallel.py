"""S3 idempotency (replay / request_id conflicts) and the parallel turn guard.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "Streaming Contract" idempotency table: a completed, not soft-deleted
  ``(chat_id, request_id)`` is replayed as ``stream_started``
  (``is_new_turn: false``, persisted assistant ``message_id``), one ``delta``
  with the full persisted text and ``done`` (usage from the stored message,
  ``quota_decision`` / ``downgrade_from`` rebuilt, no ``downgrade_reason``,
  no citations); any other state or a soft-deleted turn is HTTP 409
  ``aborted`` ``context.reason = request_id_conflict`` (JSON, no SSE); the
  losing request of a same-key race is reported as ``request_id_conflict``;
* §3.3 "Replay is side-effect-free invariant" and §3.7 "Idempotency Rules":
  no provider call, no quota reserve or ``quota_usage`` change, no outbox
  message (usage / audit queues), no new rows;
* §3.3 "Parallel turn guard" and §3.7 "Parallel Turn Policy (P1)": a chat
  with a ``running`` turn rejects any other send with 409 ``aborted``
  ``turn_already_running`` (also for the insert-race loser); the idempotency
  check runs first, so a completed turn replays while another turn runs;
* ``DoneData.quota_warnings`` is absent on replay; ADR-0010 (replay ``done``
  rebuilt from the stored models).

Running turns are produced with the mock ``hang`` scenario and released by
disconnecting their client (the turn then leaves ``running``).
"""

from __future__ import annotations

import threading
import time
import uuid
from typing import Any

import httpx
import pytest

from mc_helpers import assert_problem, db_uuid, parse_sse_lines, wait_until
from mc_seed import outbox_payloads_for_chat, outbox_snapshot, seed_turn

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
USAGE_QUEUE = "mini-chat.usage_snapshot"
AUDIT_QUEUE = "mini-chat.audit"
HANG = {"kind": "hang", "hang_max_secs": 90}


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def _new_chat(mc, user: str = "A1", **body: Any) -> dict[str, Any]:
    resp = mc.post(CHATS, user, json={"title": "S3 idempotency", **body})
    assert resp.status_code == 201, resp.text
    return resp.json()


def _post_stream(mc, chat_id: str, body: dict[str, Any], user: str = "A1"):
    return mc.stream("POST", _stream_path(chat_id), user, json=body, timeout=60.0)


def _complete_turn(mc, chat_id: str, content: str, request_id: str | None = None, user: str = "A1"):
    body: dict[str, Any] = {"content": content}
    if request_id is not None:
        body["request_id"] = request_id
    result = _post_stream(mc, chat_id, body, user)
    assert result.status_code == 200, f"HTTP {result.status_code}: {result.text[:1000]}"
    assert result.is_sse, result.headers
    assert result.names()[-1:] == ["done"], f"turn did not complete: {result.events}"
    return result


def _assert_conflict(result, reason: str) -> dict[str, Any]:
    """A 409 ``aborted`` JSON Problem with ``context.reason == reason``; no SSE stream."""
    assert not result.is_sse, (
        f"expected a JSON 409 {reason}, got an SSE stream: {result.names()}"
    )
    assert result.events == []
    assert result.response is not None
    return assert_problem(result.response, 409, category="aborted", reason=reason)


def _stream_requests(mock) -> list[dict[str, Any]]:
    """Streaming Responses API calls recorded by the mock (thread-summary calls excluded)."""
    return [r for r in mock.requests("/responses", method="POST") if r["stream"]]


def _turn_rows(mc, chat_id: str) -> list[dict[str, Any]]:
    return mc.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id", (db_uuid(chat_id),)
    )


def _message_rows(mc, chat_id: str) -> list[dict[str, Any]]:
    return mc.query(
        "SELECT * FROM messages WHERE chat_id = ? ORDER BY created_at, id", (db_uuid(chat_id),)
    )


def _quota_rows(mc, user: str) -> list[dict[str, Any]]:
    ident = mc.identity(user)
    return mc.query(
        "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? "
        "ORDER BY period_type, period_start, bucket",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )


def _turn_status(mc, chat_id: str, request_id: str, user: str = "A1") -> dict[str, Any] | None:
    resp = mc.get(_turn_path(chat_id, request_id), user)
    if resp.status_code == 404:
        return None
    assert resp.status_code == 200, resp.text
    return resp.json()


def _wait_turn_left_running(mc, chat_id: str, request_id: str, user: str = "A1", timeout: float = 30.0) -> str:
    def probe() -> str | None:
        status = _turn_status(mc, chat_id, request_id, user)
        if status is not None and status["state"] != "running":
            return status["state"]
        return None

    return wait_until(probe, timeout=timeout, description=f"turn {request_id} to leave running")


def _wait_turn_state(mc, chat_id: str, request_id: str, state: str, user: str = "A1") -> dict[str, Any]:
    def probe() -> dict[str, Any] | None:
        status = _turn_status(mc, chat_id, request_id, user)
        return status if status is not None and status["state"] == state else None

    return wait_until(probe, timeout=30.0, description=f"turn {request_id} state {state!r}")


class BackgroundSend:
    """A ``messages:stream`` request driven from a background thread.

    The thread reads the SSE stream until ``stream_started`` (or a terminal
    event) and then holds the connection open until :meth:`release`, which
    disconnects the client. A JSON (non-SSE) answer is read completely.
    ``ready`` is set once the outcome of the request is known.
    """

    def __init__(
        self,
        mc,
        chat_id: str,
        body: dict[str, Any],
        user: str = "A1",
        barrier: threading.Barrier | None = None,
    ) -> None:
        self.url = mc.url(_stream_path(chat_id))
        self.headers = mc.headers(user)
        self.body = body
        self.barrier = barrier
        self.ready = threading.Event()
        self._release = threading.Event()
        self.status_code: int | None = None
        self.content_type: str = ""
        self.events: list[tuple[str, Any]] = []
        self.json: Any = None
        self.response: httpx.Response | None = None
        self.error: str | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> "BackgroundSend":
        self._thread.start()
        return self

    @property
    def is_sse(self) -> bool:
        return self.content_type.startswith("text/event-stream")

    def names(self) -> list[str]:
        return [name for name, _ in self.events]

    def _run(self) -> None:
        try:
            timeout = httpx.Timeout(connect=5.0, read=120.0, write=10.0, pool=10.0)
            with httpx.Client(timeout=timeout) as client:
                if self.barrier is not None:
                    self.barrier.wait(timeout=30)
                with client.stream("POST", self.url, headers=self.headers, json=self.body) as resp:
                    self.status_code = resp.status_code
                    self.content_type = resp.headers.get("content-type", "")
                    if not self.is_sse:
                        resp.read()
                        self.response = resp
                        try:
                            self.json = resp.json()
                        except ValueError:
                            self.json = None
                        return
                    # Keep the event iterator referenced: a dropped (closed)
                    # iterator closes the response stream, i.e. disconnects.
                    events = parse_sse_lines(resp.iter_lines())
                    for name, data in events:
                        self.events.append((name, data))
                        if name in ("stream_started", "done", "error"):
                            break
                    self.ready.set()
                    self._release.wait(timeout=180)
                    # Leaving the context closes the connection: client disconnect.
        except Exception as exc:  # noqa: BLE001 - reported to the test
            self.error = f"{type(exc).__name__}: {exc}"
        finally:
            self.ready.set()

    def wait_ready(self, timeout: float = 30.0) -> "BackgroundSend":
        assert self.ready.wait(timeout), "background stream request produced no outcome"
        assert self.error is None, self.error
        return self

    def release(self) -> None:
        self._release.set()
        self._thread.join(timeout=30)

    def stream_started(self) -> dict[str, Any]:
        assert self.status_code == 200 and self.is_sse, (
            f"expected an SSE stream, got HTTP {self.status_code} {self.json!r}"
        )
        assert self.events and self.events[0][0] == "stream_started", self.events
        return self.events[0][1]


def _start_hanging_turn(mc, mock, chat_id: str, request_id: str | None = None, user: str = "A1") -> BackgroundSend:
    """Start a turn whose provider call hangs; returns once the turn is running."""
    before = len(_stream_requests(mock))
    mock.enqueue(dict(HANG))
    body: dict[str, Any] = {"content": "Please hang"}
    if request_id is not None:
        body["request_id"] = request_id
    sender = BackgroundSend(mc, chat_id, body, user).start().wait_ready()
    sender.stream_started()
    wait_until(
        lambda: len(_stream_requests(mock)) > before,
        timeout=15.0,
        interval=0.05,
        description="the hanging provider call",
    )
    return sender


def _release_and_settle(mc, sender: BackgroundSend, chat_id: str, user: str = "A1") -> str:
    """Disconnect the hanging client and wait until its turn left ``running``."""
    request_id = sender.stream_started()["request_id"]
    sender.release()
    return _wait_turn_left_running(mc, chat_id, request_id, user)


# --------------------------------------------------------------------------
# Replay of a completed turn
# --------------------------------------------------------------------------


def test_replay_completed_turn_streams_persisted_answer(mc, mock):
    user = "A2"
    chat = _new_chat(mc, user)
    chunks = ["Replay ", "me ", "exactly."]
    mock.enqueue({"chunks": chunks, "usage": {"input_tokens": 321, "output_tokens": 54}})
    request_id = str(uuid.uuid4())
    original = _complete_turn(mc, chat["id"], "Original question", request_id, user)
    original_started = original.first("stream_started")
    original_done = original.first("done")

    replay = _post_stream(mc, chat["id"], {"content": "Original question", "request_id": request_id}, user)

    assert replay.status_code == 200, replay.text
    assert replay.is_sse, replay.headers
    assert replay.names() == ["stream_started", "delta", "done"], replay.names()
    started = replay.first("stream_started")
    assert started["request_id"] == request_id
    assert started["is_new_turn"] is False
    assert started["message_id"] == original_started["message_id"]
    assert set(started) <= {"request_id", "message_id", "is_new_turn", "thread_summary_applied"}
    assert replay.first("delta") == {"type": "text", "content": "".join(chunks)}
    done = replay.first("done")
    assert done["usage"] == {"input_tokens": 321, "output_tokens": 54}
    assert done["effective_model"] == original_done["effective_model"] == DEFAULT_MODEL
    assert done["selected_model"] == original_done["selected_model"] == DEFAULT_MODEL
    assert done["quota_decision"] == original_done["quota_decision"] == "allow"
    assert "downgrade_from" not in done, done
    assert "downgrade_reason" not in done, done
    assert "quota_warnings" not in done, f"quota_warnings must be absent on replay: {done}"
    for leaked in ("request_id", "message_id", "provider_response_id"):
        assert leaked not in done


def test_replay_uses_the_persisted_assistant_message(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    original = _complete_turn(mc, chat["id"], "Stored content", request_id)
    message_id = original.first("stream_started")["message_id"]

    messages = mc.get(f"{CHATS}/{chat['id']}/messages", "A1").json()["items"]
    assistant = next(m for m in messages if m["id"] == message_id)

    replay = _post_stream(mc, chat["id"], {"content": "Stored content", "request_id": request_id})

    assert replay.status_code == 200, replay.text
    assert replay.first("delta")["content"] == assistant["content"] == original.text_content()
    assert replay.first("done")["usage"] == {
        "input_tokens": assistant["input_tokens"],
        "output_tokens": assistant["output_tokens"],
    }


def test_replay_is_side_effect_free(mc, mock):
    user = "A2"
    chat = _new_chat(mc, user)
    request_id = str(uuid.uuid4())
    _complete_turn(mc, chat["id"], "Side effects?", request_id, user)
    # Let the original finalization's asynchronous consumers settle first.
    time.sleep(1.0)

    provider_calls = len(_stream_requests(mock))
    turns = _turn_rows(mc, chat["id"])
    messages = _message_rows(mc, chat["id"])
    quota = _quota_rows(mc, user)
    usage_outbox = outbox_snapshot(mc, USAGE_QUEUE)
    audit_outbox = outbox_snapshot(mc, AUDIT_QUEUE)
    high_water = max(usage_outbox.body_high_water, audit_outbox.body_high_water)

    for _ in range(2):
        replay = _post_stream(mc, chat["id"], {"content": "Side effects?", "request_id": request_id}, user)
        assert replay.status_code == 200, replay.text
        assert replay.names() == ["stream_started", "delta", "done"]
        # Checked right away too: a wrongly enqueued body could be vacuumed later.
        assert outbox_payloads_for_chat(mc, chat["id"], since_body_id=high_water) == []
    time.sleep(1.0)

    assert len(_stream_requests(mock)) == provider_calls, "replay must not call the provider"
    assert _turn_rows(mc, chat["id"]) == turns, "replay must not create or change turns"
    assert _message_rows(mc, chat["id"]) == messages, "replay must not create or change messages"
    assert _quota_rows(mc, user) == quota, "replay must not reserve or settle quota"
    new_payloads = outbox_payloads_for_chat(mc, chat["id"], since_body_id=high_water)
    assert new_payloads == [], f"replay enqueued outbox messages: {new_payloads}"


def test_replay_of_server_generated_request_id(mc, mock):
    chat = _new_chat(mc)
    original = _complete_turn(mc, chat["id"], "No key from me")
    request_id = original.first("stream_started")["request_id"]
    provider_calls = len(_stream_requests(mock))

    replay = _post_stream(mc, chat["id"], {"content": "No key from me", "request_id": request_id})

    assert replay.status_code == 200, replay.text
    assert replay.first("stream_started")["is_new_turn"] is False
    assert replay.first("stream_started")["message_id"] == original.first("stream_started")["message_id"]
    assert replay.first("delta")["content"] == original.text_content()
    assert len(_stream_requests(mock)) == provider_calls


def test_replay_ignores_the_resent_content(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    original = _complete_turn(mc, chat["id"], "First wording", request_id)

    replay = _post_stream(mc, chat["id"], {"content": "Different wording", "request_id": request_id})

    assert replay.status_code == 200, replay.text
    assert replay.first("stream_started")["is_new_turn"] is False
    assert replay.first("delta")["content"] == original.text_content()
    assert [m["content"] for m in mc.get(f"{CHATS}/{chat['id']}/messages", "A1").json()["items"]] == [
        "First wording",
        original.text_content(),
    ]


def test_replay_works_while_another_turn_is_running(mc, mock):
    chat = _new_chat(mc)
    first_request = str(uuid.uuid4())
    original = _complete_turn(mc, chat["id"], "Completed before", first_request)

    running = _start_hanging_turn(mc, mock, chat["id"], str(uuid.uuid4()))
    try:
        provider_calls = len(_stream_requests(mock))

        replay = _post_stream(mc, chat["id"], {"content": "Completed before", "request_id": first_request})

        assert replay.status_code == 200, f"replay must win over the parallel guard: {replay.text}"
        assert replay.names() == ["stream_started", "delta", "done"], replay.names()
        assert replay.first("stream_started")["is_new_turn"] is False
        assert replay.first("stream_started")["request_id"] == first_request
        assert replay.first("stream_started")["message_id"] == original.first("stream_started")["message_id"]
        assert replay.first("delta")["content"] == original.text_content()
        assert len(_stream_requests(mock)) == provider_calls
        assert _turn_status(mc, chat["id"], running.stream_started()["request_id"])["state"] == "running"
    finally:
        _release_and_settle(mc, running, chat["id"])


# --------------------------------------------------------------------------
# request_id conflicts
# --------------------------------------------------------------------------


def test_running_turn_request_id_reuse_is_conflict(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    running = _start_hanging_turn(mc, mock, chat["id"], request_id)
    try:
        provider_calls = len(_stream_requests(mock))
        messages = _message_rows(mc, chat["id"])

        result = _post_stream(mc, chat["id"], {"content": "Again", "request_id": request_id})

        _assert_conflict(result, "request_id_conflict")
        assert len(_stream_requests(mock)) == provider_calls
        assert _message_rows(mc, chat["id"]) == messages
        assert len(_turn_rows(mc, chat["id"])) == 1
    finally:
        _release_and_settle(mc, running, chat["id"])


def test_failed_turn_request_id_reuse_is_conflict(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    mock.enqueue({"kind": "http_error", "status": 500})
    failed = _post_stream(mc, chat["id"], {"content": "Will fail", "request_id": request_id})
    assert failed.status_code == 200 and failed.is_sse, failed.text
    assert failed.names()[-1:] == ["error"], failed.names()
    _wait_turn_state(mc, chat["id"], request_id, "error")
    turns = _turn_rows(mc, chat["id"])
    provider_calls = len(_stream_requests(mock))

    result = _post_stream(mc, chat["id"], {"content": "Will fail", "request_id": request_id})

    _assert_conflict(result, "request_id_conflict")
    assert len(_stream_requests(mock)) == provider_calls
    assert _turn_rows(mc, chat["id"]) == turns
    assert turns[0]["state"] == "failed"


def test_cancelled_turn_request_id_reuse_is_conflict(mc, mock):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    running = _start_hanging_turn(mc, mock, chat["id"], request_id)
    running.release()
    _wait_turn_state(mc, chat["id"], request_id, "cancelled")
    turns = _turn_rows(mc, chat["id"])
    provider_calls = len(_stream_requests(mock))

    result = _post_stream(mc, chat["id"], {"content": "Resume?", "request_id": request_id})

    _assert_conflict(result, "request_id_conflict")
    assert len(_stream_requests(mock)) == provider_calls
    assert _turn_rows(mc, chat["id"]) == turns


def test_soft_deleted_turn_request_id_reuse_is_conflict(mc, mock):
    chat = _new_chat(mc)
    ident = mc.identity("A1")
    seeded = seed_turn(mc, chat["id"], ident.tenant_id, ident.user_id, deleted=True)

    result = _post_stream(mc, chat["id"], {"content": "Replaced turn", "request_id": seeded.request_id})

    _assert_conflict(result, "request_id_conflict")
    assert _stream_requests(mock) == []
    assert len(_turn_rows(mc, chat["id"])) == 1
    assert len(_message_rows(mc, chat["id"])) == 2


def test_soft_deleted_completed_turn_is_not_replayed_but_live_one_is(mc, mock):
    chat = _new_chat(mc)
    ident = mc.identity("A1")
    live = seed_turn(mc, chat["id"], ident.tenant_id, ident.user_id, offset_ms=-50, assistant_text="Live answer")
    deleted = seed_turn(mc, chat["id"], ident.tenant_id, ident.user_id, deleted=True)

    replay = _post_stream(mc, chat["id"], {"content": live.user_text, "request_id": live.request_id})
    conflict = _post_stream(mc, chat["id"], {"content": deleted.user_text, "request_id": deleted.request_id})

    assert replay.status_code == 200, replay.text
    assert replay.names() == ["stream_started", "delta", "done"]
    assert replay.first("stream_started")["message_id"] == live.assistant_message_id
    assert replay.first("delta")["content"] == "Live answer"
    assert replay.first("done")["usage"] == {
        "input_tokens": live.input_tokens,
        "output_tokens": live.output_tokens,
    }
    assert replay.first("done")["effective_model"] == live.model
    assert replay.first("done")["selected_model"] == chat["model"]
    _assert_conflict(conflict, "request_id_conflict")
    assert _stream_requests(mock) == []


# --------------------------------------------------------------------------
# Parallel turn guard
# --------------------------------------------------------------------------


def test_parallel_guard_rejects_new_turn_while_one_is_running(mc, mock):
    chat = _new_chat(mc)
    running = _start_hanging_turn(mc, mock, chat["id"], str(uuid.uuid4()))
    try:
        provider_calls = len(_stream_requests(mock))
        messages = _message_rows(mc, chat["id"])
        other_request = str(uuid.uuid4())

        with_id = _post_stream(mc, chat["id"], {"content": "Me too", "request_id": other_request})
        without_id = _post_stream(mc, chat["id"], {"content": "Me three"})

        _assert_conflict(with_id, "turn_already_running")
        _assert_conflict(without_id, "turn_already_running")
        assert len(_stream_requests(mock)) == provider_calls
        assert _message_rows(mc, chat["id"]) == messages, "a rejected send must persist nothing"
        assert len(_turn_rows(mc, chat["id"])) == 1
        assert _turn_status(mc, chat["id"], other_request) is None
    finally:
        _release_and_settle(mc, running, chat["id"])


def test_parallel_guard_is_per_chat(mc, mock):
    busy = _new_chat(mc)
    other = _new_chat(mc)
    running = _start_hanging_turn(mc, mock, busy["id"])
    try:
        result = _complete_turn(mc, other["id"], "Different chat")
        assert result.first("stream_started")["is_new_turn"] is True
    finally:
        _release_and_settle(mc, running, busy["id"])


def test_new_turn_is_accepted_after_running_turn_finished(mc, mock):
    chat = _new_chat(mc)
    running = _start_hanging_turn(mc, mock, chat["id"], str(uuid.uuid4()))
    blocked = _post_stream(mc, chat["id"], {"content": "Too early"})
    _assert_conflict(blocked, "turn_already_running")

    state = _release_and_settle(mc, running, chat["id"])
    assert state != "running"

    result = _complete_turn(mc, chat["id"], "Now it is my turn", str(uuid.uuid4()))
    assert result.first("stream_started")["is_new_turn"] is True
    assert sum(1 for t in _turn_rows(mc, chat["id"]) if t["state"] == "running") == 0
    assert _turn_rows(mc, chat["id"])[-1]["state"] == "completed"


# --------------------------------------------------------------------------
# Concurrent races
# --------------------------------------------------------------------------


def _race(mc, mock, chat_id: str, bodies: list[dict[str, Any]]) -> list[BackgroundSend]:
    mock.enqueue(*[dict(HANG) for _ in bodies])
    barrier = threading.Barrier(len(bodies))
    senders = [BackgroundSend(mc, chat_id, body, barrier=barrier).start() for body in bodies]
    for sender in senders:
        sender.wait_ready(timeout=60)
    return senders


def _split_outcomes(senders: list[BackgroundSend]) -> tuple[list[BackgroundSend], list[BackgroundSend]]:
    winners = [s for s in senders if s.status_code == 200 and s.is_sse]
    losers = [s for s in senders if s not in winners]
    return winners, losers


@pytest.mark.parametrize("attempt", range(3))
def test_concurrent_sends_with_different_request_ids_start_one_turn(mc, mock, attempt):
    chat = _new_chat(mc)
    ids = [str(uuid.uuid4()), str(uuid.uuid4())]
    senders = _race(mc, mock, chat["id"], [{"content": f"Racer {i}", "request_id": rid} for i, rid in enumerate(ids)])
    winners, losers = _split_outcomes(senders)
    try:
        assert len(winners) == 1, [(s.status_code, s.json) for s in senders]
        assert len(losers) == 1
        loser = losers[0]
        assert loser.response is not None, loser.error
        assert_problem(loser.response, 409, category="aborted", reason="turn_already_running")
        winner_id = winners[0].stream_started()["request_id"]
        loser_id = next(rid for rid in ids if rid != winner_id)
        assert winners[0].stream_started()["is_new_turn"] is True
        rows = _turn_rows(mc, chat["id"])
        assert [r["request_id"] for r in rows] == [winner_id], rows
        assert _turn_status(mc, chat["id"], loser_id) is None
        assert [m["request_id"] for m in _message_rows(mc, chat["id"])] == [winner_id]
    finally:
        for winner in winners:
            _release_and_settle(mc, winner, chat["id"])
        for loser in losers:
            loser.release()


@pytest.mark.parametrize("attempt", range(3))
def test_concurrent_sends_with_same_new_request_id_start_one_turn(mc, mock, attempt):
    chat = _new_chat(mc)
    request_id = str(uuid.uuid4())
    senders = _race(
        mc, mock, chat["id"], [{"content": "Same key", "request_id": request_id} for _ in range(2)]
    )
    winners, losers = _split_outcomes(senders)
    try:
        assert len(winners) == 1, [(s.status_code, s.json) for s in senders]
        assert len(losers) == 1
        loser = losers[0]
        assert loser.response is not None, loser.error
        assert_problem(loser.response, 409, category="aborted", reason="request_id_conflict")
        started = winners[0].stream_started()
        assert started["request_id"] == request_id
        assert started["is_new_turn"] is True
        rows = _turn_rows(mc, chat["id"])
        assert [r["request_id"] for r in rows] == [request_id], rows
        user_messages = [m for m in _message_rows(mc, chat["id"]) if m["role"] == "user"]
        assert len(user_messages) == 1, user_messages
        assert len(_stream_requests(mock)) == 1
    finally:
        for winner in winners:
            _release_and_settle(mc, winner, chat["id"])
        for loser in losers:
            loser.release()
