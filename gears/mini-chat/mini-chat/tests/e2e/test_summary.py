"""Thread summary: trigger, provider request, CAS commit, usage, retries, invalidation."""

from __future__ import annotations

import time
import uuid

from conftest import TENANT_A, ok_stream, ub, wait_until

SUMMARY_USER = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
PREAMBLE = "This conversation has earlier messages that have been summarized."


def _fill(api, mock, chat_id, turns=4, size=2400):
    rids = []
    for i in range(turns):
        mock.script({"text": f"answer {i} " + "z" * size})
        rid = str(uuid.uuid4())
        ok_stream(api.send(chat_id, f"question {i} " + "q" * size, request_id=rid))
        rids.append(rid)
    return rids


def _summary_row(server, chat_id):
    rows = server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(chat_id))
    return rows[0] if rows else None


def test_summary_created_and_applied(api, server, mock):
    chat = api.create_chat(model="gpt-tiny")
    _fill(api, mock, chat["id"])
    row = wait_until(lambda: _summary_row(server, chat["id"]), timeout=30, msg="thread summary row")
    assert row["summary_text"] == "Mock summary of the conversation."
    assert row["token_estimate"] == 7  # output_tokens - reasoning_tokens of the summary call

    reqs = mock.summary_requests(chat["id"])
    assert reqs
    body = reqs[0]["json"]
    assert body["model"] == "gpt-4.1-mini-provider"
    assert body["stream"] is False
    assert body["max_output_tokens"] == 4096
    assert body["instructions"].startswith("You are a conversation summarizer.")
    assert body["user"] == uuid.UUID(TENANT_A).hex + uuid.UUID(SUMMARY_USER).hex
    assert body["metadata"]["request_type"] == "summary"
    assert "tools" not in body
    prompt = body["input"][0]["content"][0]["text"]
    assert prompt.startswith("Summarize the following conversation:")
    assert "User: question 0" in prompt
    assert "Assistant: answer 0" in prompt
    assert "<analysis>" in prompt and "<summary>" in prompt
    # the latest turn is never summarized
    assert "question 3" not in prompt

    # covered messages are compressed; the frontier message is covered
    frontier = row["summarized_up_to_message_id"]
    compressed = server.query("SELECT id, is_compressed FROM messages WHERE chat_id = ? ORDER BY created_at", ub(chat["id"]))
    flags = [m["is_compressed"] for m in compressed]
    assert flags[0] == 1
    assert flags[-1] == 0 and flags[-2] == 0
    assert any(m["id"] == frontier and m["is_compressed"] == 1 for m in compressed)
    # messages stay visible to the UI
    assert len(api.messages(chat["id"])) == 8

    # system-task usage event
    ev = wait_until(lambda: server.usage_events(chat_id=chat["id"], billing_outcome="system_task"), msg="summary usage")[0]
    assert ev["requester_type"] == "system"
    assert ev["system_task_type"] == "thread_summary_update"
    assert ev["settlement_method"] == "none"
    assert ev["actual_credits_micro"] == 0
    assert "user_id" not in ev and "turn_id" not in ev
    assert ev["dedupe_key"].startswith(uuid.UUID(TENANT_A).hex + "/thread_summary_update/")
    assert ev["usage"]["output_tokens"] == 7

    # the next turn sends the summary (as a user message with the preamble) and skips compressed messages
    mock.script({"text": "next"})
    r = ok_stream(api.send(chat["id"], "follow up"))
    applied = r.first("stream_started").get("thread_summary_applied")
    assert applied and applied["token_estimate"] > 0
    nxt = mock.chat_requests(chat["id"])[-1]["json"]
    first = nxt["input"][0]
    assert first["role"] == "user"
    assert first["content"][0]["text"].startswith(PREAMBLE)
    assert "Mock summary of the conversation." in first["content"][0]["text"]
    texts = [m["content"][0]["text"] for m in nxt["input"][1:]]
    assert not any(t.startswith("question 0") for t in texts)


def test_summary_retry_after_provider_failure(api, server, mock):
    chat = api.create_chat(model="gpt-tiny")
    mock.summary_script({"http_status": 500}, chat_id=chat["id"])
    _fill(api, mock, chat["id"])
    row = wait_until(lambda: _summary_row(server, chat["id"]), timeout=90, interval=0.5, msg="summary after retry")
    assert row["summary_text"] == "Mock summary of the conversation."
    assert len(mock.summary_requests(chat["id"])) >= 2


def test_summary_without_summary_block_is_retried(api, server, mock):
    chat = api.create_chat(model="gpt-tiny")
    plain = {"text": "Plain summary text."}
    # later summary cycles of the same chat answer with the same text
    mock.summary_script({"text": "<analysis>only analysis</analysis>"}, plain, plain, plain, chat_id=chat["id"])
    _fill(api, mock, chat["id"])
    row = wait_until(lambda: _summary_row(server, chat["id"]), timeout=90, interval=0.5, msg="summary")
    assert row["summary_text"] == "Plain summary text."
    assert len(mock.summary_requests(chat["id"])) >= 2


def test_failed_summary_keeps_state(api, server, mock):
    chat = api.create_chat(model="gpt-tiny")
    mock.summary_script(*[{"http_status": 500} for _ in range(10)], chat_id=chat["id"])
    _fill(api, mock, chat["id"])
    wait_until(lambda: len(mock.summary_requests(chat["id"])) >= 1, timeout=30, msg="summary attempt")
    assert _summary_row(server, chat["id"]) is None
    assert not server.query("SELECT id FROM messages WHERE chat_id = ? AND is_compressed = 1", ub(chat["id"]))


def _settled_summary(server, mock, chat_id):
    """Waits for the summary row and for in-flight summary work of the chat to finish."""
    wait_until(lambda: _summary_row(server, chat_id), timeout=30, msg="summary")
    last = (-1, None)
    stable_since = time.time()
    while time.time() - stable_since < 2.5:
        cur = (len(mock.summary_requests(chat_id)), _summary_row(server, chat_id)["summarized_up_to_message_id"])
        if cur != last:
            last, stable_since = cur, time.time()
        time.sleep(0.25)
    return _summary_row(server, chat_id)


def test_mutation_of_covered_turn_invalidates_summary(api, server, mock):
    chat = api.create_chat(model="gpt-tiny")
    rids = _fill(api, mock, chat["id"])
    row = _settled_summary(server, mock, chat["id"])
    frontier_msg = row["summarized_up_to_message_id"]
    owner = server.query("SELECT request_id FROM messages WHERE id = ?", frontier_msg)[0]["request_id"]
    owner_rid = str(uuid.UUID(bytes=owner))
    assert owner_rid != rids[-1], "the latest turn is never summarized"
    # deleting uncovered latest turns keeps the summary
    for rid in reversed(rids):
        if rid == owner_rid:
            break
        assert api.delete(f"/chats/{chat['id']}/turns/{rid}").status_code == 204
        assert _summary_row(server, chat["id"]) is not None
    # mutating the covered (now latest) turn deletes the summary and clears is_compressed
    mock.script({"text": "retried"})
    ok_stream(api.retry(chat["id"], owner_rid))
    assert _summary_row(server, chat["id"]) is None
    assert not server.query("SELECT id FROM messages WHERE chat_id = ? AND is_compressed = 1", ub(chat["id"]))
