"""Parallel turn enforcement (acceptance: Parallel Turn Enforcement)."""

import threading
import uuid

import pytest

from mchelpers import Api, BackgroundStream, assert_problem, nonce


# Acceptance: Parallel turns — only one turn may run per chat at a time
@pytest.mark.timeout(60)
def test_second_turn_rejected_while_running(api, mock_llm):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "hang [[hang]] " + nonce()).wait_started()
    try:
        n = nonce()
        s = api.stream(c["id"], f"parallel {n}", request_id=str(uuid.uuid4()))
        assert not s.is_sse
        assert_problem(api.as_response(s), 409, "aborted", reason="turn_already_running")
        s = api.stream(c["id"], f"parallel no id {n}")
        assert_problem(api.as_response(s), 409, "aborted", reason="turn_already_running")
        assert mock_llm.chat_requests(contains=n) == []
    finally:
        bg.stop()


# Acceptance: Parallel turns — a different chat is not blocked
@pytest.mark.timeout(60)
def test_other_chat_not_blocked(api):
    c1 = api.create_chat()
    c2 = api.create_chat()
    bg = BackgroundStream.send(api, c1["id"], "hang [[hang]] " + nonce()).wait_started()
    try:
        assert api.stream(c2["id"], "independent " + nonce()).done
    finally:
        bg.stop()


# Acceptance: Parallel turns — a new turn is accepted once the previous one is terminal
@pytest.mark.timeout(60)
def test_new_turn_after_terminal(api):
    c = api.create_chat()
    bg = BackgroundStream.send(api, c["id"], "slow [[slow]] " + nonce()).wait_started().wait_delta()
    rid = bg.request_id
    bg.stop()
    api.wait_turn_state(c["id"], rid, {"cancelled", "error", "done"}, timeout=30)
    s = api.stream(c["id"], "after cancel " + nonce())
    assert s.done
    # after a failed turn as well
    s = api.stream(c["id"], "fail [[fail]] " + nonce())
    assert s.error
    assert api.stream(c["id"], "after failure " + nonce()).done


# Acceptance: Parallel turns — racing sends: exactly one stream, the other 409 turn_already_running
@pytest.mark.timeout(90)
def test_racing_sends(server):
    a1 = Api(server.base_url, "tok-a")
    a2 = Api(server.base_url, "tok-a")
    try:
        c = a1.create_chat()
        barrier = threading.Barrier(2)
        results = [None, None]

        def run(i, client):
            barrier.wait()
            results[i] = client.stream(
                c["id"], f"race {i} [[slow:2:1]] " + nonce(), request_id=str(uuid.uuid4()), timeout=60
            )

        ts = [threading.Thread(target=run, args=(i, cl)) for i, cl in enumerate((a1, a2))]
        for t in ts:
            t.start()
        for t in ts:
            t.join(60)
        sse = [r for r in results if r is not None and r.is_sse]
        rejected = [r for r in results if r is not None and not r.is_sse]
        assert len(sse) == 1 and len(rejected) == 1, [(r.status_code, r.text[:200]) for r in results if r]
        assert sse[0].done
        assert_problem(a1.as_response(rejected[0]), 409, "aborted", reason="turn_already_running")
    finally:
        a1.close()
        a2.close()
