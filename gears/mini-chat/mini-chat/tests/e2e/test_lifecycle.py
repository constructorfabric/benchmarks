"""Turn lifecycle recovery: orphan watchdog and retry/edit setup failures."""

import uuid

from harness import Client, parse_sse, ubytes, wait_for


def test_orphan_watchdog_finalizes_stale_turn(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"hang": True}])
    with client.c.stream("POST", f"/v1/chats/{c['id']}/messages:stream",
                         json={"content": "stuck"}, headers=client.h) as resp:
        it = resp.iter_text()
        rid = parse_sse(next(it))[0][1]["request_id"]
        stack.execute(
            "update chat_turns set last_progress_at = '2020-01-01T00:00:00.000000001Z' where request_id = ?",
            (ubytes(rid),),
        )
        t = wait_for(
            lambda: (x := stack.query("select * from chat_turns where request_id = ?", (ubytes(rid),))[0])["state"] != "running" and x,
            timeout=20,
        )
        assert t["state"] == "failed" and t["error_code"] == "orphan_timeout"
        assert t["completed_at"] is not None
        u = wait_for(lambda: [e for e in logs("usage") if e["request_id"] == rid])
        assert u[0]["billing_outcome"] == "aborted" and u[0]["settlement_method"] == "estimated"
        assert u[0]["terminal_state"] == "failed" and u[0]["usage"] is None
        assert u[0]["actual_credits_micro"] > 0
        st = client.req("GET", f"/v1/chats/{c['id']}/turns/{rid}").json()
        assert st["state"] == "error" and st["error_code"] == "orphan_timeout"
    # the chat is no longer blocked
    ev = client.send(c["id"], "next")
    assert ev[-1][0] == "done"


def test_watchdog_skips_fresh_turns(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"hang": True}])
    with client.c.stream("POST", f"/v1/chats/{c['id']}/messages:stream",
                         json={"content": "fresh"}, headers=client.h) as resp:
        it = resp.iter_text()
        rid = parse_sse(next(it))[0][1]["request_id"]
        # started long ago but with recent progress: not an orphan
        stack.execute(
            "update chat_turns set started_at = '2020-01-01T00:00:00.000000001Z' where request_id = ?",
            (ubytes(rid),),
        )
        import time

        time.sleep(3)
        t = stack.query("select state from chat_turns where request_id = ?", (ubytes(rid),))[0]
        assert t["state"] == "running"


def test_edit_setup_failure_marks_new_turn_failed(stack, client, mock):
    c = client.create_chat(model="gpt-tiny")
    rid = client.send(c["id"], "short")[0][1]["request_id"]
    r = client.req("PATCH", f"/v1/chats/{c['id']}/turns/{rid}", json={"content": "x" * 5000})
    assert r.status_code == 400
    assert r.json()["context"]["field_violations"][0]["reason"] == "CONTEXT_BUDGET_EXCEEDED"
    rows = stack.query("select * from chat_turns where chat_id = ? order by started_at", (ubytes(c["id"]),))
    assert len(rows) == 2
    old, new = rows
    assert old["deleted_at"] is not None
    assert new["state"] == "failed" and new["error_code"] == "context_length_exceeded"
    assert new["reserve_tokens"] is None
    st = client.req("GET", f"/v1/chats/{c['id']}/turns/{uuid.UUID(bytes=new['request_id'])}").json()
    assert st["state"] == "error" and st["error_code"] == "context_length_exceeded"
    # the chat accepts new turns
    assert client.send(c["id"], "ok")[-1][0] == "done"


def test_input_too_long_on_edit_is_a_preflight_rejection(stack, client, mock):
    c = client.create_chat(model="gpt-tiny")
    rid = client.send(c["id"], "short")[0][1]["request_id"]
    r = client.req("PATCH", f"/v1/chats/{c['id']}/turns/{rid}", json={"content": "x" * 9000})
    assert r.status_code == 400
    assert r.json()["context"]["field_violations"][0]["reason"] == "INPUT_TOO_LONG"
    rows = stack.query("select * from chat_turns where chat_id = ?", (ubytes(c["id"]),))
    assert len(rows) == 1 and rows[0]["deleted_at"] is None
