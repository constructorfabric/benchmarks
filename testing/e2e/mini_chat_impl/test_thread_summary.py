"""Thread summary (acceptance: Cleanup & Recovery — thread summary generation,
failure/retry and mutation-driven invalidation; Context Assembly — summary)."""

import json

import pytest

from mchelpers import SUMMARY_PREAMBLE, nonce, request_input_roles_texts, text_blob, ub, wait_for
from mock_llm import SUMMARY_MARKER


def _summary_row(db, chat_id):
    return db.one("SELECT * FROM thread_summaries WHERE chat_id = ?", (ub(chat_id),))


def _two_big_turns(api, a_marker, b_marker):
    """On std-tiny (input budget ~3000 tokens) two ~6 KB turns force truncation -> summary trigger."""
    c = api.create_chat(model="std-tiny")
    s1 = api.stream(c["id"], text_blob(6000, a_marker))
    assert s1.done
    s2 = api.stream(c["id"], text_blob(6000, b_marker))
    assert s2.done
    return c, s1, s2


# Acceptance: Cleanup & Recovery / Context assembly — summary generated, stored and used by the next turn
@pytest.mark.timeout(120)
def test_summary_generated_and_applied(api_for, db, mock_llm):
    a = api_for("tok-k2")
    am, bm = "SUM-A-" + nonce(), "SUM-B-" + nonce()
    c, s1, s2 = _two_big_turns(a, am, bm)
    quota_after_turns = db.quota_snapshot("tok-k2")
    row = wait_for(lambda: _summary_row(db, c["id"]), timeout=60, interval=0.5, desc="thread_summaries row")
    assert SUMMARY_MARKER in row["summary_text"]
    assert "<analysis>" not in row["summary_text"] and "<summary>" not in row["summary_text"]
    assert row["token_estimate"] is not None and int(row["token_estimate"]) > 0
    # the summary call: non-streaming, summary model, conversation of the summarized range
    sreqs = mock_llm.summary_requests(contains=am)
    assert sreqs, "the summary worker must call the provider"
    sb = sreqs[-1]["json"]
    assert sb["model"] == "mock-std-tiny"
    assert not sb.get("stream")
    assert bm not in sreqs[-1]["body_text"], "the causing turn is never summarized"
    # frontier: the assistant message of turn 1; its range is marked compressed
    msgs = db.message_rows(c["id"])
    by_req = {}
    for m in msgs:
        by_req.setdefault(bytes(m["request_id"]), []).append(m)
    t1 = by_req[ub(s1.request_id)]
    t2 = by_req[ub(s2.request_id)]
    assert all(m["is_compressed"] in (1, True) for m in t1)
    assert all(m["is_compressed"] in (0, False) for m in t2)
    asst1 = [m for m in t1 if m["role"] == "assistant"][0]
    assert bytes(row["summarized_up_to_message_id"]) == bytes(asst1["id"])
    # no chat_turns row and no user quota for the system task
    assert len(db.turns(c["id"])) == 2
    assert db.quota_snapshot("tok-k2") == quota_after_turns
    # next turn: summary preamble message + stream_started.thread_summary_applied
    cm = "SUM-C-" + nonce()
    s3 = a.stream(c["id"], f"short follow-up {cm}")
    assert s3.done
    tsa = s3.started.get("thread_summary_applied")
    assert tsa and isinstance(tsa.get("token_estimate"), int)
    req = mock_llm.chat_requests(contains=cm)[-1]
    items = request_input_roles_texts(req)
    summary_items = [(r, t) for r, t in items if SUMMARY_PREAMBLE in t]
    assert len(summary_items) == 1 and summary_items[0][0] == "user"
    assert SUMMARY_MARKER in summary_items[0][1]
    body = json.dumps(req["json"])
    assert am not in body, "compressed messages are not re-sent"
    assert bm in body, "recent messages after the frontier follow the summary"
    assert [r for r, _ in items if r in ("user", "assistant")][-1] == "user"
    # the UI still sees the whole history
    assert len(a.messages(c["id"])) == 6


# Acceptance: Cleanup & Recovery — summary failure is retried; the previous state is kept meanwhile
@pytest.mark.slow
@pytest.mark.timeout(180)
def test_summary_failure_is_retried(api_for, db, mock_llm):
    a = api_for("tok-k4")
    am, bm = "SUMF-A-" + nonce(), "SUMF-B-" + nonce()
    mock_llm.configure(summary_fail={"match": am, "count": 1})
    c, s1, s2 = _two_big_turns(a, am, bm)
    wait_for(lambda: mock_llm.summary_requests(contains=am), timeout=60, interval=0.5, desc="first summary attempt")
    first = mock_llm.summary_requests(contains=am)[0]
    assert first.get("failed_on_purpose")
    row = wait_for(lambda: _summary_row(db, c["id"]), timeout=150, interval=1, desc="summary after retry")
    assert SUMMARY_MARKER in row["summary_text"]
    assert len(mock_llm.summary_requests(contains=am)) >= 2


# Acceptance: Cleanup & Recovery — mutation-driven invalidation of a covering summary
@pytest.mark.timeout(120)
def test_summary_invalidated_by_mutation(api_for, db):
    a = api_for("tok-k3")
    am, bm = "SUMI-A-" + nonce(), "SUMI-B-" + nonce()
    c, s1, s2 = _two_big_turns(a, am, bm)
    wait_for(lambda: _summary_row(db, c["id"]), timeout=60, interval=0.5, desc="thread_summaries row")
    # deleting turn 2: the summary frontier (turn 1) is before turn 2 -> kept
    assert a.delete(f"/v1/chats/{c['id']}/turns/{s2.request_id}").status_code == 204
    assert _summary_row(db, c["id"]) is not None
    # deleting turn 1 (now the latest, covered by the summary) -> summary dropped, is_compressed cleared
    assert a.delete(f"/v1/chats/{c['id']}/turns/{s1.request_id}").status_code == 204
    assert _summary_row(db, c["id"]) is None
    assert all(m["is_compressed"] in (0, False) for m in db.message_rows(c["id"]))
    # the next turn runs without a summary
    s3 = a.stream(c["id"], "fresh start " + nonce())
    assert s3.done
    assert not s3.started.get("thread_summary_applied")
