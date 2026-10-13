"""Messages list, Turn Status API, reactions and turn mutations (retry / edit / delete).

DESIGN §3.3 List Messages, Turn Status API, Message Reaction API; §3.9 Turn Mutation Rules.
"""

import time
import uuid

import pytest

from conftest import (
    CHAT_RT,
    MESSAGE_RT,
    ODATA_RT,
    TURN_RT,
    assert_no_provider_ids,
    assert_problem,
    background_stream,
    sse_request,
)

MSG_REQUIRED = {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}


def messages(api, chat_id, **params):
    r = api.get(f"/chats/{chat_id}/messages", params=params)
    assert r.status_code == 200, r.text
    return r.json()


# ------------------------------------------------------------------ messages
def test_messages_list_contract(api, chat):
    t1 = api.turn(chat["id"], "one")
    t2 = api.turn(chat["id"], "two")
    page = messages(api, chat["id"])
    items = page["items"]
    assert page["page_info"]["limit"] == 20
    assert [m["role"] for m in items] == ["user", "assistant", "user", "assistant"]
    assert [m["content"] for m in items[::2]] == ["one", "two"]
    stamps = [m["created_at"] for m in items]
    assert stamps == sorted(stamps)
    for m in items:
        assert MSG_REQUIRED <= set(m), m
        assert m["attachments"] == []
        assert m["my_reaction"] is None
        assert_no_provider_ids(m)
    user, asst = items[0], items[1]
    assert user["request_id"] == asst["request_id"] == t1.started["request_id"]
    assert items[2]["request_id"] == t2.started["request_id"]
    assert "model" not in user and "input_tokens" not in user
    assert asst["model"] == "gpt-4.1"
    assert asst["input_tokens"] == 120 and asst["output_tokens"] == 30
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 4


def test_messages_odata(api, chat):
    for i in range(3):
        api.turn(chat["id"], f"m{i}")
    all_items = messages(api, chat["id"])["items"]
    desc = messages(api, chat["id"], **{"$orderby": "created_at desc"})["items"]
    assert [m["id"] for m in desc] == [m["id"] for m in reversed(all_items)]
    only_asst = messages(api, chat["id"], **{"$filter": "role eq 'assistant'"})["items"]
    assert len(only_asst) == 3 and {m["role"] for m in only_asst} == {"assistant"}
    later = messages(api, chat["id"], **{"$filter": f"created_at ge {all_items[2]['created_at']}"})["items"]
    assert [m["id"] for m in later] == [m["id"] for m in all_items[2:]]
    earlier = messages(api, chat["id"], **{"$filter": f"created_at lt {all_items[2]['created_at']}"})["items"]
    assert [m["id"] for m in earlier] == [m["id"] for m in all_items[:2]]
    r = api.get(f"/chats/{chat['id']}/messages", params={"$filter": f"updated_at gt {all_items[1]['created_at']}"})
    assert_problem(r, 400, "invalid_argument", resource_type=ODATA_RT)  # not a filterable field
    one = messages(api, chat["id"], **{"$filter": f"id eq {all_items[3]['id']}"})["items"]
    assert [m["id"] for m in one] == [all_items[3]["id"]]
    # Cursor pagination.
    seen, page = [], messages(api, chat["id"], limit=4)
    while True:
        seen += [m["id"] for m in page["items"]]
        cur = page["page_info"].get("next_cursor")
        if not cur:
            break
        page = messages(api, chat["id"], limit=4, cursor=cur)
    assert seen == [m["id"] for m in all_items]
    assert messages(api, chat["id"], limit=500)["page_info"]["limit"] == 100
    for params, reason in (({"limit": 0}, "INVALID_LIMIT"), ({"$filter": "content eq 'x'"}, None),
                           ({"$orderby": "content asc"}, "INVALID_ORDERBY_FIELD"),
                           ({"cursor": "@@@"}, "INVALID_CURSOR")):
        r = api.get(f"/chats/{chat['id']}/messages", params=params)
        assert_problem(r, 400, "invalid_argument", reason=reason, resource_type=ODATA_RT)


def test_messages_filter_timestamp_exact(api, chat):
    api.turn(chat["id"], "a")
    items = messages(api, chat["id"])["items"]
    ts = items[0]["created_at"]
    eq = messages(api, chat["id"], **{"$filter": f"created_at eq {ts}"})["items"]
    gt = messages(api, chat["id"], **{"$filter": f"created_at gt {ts}"})["items"]
    assert ([m["id"] for m in eq], [m["id"] for m in gt]) == ([items[0]["id"]], [items[1]["id"]])


def test_messages_of_unknown_chat(api):
    r = api.get(f"/chats/{uuid.uuid4()}/messages")
    assert_problem(r, 404, "not_found", resource_type=CHAT_RT)


# ------------------------------------------------------------------ turn status
def test_turn_status(api, chat):
    res = api.turn(chat["id"], "status")
    rid = res.started["request_id"]
    st = api.get(f"/chats/{chat['id']}/turns/{rid}")
    assert st.status_code == 200
    body = st.json()
    assert body["request_id"] == rid
    assert body["state"] == "done"
    assert body["assistant_message_id"] == res.started["message_id"]
    assert "updated_at" in body and "chat_id" not in body and "error_code" not in body
    assert_no_provider_ids(body)
    assert_problem(api.get(f"/chats/{chat['id']}/turns/{uuid.uuid4()}"), 404, "not_found", resource_type=TURN_RT)
    assert_problem(api.get(f"/chats/{chat['id']}/turns/xyz"), 400, "invalid_argument",
                   reason="invalid_path_params")


# ------------------------------------------------------------------ reactions
def test_reactions(api, chat):
    api.turn(chat["id"], "react")
    user_msg, asst_msg = messages(api, chat["id"])["items"]
    base = f"/chats/{chat['id']}/messages/{asst_msg['id']}/reaction"

    r = api.put(base, json={"reaction": "like"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["message_id"] == asst_msg["id"] and body["reaction"] == "like" and "created_at" in body
    assert messages(api, chat["id"])["items"][1]["my_reaction"] == "like"

    r = api.put(base, json={"reaction": "dislike"})  # upsert
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    items = messages(api, chat["id"])["items"]
    assert items[1]["my_reaction"] == "dislike" and items[0]["my_reaction"] is None

    r = api.put(base, json={"reaction": "love"})
    assert_problem(r, 400, "invalid_argument", field="reaction", reason="INVALID_REACTION")
    r = api.put(base, json={})
    assert_problem(r, 422, "invalid_argument", reason="invalid_json_body")

    assert api.delete(base).status_code == 204
    assert messages(api, chat["id"])["items"][1]["my_reaction"] is None
    assert api.delete(base).status_code == 204  # idempotent

    ubase = f"/chats/{chat['id']}/messages/{user_msg['id']}/reaction"
    viol = {"subject": "reaction_target", "type": "STATE"}
    assert_problem(api.put(ubase, json={"reaction": "like"}), 400, "failed_precondition", violation=viol)
    assert_problem(api.delete(ubase), 400, "failed_precondition", violation=viol)

    missing = f"/chats/{chat['id']}/messages/{uuid.uuid4()}/reaction"
    assert_problem(api.put(missing, json={"reaction": "like"}), 404, "not_found", resource_type=MESSAGE_RT)


def test_reaction_isolated_per_user(api, reviewer, chat):
    api.turn(chat["id"], "react2")
    asst = messages(api, chat["id"])["items"][1]
    base = f"/chats/{chat['id']}/messages/{asst['id']}/reaction"
    assert_problem(reviewer.put(base, json={"reaction": "like"}), 404, "not_found")
    assert messages(api, chat["id"])["items"][1]["my_reaction"] is None


# ------------------------------------------------------------------ turn mutations
def test_retry_last_turn(api, chat, fresh_mock, db):
    first = api.turn(chat["id"], "please retry")
    old_rid = first.started["request_id"]
    res = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{old_rid}/retry")
    assert res.status == 200, res.problem
    assert res.terminal_name == "done", res.events
    new_rid = res.started["request_id"]
    assert new_rid != old_rid and uuid.UUID(new_rid).version == 4
    assert res.started["is_new_turn"] is True
    assert res.started["message_id"] != first.started["message_id"]
    # The provider got the original user message again.
    body = fresh_mock.responses_calls()[-1]["body"]
    assert "please retry" in str(body["input"][-1])
    assert str(body["input"]).count("please retry") == 1
    items = messages(api, chat["id"])["items"]
    assert [m["request_id"] for m in items] == [new_rid, new_rid]
    assert items[0]["content"] == "please retry"
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2
    assert_problem(api.get(f"/chats/{chat['id']}/turns/{old_rid}"), 404, "not_found", resource_type=TURN_RT)
    assert api.get(f"/chats/{chat['id']}/turns/{new_rid}").json()["state"] == "done"
    old = db.turn(chat["id"], old_rid)
    assert old["deleted_at"] is not None and old["replaced_by_request_id"] == new_rid
    # Old request id: replay is gone, mutations say not latest.
    again = api.stream(chat["id"], "x", request_id=old_rid)
    assert_problem(again.problem, 409, "aborted", ctx_reason="request_id_conflict")
    r = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{old_rid}/retry")
    assert_problem(r.problem, 409, "aborted", ctx_reason="NOT_LATEST_TURN")
    # Audit event turn_retry with both request ids (DESIGN §3.9 Audit Events for Turn Mutations).
    from conftest import wait_until

    def retry_audit():
        return [b["json"] for b in db.outbox_bodies("mini_chat.audit_event.v1")
                if "turn_retry" in (b["json"].get("event_type"), b["json"].get("event_kind"))
                and b["json"].get("chat_id") == chat["id"]]

    ev = wait_until(retry_audit, timeout=10, desc="turn_retry audit event")[0]
    assert ev["original_request_id"] == old_rid and ev["new_request_id"] == new_rid
    assert ev["actor_user_id"] and ev["timestamp"]


def test_mutations_immediately_after_completion(api):
    """Regression: retry / edit / delete / upload right after `done` raced the outbox workers on SQLite
    and failed with 500 `database is locked` (SQLITE_BUSY_SNAPSHOT)."""
    chat = api.create_chat()
    statuses = []
    for _ in range(4):
        t = api.turn(chat["id"], "x")
        statuses.append(("delete", api.delete(f"/chats/{chat['id']}/turns/{t.started['request_id']}").status_code))
        t = api.turn(chat["id"], "x")
        statuses.append(("retry", sse_request(api, "POST",
                                              f"/chats/{chat['id']}/turns/{t.started['request_id']}/retry").status))
        t = api.turn(chat["id"], "x")
        statuses.append(("edit", sse_request(api, "PATCH", f"/chats/{chat['id']}/turns/{t.started['request_id']}",
                                             {"content": "e"}).status))
        api.turn(chat["id"], "x")
        statuses.append(("upload", api.upload(chat["id"], "a.txt", b"hi", "text/plain").status_code))
    expected = {"delete": 204, "retry": 200, "edit": 200, "upload": 201}
    bad = [(op, st) for op, st in statuses if st != expected[op]]
    assert bad == [], bad


def test_retry_of_failed_turn(api, chat):
    failed = api.stream(chat["id"], "fails [[error]]")
    assert failed.terminal_name == "error"
    rid = failed.started["request_id"]
    res = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{rid}/retry")
    assert res.status == 200, res.problem
    # Same content -> same mock failure, but the mutation went through the full pipeline.
    assert res.terminal_name == "error" and res.terminal["code"] == "provider_error"
    assert res.started["request_id"] != rid


def test_edit_last_turn(api, chat, fresh_mock):
    first = api.turn(chat["id"], "original text")
    old_rid = first.started["request_id"]
    res = sse_request(api, "PATCH", f"/chats/{chat['id']}/turns/{old_rid}", {"content": "edited text"})
    assert res.status == 200, res.problem
    assert res.terminal_name == "done"
    new_rid = res.started["request_id"]
    assert new_rid != old_rid
    body = fresh_mock.responses_calls()[-1]["body"]
    assert "edited text" in str(body["input"]) and "original text" not in str(body["input"])
    items = messages(api, chat["id"])["items"]
    assert [m["content"] for m in items] == ["edited text", "Hello from mock."]
    assert {m["request_id"] for m in items} == {new_rid}

    r = sse_request(api, "PATCH", f"/chats/{chat['id']}/turns/{new_rid}", {"content": "  "})
    assert_problem(r.problem, 400, "invalid_argument", field="content", reason="EMPTY_CONTENT")
    r = sse_request(api, "PATCH", f"/chats/{chat['id']}/turns/{old_rid}", {"content": "again"})
    assert_problem(r.problem, 409, "aborted", ctx_reason="NOT_LATEST_TURN")


def test_delete_last_turn(api, chat):
    t1 = api.turn(chat["id"], "keep me")
    t2 = api.turn(chat["id"], "delete me")
    rid1, rid2 = t1.started["request_id"], t2.started["request_id"]
    # Not latest -> 409.
    assert_problem(api.delete(f"/chats/{chat['id']}/turns/{rid1}"), 409, "aborted", ctx_reason="NOT_LATEST_TURN")
    r = api.delete(f"/chats/{chat['id']}/turns/{rid2}")
    assert r.status_code == 204 and r.content == b""
    items = messages(api, chat["id"])["items"]
    assert [m["content"] for m in items] == ["keep me", "Hello from mock."]
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 2
    assert_problem(api.get(f"/chats/{chat['id']}/turns/{rid2}"), 404, "not_found", resource_type=TURN_RT)
    # Deleted turn: second delete is not latest; request id cannot be reused.
    assert_problem(api.delete(f"/chats/{chat['id']}/turns/{rid2}"), 409, "aborted", ctx_reason="NOT_LATEST_TURN")
    again = api.stream(chat["id"], "x", request_id=rid2)
    assert_problem(again.problem, 409, "aborted", ctx_reason="request_id_conflict")
    # The previous turn is latest again.
    r = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{rid1}/retry")
    assert r.status == 200 and r.terminal_name == "done"


def test_mutations_of_running_turn(api, chat):
    prev = api.turn(chat["id"], "before")
    bg = background_stream(api, chat["id"], "running [[slow]]").wait_started()
    try:
        # Wait until the running turn is visible.
        rid = None
        for _ in range(40):
            items = api.get(f"/chats/{chat['id']}/messages").json()["items"]
            if len(items) >= 3:
                rid = items[-1]["request_id"]
                break
            time.sleep(0.1)
        assert rid, "running turn's user message not visible"
        viol = {"subject": "turn_state", "type": "STATE"}
        r = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{rid}/retry")
        assert_problem(r.problem, 400, "failed_precondition", violation=viol)
        r = sse_request(api, "PATCH", f"/chats/{chat['id']}/turns/{rid}", {"content": "x"})
        assert_problem(r.problem, 400, "failed_precondition", violation=viol)
        assert_problem(api.delete(f"/chats/{chat['id']}/turns/{rid}"), 400, "failed_precondition", violation=viol)
        # The previous (completed) turn is no longer the latest.
        r = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{prev.started['request_id']}/retry")
        assert_problem(r.problem, 409, "aborted", ctx_reason="NOT_LATEST_TURN")
    finally:
        result = bg.finish()
    assert result.terminal_name == "done"


def test_mutation_unknown_turn(api, chat):
    api.turn(chat["id"], "x")
    rid = str(uuid.uuid4())
    r = sse_request(api, "POST", f"/chats/{chat['id']}/turns/{rid}/retry")
    assert r.problem["status"] in (404, 409), r.problem
    if r.problem["status"] == 404:
        assert_problem(r.problem, 404, "not_found")
    else:
        assert_problem(r.problem, 409, "aborted", ctx_reason="NOT_LATEST_TURN")
