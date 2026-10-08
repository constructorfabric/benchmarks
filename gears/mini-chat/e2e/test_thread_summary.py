"""Thread summary: generation, application in the next context, failure/retry, invalidation by mutations.

Acceptance criteria covered:
* Cleanup & Recovery — "Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly"
* Context Assembly — "System prompt, thread summary, and recent history are assembled and truncated deterministically within budget"
"""

from __future__ import annotations

import json
import time
import uuid

from helpers import (
    as_uuid,
    http_error,
    input_pairs,
    list_messages,
    message_rows,
    new_chat,
    rows,
    send_ok,
    tenant_id,
    ub,
    usage_events,
    wait_until,
)

PREAMBLE = "This conversation has earlier messages that have been summarized."


def big(tag: str) -> str:
    # ≈ 1500 estimated tokens: turn 2's context (≈ 3230) reaches the 80 % threshold of the tiny-ctx
    # input budget (3072) and also overflows it, so either the proactive or the urgent trigger fires.
    return tag + ("." * 5050)


def summary_row(srv, cid):
    rs = rows(srv, "SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(cid),))
    return rs[0] if rs else None


def _two_big_turns(srv):
    """Turn 2's assembled context exceeds 80 % of the tiny-ctx budget → proactive summary of turn 1."""
    cid = new_chat(srv, "tiny-ctx")
    tag = uuid.uuid4().hex[:8]
    s1, _, _ = send_ok(srv, cid, big(f"AAA{tag}"))
    s2, _, _ = send_ok(srv, cid, big(f"BBB{tag}"))
    s1["tag"] = s2["tag"] = tag
    return cid, s1, s2


def summary_requests_for(srv, tag):
    return [b for b in srv.summary_requests() if f"AAA{tag}" in json.dumps(b)]


def test_summary_generated_applied_and_invalidated(fresh):
    cid, s1, s2 = _two_big_turns(fresh)
    row = wait_until(lambda: summary_row(fresh, cid), timeout=20)
    assert row, "thread summary was not generated"
    assert row["summary_text"] == "Conversation summary text."
    assert int(row["token_estimate"]) == 40, "output_tokens - reasoning_tokens of the summary call"
    assert as_uuid(row["summarized_up_to_message_id"]) == s1["message_id"], "frozen target = last message before the causing turn"
    # The summary request: non-streaming, summary model, request_type summary, conversation in the prompt.
    sreqs = summary_requests_for(fresh, s1["tag"])
    assert sreqs, "no summary request"
    sb = sreqs[-1]
    assert sb.get("stream") in (False, None)
    assert sb["model"] == "gpt-4.1-mini"
    assert sb["metadata"]["request_type"] == "summary"
    prompt = " ".join(t for _, t in input_pairs(sb)) + " " + (sb.get("instructions") or "")
    assert f"User: AAA{s1['tag']}" in prompt and f"BBB{s1['tag']}" not in prompt, "only the frozen range is summarized"
    # The summarized range is marked compressed; the rest is not.
    msgs = {as_uuid(m["id"]): m for m in message_rows(fresh, cid)}
    assert msgs[s1["message_id"]]["is_compressed"] in (1, True)
    assert msgs[s2["message_id"]]["is_compressed"] in (0, False)
    # UI still shows all messages.
    assert len(list_messages(fresh, cid)) == 4
    # System usage event for the summary task.
    sys_ev = wait_until(lambda: [e for e in usage_events(fresh) if e.get("chat_id") == cid and e.get("billing_outcome") == "system_task"], timeout=10)
    assert sys_ev, "no system_task usage event"
    e = sys_ev[0]
    assert e["settlement_method"] == "none" and e["requester_type"] == "system"
    assert "user_id" not in e and "turn_id" not in e
    assert e["system_task_type"] == "thread_summary_update"
    assert e["actual_credits_micro"] == 0
    assert e["dedupe_key"].startswith(f"{uuid.UUID(tenant_id('a1')).hex}/thread_summary_update/")

    # Next turn carries the summary.
    fresh.mock_reset()
    s3, _, _ = send_ok(fresh, cid, "short follow-up")
    assert s3.get("thread_summary_applied", {}).get("token_estimate") == 40
    pairs = input_pairs(fresh.chat_requests()[-1])
    assert pairs[0][0] == "user" and "Conversation summary text." in pairs[0][1]
    assert PREAMBLE in pairs[0][1]
    texts = [t for _, t in pairs]
    assert not any(t.startswith("AAA") for t in texts), "compressed messages are not re-sent"
    assert any(t.startswith(f"BBB{s1['tag']}") for t in texts)
    assert texts[-1] == "short follow-up"

    # Mutations: deleting turns not covered keeps the summary; deleting the covered latest turn drops it.
    assert fresh.req("DELETE", f"/chats/{cid}/turns/{s3['request_id']}").status_code == 204
    assert summary_row(fresh, cid) is not None
    assert fresh.req("DELETE", f"/chats/{cid}/turns/{s2['request_id']}").status_code == 204
    assert summary_row(fresh, cid) is not None
    assert fresh.req("DELETE", f"/chats/{cid}/turns/{s1['request_id']}").status_code == 204
    assert summary_row(fresh, cid) is None, "summary covering the deleted turn is removed"
    assert all(m["is_compressed"] in (0, False) for m in message_rows(fresh, cid)), "is_compressed cleared"


def test_retry_of_covered_turn_invalidates_summary(fresh):
    cid, s1, s2 = _two_big_turns(fresh)
    assert wait_until(lambda: summary_row(fresh, cid), timeout=20)
    assert fresh.req("DELETE", f"/chats/{cid}/turns/{s2['request_id']}").status_code == 204
    r, events = fresh.sse("POST", f"/chats/{cid}/turns/{s1['request_id']}/retry")
    assert events and events[-1].event == "done", r.text[:500]
    assert "thread_summary_applied" not in events[0].data
    assert summary_row(fresh, cid) is None
    assert all(m["is_compressed"] in (0, False) for m in message_rows(fresh, cid))


def test_no_summary_for_small_context(fresh):
    cid = new_chat(fresh, "tiny-ctx")
    send_ok(fresh, cid, "small one")
    send_ok(fresh, cid, "small two")
    time.sleep(2)
    assert summary_row(fresh, cid) is None
    assert not [b for b in fresh.summary_requests() if "small one" in json.dumps(b)]


def test_summary_failure_is_retried(fresh):
    fresh.mock_script(http_error(500, "summary provider down", match={"stream": False}))
    cid, s1, _ = _two_big_turns(fresh)
    row = wait_until(lambda: summary_row(fresh, cid), timeout=30)
    assert row, "summary not generated after a transient provider failure"
    assert len(summary_requests_for(fresh, s1["tag"])) >= 2


def test_summary_permanent_failure_keeps_state(fresh):
    for _ in range(5):
        fresh.mock_script(http_error(500, "summary provider down", match={"stream": False}))
    cid, s1, _ = _two_big_turns(fresh)
    tag = s1["tag"]
    assert wait_until(lambda: len(summary_requests_for(fresh, tag)) >= 3, timeout=30), fresh.summary_requests()
    time.sleep(2)
    assert summary_row(fresh, cid) is None
    assert all(m["is_compressed"] in (0, False) for m in message_rows(fresh, cid))
    n = len(summary_requests_for(fresh, tag))
    time.sleep(2)
    assert len(summary_requests_for(fresh, tag)) == n <= 3, "bounded by thread_summary_worker.max_attempts (3)"
    # Turns keep working without a summary.
    s3, _, _ = send_ok(fresh, cid, "still fine")
    assert "thread_summary_applied" not in s3
