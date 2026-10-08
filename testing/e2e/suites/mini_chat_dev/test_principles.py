"""Cross-cutting principles and constraints (DESIGN section 2.2): model locked per chat, quota
before outbound, no buffering, context window budget."""

import json
import time
from contextlib import closing

import pytest

from . import mock_provider as mp
from .helpers import (PREFIX, api, assert_problem, create_chat, db, exhausted_quota, parse_sse, reserved_credits, stream,
                      uuid_bytes)

pytestmark = pytest.mark.usefixtures("server")


def _chat_row(chat_id: str):
    with closing(db()) as conn:
        return conn.execute("SELECT model FROM chats WHERE id = ?", (uuid_bytes(chat_id),)).fetchone()


def _turns(chat_id: str) -> list:
    with closing(db()) as conn:
        return conn.execute("SELECT request_id, state FROM chat_turns WHERE chat_id = ?",
                            (uuid_bytes(chat_id),)).fetchall()


# ── Model locked per chat ────────────────────────────────────────────────────


def test_chat_model_is_immutable(reset_mock):
    s = api()
    chat = create_chat(s, model="gpt-standard")
    url = f"{PREFIX}/chats/{chat['id']}"

    r = s.patch(url, json={"title": "renamed", "model": "gpt-premium"})
    assert r.status_code == 200, r.text
    assert r.json()["model"] == "gpt-standard"
    assert_problem(s.patch(url, json={"model": "gpt-premium"}), 422, "invalid_argument")
    assert s.get(url).json()["model"] == "gpt-standard"
    assert _chat_row(chat["id"])["model"] == "gpt-standard"

    res = stream(s, chat["id"], {"content": "hi", "model": "gpt-premium"})  # unknown body field: ignored
    assert res.terminal[0] == "done", res.raw
    done = res.of("done")[0]
    assert done["selected_model"] == done["effective_model"] == "gpt-standard"
    assert reset_mock.requests(path="/responses")[-1]["json"]["model"] == "gpt-standard"
    assert s.get(url).json()["model"] == "gpt-standard"


def test_downgrade_changes_the_effective_model_only(reset_mock):
    s = api()
    chat = create_chat(s)  # default: gpt-premium
    assert stream(s, chat["id"], {"content": "first"}).terminal[0] == "done"
    with exhausted_quota(chat["id"], "bucket = 'tier:premium'"):
        res = stream(s, chat["id"], {"content": "second"})
    assert res.terminal[0] == "done", res.raw
    done = res.of("done")[0]
    assert done["selected_model"] == "gpt-premium"
    assert done["effective_model"] == "gpt-standard"
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_from"] == "gpt-premium"
    assert reset_mock.requests(path="/responses")[-1]["json"]["model"] == "gpt-standard"
    # The chat keeps its selected model; the effective model is recorded on the message.
    assert s.get(f"{PREFIX}/chats/{chat['id']}").json()["model"] == "gpt-premium"
    assert _chat_row(chat["id"])["model"] == "gpt-premium"
    items = s.get(f"{PREFIX}/chats/{chat['id']}/messages").json()["items"]
    assert [m.get("model") for m in items if m["role"] == "assistant"] == ["gpt-premium", "gpt-standard"]


# ── Quota before outbound ────────────────────────────────────────────────────


def test_exhausted_quota_is_429_before_any_provider_call(reset_mock):
    s = api()
    chat = create_chat(s)
    assert stream(s, chat["id"], {"content": "creates the quota rows"}).terminal[0] == "done"
    calls = len(reset_mock.requests(path="/responses"))
    turns = len(_turns(chat["id"]))
    reserved = reserved_credits(chat["id"])
    with exhausted_quota(chat["id"]):
        r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "one more"},
                   headers={"Accept": "text/event-stream"})
        p = assert_problem(r, 429, "resource_exhausted")
        assert "text/event-stream" not in r.headers.get("Content-Type", "")
        assert p["context"]["violations"], p
        assert len(reset_mock.requests(path="/responses")) == calls, "the provider was called"
        assert len(_turns(chat["id"])) == turns, "a turn was started"
        assert reserved_credits(chat["id"]) == reserved, "a reserve was booked"


# ── No buffering ─────────────────────────────────────────────────────────────


def test_stream_is_relayed_without_buffering(reset_mock):
    gap_ms = 500
    reset_mock.enqueue("responses", {"delay_ms": gap_ms, "events": [
        mp.ev_created(), mp.ev_delta("one "), mp.ev_delta("two "), mp.ev_delta("three "), mp.ev_delta("four"),
        mp.ev_completed()]})
    s = api()
    chat = create_chat(s)
    start = time.monotonic()
    arrivals: list[tuple[str, float]] = []
    buf = ""
    with s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "count"},
                headers={"Accept": "text/event-stream"}, stream=True, timeout=60) as r:
        assert r.status_code == 200, r.text
        for chunk in r.iter_content(chunk_size=None, decode_unicode=True):
            buf += chunk
            while "\n\n" in buf:
                block, buf = buf.split("\n\n", 1)
                for name, _ in parse_sse(block + "\n\n"):
                    arrivals.append((name, time.monotonic() - start))
    names = [n for n, _ in arrivals]
    assert names == ["stream_started", "delta", "delta", "delta", "delta", "done"], names
    deltas = [t for n, t in arrivals if n == "delta"]
    done_at = arrivals[-1][1]
    # The mock needs 5 gaps (~2.5 s) to finish; the first delta must reach the client while
    # the provider is still streaming (>= 2.4 gaps, i.e. 3 gaps less 20 % tolerance, before the end), not when it is done.
    assert done_at - deltas[0] >= 3 * gap_ms / 1000 * 0.8, arrivals
    # Every delta is relayed on its own, spaced like the provider sent them.
    for a, b in zip(deltas, deltas[1:]):
        assert b - a >= gap_ms / 1000 * 0.6, arrivals
    req = reset_mock.requests(path="/responses")[-1]
    assert req["events_sent"] == 6


# ── Context window budget ────────────────────────────────────────────────────

# gpt-tiny: context_window 4096, max_output_tokens 1024, max_input_tokens 3072; estimation
# budgets: 4 bytes/token, fixed overhead 100, safety margin 10 %.
#   INPUT_TOO_LONG          - message estimate ceil((ceil(n/4) + 100) * 1.1) > 3072  (n > 10768)
#   CONTEXT_BUDGET_EXCEEDED - token budget min(3072, 4096 - 1024) - 100 = 2972; system prompt
#                             ("You are a helpful assistant.", 118 tokens) + message > 2972.
# 10 400 bytes -> 2970 tokens: within max_input_tokens, but the assembled request does not fit.
@pytest.mark.parametrize("size,reason", [(12_000, "INPUT_TOO_LONG"), (10_400, "CONTEXT_BUDGET_EXCEEDED")])
def test_context_budget_is_enforced_for_message_and_assembled_request(reset_mock, size, reason):
    s = api()
    chat = create_chat(s, model="gpt-tiny")
    reserved = reserved_credits(chat["id"])
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "x" * size},
               headers={"Accept": "text/event-stream"})
    assert_problem(r, 400, "out_of_range", reason=reason, field="content")
    assert reset_mock.requests(path="/responses") == []
    assert _turns(chat["id"]) == []
    assert reserved_credits(chat["id"]) == reserved


def test_message_just_within_the_budget_is_sent(reset_mock):
    s = api()
    chat = create_chat(s, model="gpt-tiny")
    # 9 900 bytes -> 2833 tokens + 118 system prompt = 2951 <= 2972.
    res = stream(s, chat["id"], {"content": "y" * 9_900})
    assert res.terminal[0] == "done", res.raw
    body = reset_mock.requests(path="/responses")[-1]["json"]
    assert json.dumps(body["input"]).count("y" * 9_900) == 1
