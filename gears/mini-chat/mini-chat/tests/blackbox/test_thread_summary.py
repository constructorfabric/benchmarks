"""US7 black-box: thread summary on the tiny-context model.

``gpt-4.1-mini-tiny-ctx`` has ``max_input_tokens = 3072`` and ``max_output_tokens = 1024``
(effective input budget 3072). Each turn below adds ~600 + ~560 tokens of history, so the
80% compression threshold is crossed after a few turns and the finalizer enqueues a
``mini-chat.thread_summary`` outbox message; the worker calls the provider
(non-streaming, ``request_type = summary``) and commits a ``thread_summaries`` row.
"""

import json

import yaml

from conftest import TENANT_A, ub, us, wait_until
from server import ROOT

MODEL = "gpt-4.1-mini-tiny-ctx"
SYSTEM_USER = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
MINI_CHAT_PATCH = {"thread_summary_worker": {"enabled": True, "summary_model_id": "gpt-4.1-mini", "compression_threshold_pct": 80}}

with open(f"{ROOT}/config/mini-chat.yaml") as _f:
    SUMMARY_PROMPT = yaml.safe_load(_f)["gears"]["mini-chat"]["config"]["thread_summary_worker"]["summary_system_prompt"]


def _question(i):
    return f"question {i} " + "x" * 2400


def _answer(i):
    return f"answer {i} " + "lorem ipsum dolor " * 120


def _turn(api, mock, chat_id, i):
    mock.push({"text": _answer(i), "usage": {"input_tokens": 500, "output_tokens": 600}})
    res = api.send(chat_id, _question(i))
    assert res.status == 200 and res.names[-1] == "done", res
    return res


def _summary_tasks(db, chat_id):
    """Thread-summary outbox payloads enqueued for ``chat_id`` (finalize transaction)."""
    out = []
    for r in db.rows("SELECT payload FROM toolkit_outbox_body"):
        p = json.loads(bytes(r["payload"]))
        if p.get("chat_id") == chat_id and "frozen_target_message_id" in p:
            out.append(p)
    return out


def _drive_until_summary(api, mock, db, chat_id, max_turns=6):
    """Send turns until a summary is scheduled, then wait for the worker's commit."""
    turns = []
    for i in range(max_turns):
        turns.append(_turn(api, mock, chat_id, i))
        scheduled = (
            _summary_tasks(db, chat_id)
            or [r for r in mock.summary_requests() if r["json"]["metadata"]["chat_id"] == chat_id]
            or db.rows("SELECT 1 FROM thread_summaries WHERE chat_id = ?", ub(chat_id))
        )
        if scheduled:
            wait_until(
                lambda: db.rows("SELECT 1 FROM thread_summaries WHERE chat_id = ?", ub(chat_id)),
                timeout=30,
                desc="thread summary commit",
            )
            return turns
    raise AssertionError("thread summary never triggered")


def test_tiny_context_turns_trigger_thread_summary(api, mock, db):
    c = api.create_chat(model=MODEL)
    mock.push_summary({
        "text": "<analysis>reasoning that must not be stored</analysis><summary>The user asked numbered questions; the assistant answered with lorem ipsum.</summary>",
        "usage": {"input_tokens": 1500, "output_tokens": 42},
    })
    turns = _drive_until_summary(api, mock, db, c["id"])
    assert 2 <= len(turns) <= 5, f"summary after {len(turns)} turns"
    trigger = turns[-1]

    # --- the durable outbox task (no provider identifiers, stable system identity)
    tasks = _summary_tasks(db, c["id"])
    if tasks:  # bodies may already be vacuumed after delivery
        t = tasks[0]
        assert t["tenant_id"] == TENANT_A and t["system_task_type"] == "thread_summary_update"
        assert t["base_frontier_message_id"] is None
        assert "resp_" not in json.dumps(t)

    # --- the summary provider call
    sreqs = mock.summary_requests()
    assert len(sreqs) >= 1
    body = sreqs[0]["json"]
    assert body["stream"] is False
    assert body["model"] == "gpt-4.1-mini"  # summary_model_id -> provider_model_id
    assert body["metadata"] == {
        "tenant_id": TENANT_A,
        "user_id": SYSTEM_USER,
        "chat_id": c["id"],
        "request_type": "summary",
        "feature": "none",
    }
    assert body["instructions"] == SUMMARY_PROMPT
    assert "tools" not in body
    assert len(body["input"]) == 1 and body["input"][0]["role"] == "user"
    prompt = body["input"][0]["content"]
    assert "question 0" in prompt and "answer 0" in prompt
    # the triggering turn is never part of its own summary
    assert f"question {len(turns) - 1} " not in prompt

    # --- committed state
    row = db.one("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"]))
    assert row["summary_text"] == "The user asked numbered questions; the assistant answered with lorem ipsum."
    assert row["token_estimate"] == 42  # output_tokens - reasoning_tokens
    assert row["tenant_id"] == ub(TENANT_A)
    frontier = us(row["summarized_up_to_message_id"])
    live = db.messages(c["id"], include_deleted=False)
    ids = [us(m["id"]) for m in live]
    assert frontier in ids
    # frontier = last message before the triggering turn (its user message is after it)
    trig_user = next(m for m in live if us(m["request_id"]) == trigger.request_id and m["role"] == "user")
    idx = ids.index(frontier)
    assert ids.index(us(trig_user["id"])) == idx + 1
    # exactly the summarized range is compressed
    assert [m["is_compressed"] for m in live[: idx + 1]] == [1] * (idx + 1)
    assert all(m["is_compressed"] == 0 for m in live[idx + 1:])
    # the summary is not a message; the UI still sees all messages
    assert len(api.messages(c["id"], limit=100)) == len(live)

    # --- the next turn uses the summary
    nxt = _turn(api, mock, c["id"], 99)
    sent = mock.chat_requests()[-1]["json"]["input"]
    # DESIGN §SSE stream_started: carries the summary's stored token_estimate
    assert nxt.started.get("thread_summary_applied") == {"token_estimate": row["token_estimate"]}
    assert sent[0]["role"] == "user"
    assert sent[0]["content"].endswith("The user asked numbered questions; the assistant answered with lorem ipsum.")
    assert "summarized" in sent[0]["content"]
    flat = " ".join(m["content"] if isinstance(m["content"], str) else "" for m in sent)
    assert "question 0 " not in flat  # compressed history is not re-sent
    assert sent[-1] == {"role": "user", "content": _question(99)}
    # no reasoning block leaks into stored or sent text
    assert "reasoning that must not be stored" not in flat


def test_first_turn_on_tiny_model_has_no_summary(api, mock, db):
    c = api.create_chat(model=MODEL)
    res = _turn(api, mock, c["id"], 0)
    assert "thread_summary_applied" not in res.started
    assert db.rows("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"])) == []


def test_summary_provider_failure_keeps_state_then_retries(api, mock, db):
    c = api.create_chat(model=MODEL)
    mock.push_summary({"status": 500, "message": "summary backend down"})
    mock.push_summary({"text": "<summary>Recovered summary.</summary>"})
    _drive_until_summary(api, mock, db, c["id"])
    row = wait_until(
        lambda: db.rows("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"])),
        timeout=30,
        desc="summary after retry",
    )[0]
    assert row["summary_text"] == "Recovered summary."
    assert len(mock.summary_requests()) >= 2


def test_mutation_covered_by_summary_drops_it(api, mock, db):
    c = api.create_chat(model=MODEL)
    mock.push_summary({"text": "<summary>S1</summary>"})
    turns = _drive_until_summary(api, mock, db, c["id"])
    row = db.one("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"]))
    trigger, previous = turns[-1], turns[-2]

    # deleting the triggering turn: the summary does not cover it -> kept
    assert api.delete_turn(c["id"], trigger.request_id).status_code == 204
    assert db.rows("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"])) == [row]

    # deleting the (now latest) turn that the summary covers -> summary dropped,
    # every message of the chat uncompressed
    assert api.delete_turn(c["id"], previous.request_id).status_code == 204
    assert db.rows("SELECT * FROM thread_summaries WHERE chat_id = ?", ub(c["id"])) == []
    assert all(m["is_compressed"] == 0 for m in db.messages(c["id"]))

    # the next turn assembles uncompressed history (no summary applied)
    mock.push({"text": "short", "usage": {"input_tokens": 1, "output_tokens": 1}})
    res = api.send(c["id"], "short question")
    assert "thread_summary_applied" not in res.started
    sent = mock.chat_requests()[-1]["json"]["input"]
    assert not sent[0]["content"].startswith("This conversation has earlier messages")
