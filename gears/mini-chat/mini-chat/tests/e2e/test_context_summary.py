"""Context assembly (system prompt, guards, summary, recent history,
deterministic truncation) and the thread summary lifecycle."""

from __future__ import annotations

import uuid

import prov
from harness import ub, wait_until

PREAMBLE = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after."


def summary_requests(env):
    return [r for r in env.mock.requests("/responses") if r.get("json", {}).get("stream") is False]


def test_recent_history_limit_and_order(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    for i in range(7):
        c.send(chat["id"], f"q{i}")
    env.mock.reset()
    c.send(chat["id"], "now")
    inp = env.mock.chat_requests()[0]["json"]["input"]
    assert len(inp) == 11  # recent_messages_limit (10) + the new message
    assert inp[0]["role"] == "user" and inp[0]["content"] == [{"type": "input_text", "text": "q2"}] or inp[0]["content"] == "q2"
    texts = [m["content"] if isinstance(m["content"], str) else m["content"][0]["text"] for m in inp]
    assert texts[-1] == "now"
    assert texts[:-1] == [t for i in range(2, 7) for t in (f"q{i}", f"Echo: q{i}")]
    assert [m["role"] for m in inp[:-1]] == ["user", "assistant"] * 5


def test_truncation_summary_trigger_and_application(env):
    c = env.a
    chat = c.create_chat(model="tiny-ctx")
    big = lambda n: (f"turn{n} " + "w" * 2000)[:2000]  # noqa: E731
    c.send(chat["id"], big(1))
    c.send(chat["id"], big(2))
    env.mock.reset()
    r3 = c.send(chat["id"], big(3))
    assert r3.names[-1] == "done"
    # context was truncated deterministically: oldest whole turn dropped, history starts with a user message
    inp = env.mock.chat_requests()[0]["json"]["input"]
    texts = [m["content"] if isinstance(m["content"], str) else m["content"][0]["text"] for m in inp]
    assert texts == [big(2), "Echo: " + big(2), big(3)]
    # the truncation triggers an asynchronous summary of the range before the causing turn
    wait_until(lambda: env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)), 20, msg="summary")
    srow = env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),))[0]
    assert srow["summary_text"] == "Mock summary of the conversation."
    assert srow["token_estimate"] == 40
    msgs = c.messages(chat["id"])
    assert str(uuid.UUID(bytes=srow["summarized_up_to_message_id"])) == msgs[3]["id"]  # assistant of turn 2
    compressed = env.server.query("SELECT is_compressed FROM messages WHERE chat_id = ? ORDER BY created_at", (ub(chat["id"]),))
    assert [x["is_compressed"] for x in compressed] == [1, 1, 1, 1, 0, 0]
    sreq = summary_requests(env)[0]["json"]
    assert sreq["model"] == "prov-gpt-4.1-mini" and sreq["stream"] is False
    assert sreq["instructions"].startswith("You are a conversation summarizer.")
    prompt = sreq["input"][0]["content"][0]["text"] if isinstance(sreq["input"][0]["content"], list) else sreq["input"][0]["content"]
    assert prompt.startswith("Summarize the following conversation:")
    assert f"User: {big(1)}" in prompt and "Assistant: Echo: " + big(2) in prompt
    assert big(3) not in prompt
    assert sreq["metadata"]["request_type"] == "summary"
    # system identity: the chat's tenant and the platform default subject
    assert sreq["user"] == "00000000df515b429538d2b56b7ee953" + "111111116a8847689dfc6bcd5187d9ed"
    wait_until(lambda: [l for l in env.server.log_text().splitlines() if "usage event published" in l and "thread_summary_update" in l], msg="system usage")
    # the next turn uses the summary; compressed messages are not sent
    env.mock.reset()
    r4 = c.send(chat["id"], "short follow-up")
    assert r4.started["thread_summary_applied"] == {"token_estimate": 40}
    inp = env.mock.chat_requests()[0]["json"]["input"]
    first = inp[0]["content"] if isinstance(inp[0]["content"], str) else inp[0]["content"][0]["text"]
    assert inp[0]["role"] == "user" and first.startswith(PREAMBLE) and first.endswith("Mock summary of the conversation.")
    texts = [m["content"] if isinstance(m["content"], str) else m["content"][0]["text"] for m in inp[1:]]
    assert texts == [big(3), "Echo: " + big(3), "short follow-up"]
    # messages remain visible in history
    assert len(c.messages(chat["id"])) == 8


def test_summary_failure_is_retried_then_dead_lettered(env):
    c = env.a
    chat = c.create_chat(model="tiny-ctx")
    big = lambda n: (f"turn{n} " + "w" * 2000)[:2000]  # noqa: E731
    c.send(chat["id"], big(1))
    c.send(chat["id"], big(2))
    env.mock.reset()
    err = {"kind": "error", "status": 500, "body": {"error": {"message": "summary failed"}}}
    env.mock.script([prov.text_reply("Echo answer"), err, err, err])
    c.send(chat["id"], big(3))
    wait_until(lambda: len(summary_requests(env)) >= 3, 40, 0.5, "3 summary attempts")
    import time

    time.sleep(3)
    assert len(summary_requests(env)) == 3  # thread_summary_worker.max_attempts
    assert not env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),))
    assert all(x["is_compressed"] == 0 for x in env.server.query("SELECT is_compressed FROM messages WHERE chat_id = ?", (ub(chat["id"]),)))
    # the chat keeps working without a summary
    r = c.send(chat["id"], "still fine")
    assert r.names[-1] == "done" and "thread_summary_applied" not in r.started


def test_mutation_invalidates_covering_summary(env):
    c = env.a
    chat = c.create_chat(model="tiny-ctx")
    big = lambda n: (f"turn{n} " + "w" * 2000)[:2000]  # noqa: E731
    rid1 = c.send(chat["id"], big(1)).started["request_id"]
    rid2 = c.send(chat["id"], big(2)).started["request_id"]
    rid3 = c.send(chat["id"], big(3)).started["request_id"]
    wait_until(lambda: env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),)), 20, msg="summary")
    # deleting the causing turn keeps the summary (it does not cover it)
    assert c.delete(f"/chats/{chat['id']}/turns/{rid3}").status_code == 204
    assert env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),))
    # turn 2 is now the latest and covered by the summary: deleting it drops the summary
    assert c.delete(f"/chats/{chat['id']}/turns/{rid2}").status_code == 204
    assert not env.server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat["id"]),))
    assert all(x["is_compressed"] == 0 for x in env.server.query("SELECT is_compressed FROM messages WHERE chat_id = ?", (ub(chat["id"]),)))
    env.mock.reset()
    r = c.send(chat["id"], "after invalidation")
    assert "thread_summary_applied" not in r.started
    inp = env.mock.chat_requests()[0]["json"]["input"]
    texts = [m["content"] if isinstance(m["content"], str) else m["content"][0]["text"] for m in inp]
    assert texts == [big(1), "Echo: " + big(1), "after invalidation"]
    assert rid1


def test_guards_and_tool_availability(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    c.send(chat["id"], "plain")
    body = env.mock.chat_requests()[0]["json"]
    assert body["instructions"] == "SYSTEM PROMPT premium-1"
    assert "tools" not in body or not body["tools"]
    env.mock.reset()
    c.send(chat["id"], "with search", web_search={"enabled": True})
    body = env.mock.chat_requests()[0]["json"]
    assert body["instructions"].startswith("SYSTEM PROMPT premium-1\n\nUse web_search only if")
    # a model without web search support: the tool and its guard are skipped
    nov = c.create_chat(model="std-novision")
    env.mock.reset()
    r = c.send(nov["id"], "with search", web_search={"enabled": True})
    assert r.names[-1] == "done"
    body = env.mock.chat_requests()[0]["json"]
    assert "tools" not in body or not body["tools"]
    assert "web_search" not in body["instructions"]
