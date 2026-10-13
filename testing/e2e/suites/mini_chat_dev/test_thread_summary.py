"""Thread summary worker (DESIGN section 3.6 "Thread Summary Update", section 3.2 system tasks).

`gpt-tiny` has an effective input budget of 3072 tokens, so the proactive trigger fires at
80% = 2458 assembled tokens. Items are estimated as ceil((ceil(bytes / 4) + 100) * 1.1):
a 4000-byte assistant reply is 1210 tokens, a short user message 112, the system prompt 118.

* turn 1 (q1 -> 4000-byte a1), turn 2 (q2 -> 4000-byte a2): turn 2 assembles ~1550 tokens.
* turn 3 (q3): 118 + 112 + 1210 + 112 + 1210 + 112 = 2874 >= 2458 (and within the 2972-token
  context budget, so nothing is truncated) -> a summary of q1..a2 is enqueued at finalization.
* turn 4 (q4) runs with the summary in place of q1..a2.
"""

from contextlib import closing

import pytest

from . import mock_provider as mp
from .helpers import api, captured_outbox, create_chat, db, stream, uuid_bytes, wait_until

pytestmark = pytest.mark.usefixtures("server")

SYSTEM_SUBJECT_HEX = "111111116a8847689dfc6bcd5187d9ed"
PREAMBLE = ("This conversation has earlier messages that have been summarized. The summary below "
            "covers the earlier portion of the conversation. Recent messages follow after.")
#: Capture table of the ``outbox_capture`` fixture (conftest.py).
CAPTURE = "e2e_summary_outbox_capture"


def _messages(chat_id: str) -> list[tuple[str, str, int]]:
    with closing(db()) as conn:
        rows = conn.execute(
            "SELECT role, content, is_compressed FROM messages WHERE chat_id = ? AND deleted_at IS NULL "
            "ORDER BY created_at, id", (uuid_bytes(chat_id),)).fetchall()
    return [(r["role"], r["content"], r["is_compressed"]) for r in rows]


def _summary_row(chat_id: str):
    with closing(db()) as conn:
        return conn.execute(
            "SELECT summary_text, token_estimate, summarized_up_to_message_id FROM thread_summaries "
            "WHERE chat_id = ?", (uuid_bytes(chat_id),)).fetchone()


def _system_usage_events(chat_id: str) -> list[dict]:
    events = [body for _, _, body in captured_outbox(CAPTURE, "mini-chat.usage.v1")]
    return [e for e in events if e.get("chat_id") == chat_id and e.get("requester_type") == "system"]


def _turn(s, chat_id: str, content: str):
    res = stream(s, chat_id, {"content": content})
    assert res.terminal and res.terminal[0] == "done", res.raw
    return res


def test_summary_is_generated_and_applied_to_the_next_turn(reset_mock, outbox_capture):
    s = api()
    chat = create_chat(s, model="gpt-tiny")
    a1, a2 = "A" * 4000, "B" * 4000
    reset_mock.enqueue("responses", {"events": mp.text_events(a1)})
    reset_mock.enqueue("responses", {"events": mp.text_events(a2)})

    _turn(s, chat["id"], "q1")
    _turn(s, chat["id"], "q2")
    assert reset_mock.requests(route="summary") == []
    _turn(s, chat["id"], "q3")

    # The worker's non-streaming summary request reaches the mock.
    reqs = wait_until(lambda: reset_mock.requests(route="summary"), timeout=30,
                      message="summary request")
    body = reqs[0]["json"]
    assert body["model"] == "gpt-4.1-mini"
    assert not body.get("stream")
    assert body["metadata"]["request_type"] == "summary"
    assert body["metadata"]["feature"] == "none"
    assert body["metadata"]["chat_id"] == chat["id"]
    assert len(body["user"]) == 64 and body["user"].endswith(SYSTEM_SUBJECT_HEX)
    prompt = body["input"][0]["content"][0]["text"]
    assert prompt.startswith("Summarize the following conversation:\n\nUser: q1\n\nAssistant: AAAA")
    assert "User: q2" in prompt and "Assistant: BBBB" in prompt
    assert "q3" not in prompt  # the causing turn is never summarized

    row = wait_until(lambda: _summary_row(chat["id"]), timeout=30, message="thread_summaries row")
    assert row["summary_text"] == "Summary of the conversation so far."
    assert row["token_estimate"] == 8  # mock usage: output_tokens 8, no reasoning
    assert _messages(chat["id"]) == [
        ("user", "q1", 1), ("assistant", a1, 1),
        ("user", "q2", 1), ("assistant", a2, 1),
        ("user", "q3", 0), ("assistant", "Hello world", 0),
    ]

    ev = wait_until(lambda: _system_usage_events(chat["id"]), timeout=15,
                    message="system usage event")[0]
    assert ev["billing_outcome"] == "system_task"
    assert ev["settlement_method"] == "none"
    assert ev["actual_credits_micro"] == 0
    assert ev["system_task_type"] == "thread_summary_update"
    assert ev["effective_model"] == "gpt-4.1-mini"
    assert "user_id" not in ev and "turn_id" not in ev
    assert ev["dedupe_key"].split("/")[1] == "thread_summary_update"

    # Next turn: the summary replaces the compressed history.
    res = _turn(s, chat["id"], "q4")
    started = res.of("stream_started")[0]
    assert started["thread_summary_applied"] == {"token_estimate": 8}
    chat_req = reset_mock.requests(route="responses")[-1]["json"]
    first = chat_req["input"][0]["content"][0]["text"]
    assert first.startswith(PREAMBLE)
    assert first.endswith("Summary of the conversation so far.")
    texts = [m["content"][0]["text"] for m in chat_req["input"][1:]]
    assert texts == ["q3", "Hello world", "q4"]
