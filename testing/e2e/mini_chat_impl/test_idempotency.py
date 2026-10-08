"""Idempotency & replay (acceptance: Idempotency & Replay)."""

import time
import uuid

import pytest

from mchelpers import BackgroundStream, assert_problem, assert_sse_grammar, hex32, nonce


# Acceptance: Idempotency — replay returns the stored result without side effects
def test_replay_completed_turn_has_no_side_effects(api_for, mock_llm, db):
    a = api_for("tok-k1")
    c = a.create_chat()
    n = nonce()
    rid = str(uuid.uuid4())
    s1 = a.stream(c["id"], f"replay me {n}", request_id=rid)
    assert s1.done
    time.sleep(0.5)
    calls_before = len(mock_llm.chat_requests(contains=n))
    quota_before = db.quota_snapshot("tok-k1")
    outbox_before = db.outbox_count(hex32(rid))
    turns_before = len(db.turns(c["id"]))

    s2 = a.stream(c["id"], f"replay me {n}", request_id=rid)
    assert_sse_grammar(s2)
    assert s2.names(include_ping=False) == ["stream_started", "delta", "done"]
    st = s2.started
    assert st["is_new_turn"] is False
    assert st["request_id"] == rid
    assert st["message_id"] == s1.message_id
    assert s2.of("delta") == [{"type": "text", "content": "Hello world"}]
    d = s2.done
    assert d["usage"] == {"input_tokens": 10, "output_tokens": 5}
    assert d["selected_model"] == s1.done["selected_model"]
    assert d["effective_model"] == s1.done["effective_model"]
    assert d["quota_decision"] == "allow"
    assert d.get("downgrade_reason") is None
    assert not d.get("quota_warnings"), "quota_warnings are absent on replay"
    assert s2.of("citations") == []

    time.sleep(1.0)
    assert len(mock_llm.chat_requests(contains=n)) == calls_before, "replay must not call the provider"
    assert db.quota_snapshot("tok-k1") == quota_before, "replay must not touch quota_usage"
    assert db.outbox_count(hex32(rid)) <= outbox_before, "replay must not enqueue outbox messages"
    assert len(db.turns(c["id"])) == turns_before
    assert a.chat(c["id"])["message_count"] == 2


# Acceptance: Idempotency — the key is (chat_id, request_id); content of a replay is irrelevant
def test_replay_ignores_new_content(api, mock_llm):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    assert api.stream(c["id"], "original " + nonce(), request_id=rid).done
    n = nonce()
    s = api.stream(c["id"], f"different content {n}", request_id=rid)
    assert s.started["is_new_turn"] is False
    assert s.text_content == "Hello world"
    assert mock_llm.chat_requests(contains=n) == []


# Acceptance: Idempotency — the same request id in another chat is a new turn
def test_same_request_id_other_chat_is_new_turn(api):
    rid = str(uuid.uuid4())
    c1 = api.create_chat()
    c2 = api.create_chat()
    assert api.stream(c1["id"], "one " + nonce(), request_id=rid).done
    s = api.stream(c2["id"], "two " + nonce(), request_id=rid)
    assert s.started["is_new_turn"] is True and s.done


# Acceptance: Idempotency — conflicting reuse: failed turn
def test_request_id_conflict_failed(api, mock_llm):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    s = api.stream(c["id"], "fail [[fail]] " + nonce(), request_id=rid)
    assert s.error["code"] == "provider_error"
    n = nonce()
    s2 = api.stream(c["id"], f"again {n}", request_id=rid)
    assert not s2.is_sse
    assert_problem(api.as_response(s2), 409, "aborted", reason="request_id_conflict")
    assert mock_llm.chat_requests(contains=n) == []


# Acceptance: Idempotency — conflicting reuse: running turn (idempotency before the parallel guard)
@pytest.mark.timeout(60)
def test_request_id_conflict_running(api):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    bg = BackgroundStream.send(api, c["id"], "hang [[hang]] " + nonce(), request_id=rid).wait_started()
    try:
        s = api.stream(c["id"], "dup " + nonce(), request_id=rid)
        assert_problem(api.as_response(s), 409, "aborted", reason="request_id_conflict")
    finally:
        bg.stop()


# Acceptance: Idempotency — conflicting reuse: cancelled turn
@pytest.mark.timeout(60)
def test_request_id_conflict_cancelled(api):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    bg = BackgroundStream.send(api, c["id"], "slow [[slow]] " + nonce(), request_id=rid).wait_started().wait_delta()
    bg.stop()
    api.wait_turn_state(c["id"], rid, {"cancelled"}, timeout=30)
    s = api.stream(c["id"], "dup " + nonce(), request_id=rid)
    assert_problem(api.as_response(s), 409, "aborted", reason="request_id_conflict")


# Acceptance: Idempotency — conflicting reuse: soft-deleted turn
def test_request_id_conflict_deleted(api):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    assert api.stream(c["id"], "to delete " + nonce(), request_id=rid).done
    assert api.delete(f"/v1/chats/{c['id']}/turns/{rid}").status_code == 204
    s = api.stream(c["id"], "dup " + nonce(), request_id=rid)
    assert_problem(api.as_response(s), 409, "aborted", reason="request_id_conflict")


# Acceptance: Idempotency — replay is checked before the parallel-turn guard
@pytest.mark.timeout(60)
def test_replay_served_while_other_turn_runs(api):
    c = api.create_chat()
    rid1 = str(uuid.uuid4())
    s1 = api.stream(c["id"], "first " + nonce(), request_id=rid1)
    assert s1.done
    bg = BackgroundStream.send(api, c["id"], "hang [[hang]] " + nonce()).wait_started()
    try:
        s = api.stream(c["id"], "replay while running", request_id=rid1)
        assert s.is_sse, f"replay must be served, got {s.status_code} {s.text}"
        assert s.started["is_new_turn"] is False
        assert s.started["message_id"] == s1.message_id
        assert s.done
    finally:
        bg.stop()


# Acceptance: Settlement — usage published once per turn even when replayed twice
def test_one_usage_message_per_turn_after_replays(api, db):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    assert api.stream(c["id"], "once " + nonce(), request_id=rid).done
    first = db.outbox_count(hex32(rid), "billing_outcome")
    assert first <= 1
    for _ in range(2):
        assert api.stream(c["id"], "once", request_id=rid).started["is_new_turn"] is False
    assert db.outbox_count(hex32(rid), "billing_outcome") <= first
