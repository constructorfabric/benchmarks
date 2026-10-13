"""US1 black-box: chat CRUD, isolation, messages API and the SSE stream contract."""

import uuid

import pytest

from conftest import TENANT_A, USER_A, field_reasons, ub, us, wait_until

MINI_CHAT_PATCH = {"streaming": {"sse_ping_interval_seconds": 5}}

PROVIDER_ID_MARKERS = ("resp_", "file-", "vs_", "msg_1", "sk-")


# ------------------------------------------------------------------ chat CRUD
def test_create_chat_defaults_and_location(api):
    r = api.post("/chats", json={})
    assert r.status_code == 201, r.text
    body = r.json()
    assert body["model"] == "gpt-4.1"  # catalog default (is_default)
    assert "title" not in body  # omitted when null
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    assert r.headers["location"].endswith(f"/chats/{body['id']}")
    assert api.get(f"/chats/{body['id']}").json() == body


def test_create_chat_with_model_and_title(api, db):
    body = api.create_chat(title="  My chat  ", model="gpt-4.1-mini")
    assert body["model"] == "gpt-4.1-mini"
    assert body["title"].strip() == "My chat"
    row = db.one("SELECT * FROM chats WHERE id = ?", ub(body["id"]))
    assert us(row["tenant_id"]) == TENANT_A
    assert us(row["user_id"]) == USER_A
    assert row["model"] == "gpt-4.1-mini"


def test_create_chat_invalid_model(api):
    r = api.post("/chats", json={"model": "no-such-model"})
    assert r.status_code == 400
    assert r.headers["content-type"].startswith("application/problem+json")
    assert field_reasons(r.json()) == ["INVALID_MODEL"]


def test_create_chat_title_too_long(api):
    r = api.post("/chats", json={"title": "x" * 300})
    assert r.status_code == 400
    assert field_reasons(r.json()) == ["INVALID_TITLE"]


def test_update_and_delete_chat(api, db):
    c = api.create_chat(title="old")
    r = api.patch(f"/chats/{c['id']}", json={"title": "new title"})
    assert r.status_code == 200, r.text
    assert r.json()["title"] == "new title"
    assert r.json()["model"] == c["model"]
    assert api.get(f"/chats/{c['id']}").json()["title"] == "new title"

    r = api.delete(f"/chats/{c['id']}")
    assert r.status_code == 204
    assert api.get(f"/chats/{c['id']}").status_code == 404
    assert api.delete(f"/chats/{c['id']}").status_code == 404
    assert db.one("SELECT deleted_at FROM chats WHERE id = ?", ub(c["id"]))["deleted_at"] is not None


def test_chat_model_is_immutable(api):
    c = api.create_chat(model="gpt-4.1-mini")
    r = api.patch(f"/chats/{c['id']}", json={"title": "t", "model": "gpt-4.1"})
    # unknown field rejected or ignored, but the model never changes
    assert r.status_code in (200, 400, 422)
    assert api.get(f"/chats/{c['id']}").json()["model"] == "gpt-4.1-mini"


def test_list_chats_pagination_and_ordering(api_a2):
    # api_a2 is a dedicated user so the list only contains these chats
    ids = [api_a2.create_chat(title=f"c{i}")["id"] for i in range(3)]
    r = api_a2.get("/chats", params={"limit": 2})
    assert r.status_code == 200, r.text
    page1 = r.json()
    assert [c["id"] for c in page1["items"]] == [ids[2], ids[1]]  # most recent activity first
    cursor = page1["page_info"]["next_cursor"]
    assert cursor
    page2 = api_a2.get("/chats", params={"limit": 2, "cursor": cursor}).json()
    assert [c["id"] for c in page2["items"]] == [ids[0]]
    assert page2["page_info"]["next_cursor"] is None

    # activity moves a chat to the top
    api_a2.patch(f"/chats/{ids[0]}", json={"title": "bumped"})
    top = api_a2.get("/chats", params={"limit": 1}).json()["items"][0]
    assert top["id"] == ids[0]

    asc = api_a2.get("/chats", params={"$orderby": "title asc"}).json()["items"]
    titles = [c.get("title") for c in asc]
    assert titles == sorted(titles)

    flt = api_a2.get("/chats", params={"$filter": "title eq 'c1'"}).json()["items"]
    assert [c["id"] for c in flt] == [ids[1]]


def test_list_chats_rejects_malformed_query(api):
    assert api.get("/chats", params={"$filter": "title eq"}).status_code == 400
    assert api.get("/chats", params={"$orderby": "nonexistent desc"}).status_code == 400
    assert api.get("/chats", params={"cursor": "garbage!!"}).status_code == 400


# ------------------------------------------------------------------ isolation
def test_tenant_and_owner_isolation(api, api_a2, api_b, anon):
    c = api.create_chat(title="private")
    for other in (api_a2, api_b):
        r = other.get(f"/chats/{c['id']}")
        assert r.status_code == 404, r.text
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"
        assert other.patch(f"/chats/{c['id']}", json={"title": "x"}).status_code == 404
        assert other.delete(f"/chats/{c['id']}").status_code == 404
        assert other.get(f"/chats/{c['id']}/messages").status_code == 404
        assert other.send(c["id"], "hi").status == 404
        assert c["id"] not in [x["id"] for x in other.get("/chats").json()["items"]]
    assert anon.get(f"/chats/{c['id']}").status_code == 401
    assert api.get(f"/chats/{c['id']}").status_code == 200


# ------------------------------------------------------------------ streaming
def test_stream_event_contract_and_persistence(api, mock, db):
    c = api.create_chat()
    mock.push({"text": "Hello there, streaming world", "usage": {"input_tokens": 42, "output_tokens": 7}})
    res = api.send(c["id"], "hi there")
    assert res.status == 200
    assert res.is_sse
    assert res.headers["content-type"].startswith("text/event-stream")
    names = res.names
    assert names[0] == "stream_started"
    assert names[-1] == "done"
    assert set(names[1:-1]) <= {"delta", "ping"}
    assert "delta" in names
    started = res.started
    assert started["is_new_turn"] is True
    uuid.UUID(started["request_id"])
    uuid.UUID(started["message_id"])
    assert res.text_content == "Hello there, streaming world"

    done = res.done
    assert done["usage"] == {"input_tokens": 42, "output_tokens": 7}
    assert done["effective_model"] == "gpt-4.1"
    assert done["selected_model"] == "gpt-4.1"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    for w in done.get("quota_warnings", []):
        assert set(w) >= {"tier", "period", "remaining_percentage", "warning", "exhausted"}
    for marker in PROVIDER_ID_MARKERS:
        assert marker not in res.text, f"provider identifier leaked: {marker}"

    # persisted messages share the request id
    msgs = api.messages(c["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    user, asst = msgs
    assert user["content"] == "hi there"
    assert asst["content"] == "Hello there, streaming world"
    assert user["request_id"] == asst["request_id"] == started["request_id"]
    assert asst["id"] == started["message_id"]
    assert asst["model"] == "gpt-4.1"
    assert asst["input_tokens"] == 42 and asst["output_tokens"] == 7
    for m in msgs:
        assert m["attachments"] == []
        assert "my_reaction" in m and m["my_reaction"] is None
    assert "model" not in user and "input_tokens" not in user

    # DB: completed turn row pointing at the assistant message
    turn = db.turn(c["id"], started["request_id"])
    assert turn["state"] == "completed"
    assert us(turn["assistant_message_id"]) == started["message_id"]
    assert turn["effective_model"] == "gpt-4.1"
    assert turn["completed_at"] is not None
    assert turn["provider_response_id"].startswith("resp_")  # kept internally only
    row = db.one("SELECT * FROM messages WHERE id = ?", ub(started["message_id"]))
    assert row["role"] == "assistant" and row["input_tokens"] == 42 and row["output_tokens"] == 7

    chat = api.get(f"/chats/{c['id']}").json()
    assert chat["message_count"] == 2


def test_provider_request_shape(api, mock):
    c = api.create_chat()
    rid = str(uuid.uuid4())
    res = api.send(c["id"], "what is up", request_id=rid)
    assert res.status == 200 and res.request_id == rid
    reqs = mock.chat_requests()
    assert len(reqs) == 1
    req = reqs[0]
    assert req["path"] == "/v1/responses"
    body = req["json"]
    assert body["model"] == "gpt-4.1"
    assert body["stream"] is True
    assert body["store"] is False
    assert body["max_output_tokens"] == 32768
    assert body["instructions"].startswith("You are a helpful, concise assistant.")
    assert body["input"] == [{"role": "user", "content": "what is up"}]
    assert body["metadata"] == {
        "tenant_id": TENANT_A,
        "user_id": USER_A,
        "chat_id": c["id"],
        "request_type": "chat",
        "feature": "none",
    }
    assert body["user"] == TENANT_A.replace("-", "") + USER_A.replace("-", "")
    assert body["temperature"] == 0.7
    assert "tools" not in body  # no documents, no web search


def test_history_is_sent_in_order(api, mock):
    c = api.create_chat()
    mock.push({"text": "answer one"})
    api.send(c["id"], "question one")
    mock.push({"text": "answer two"})
    api.send(c["id"], "question two")
    body = mock.chat_requests()[-1]["json"]
    assert body["input"] == [
        {"role": "user", "content": "question one"},
        {"role": "assistant", "content": "answer one"},
        {"role": "user", "content": "question two"},
    ]
    msgs = api.messages(c["id"])
    assert [m["content"] for m in msgs] == ["question one", "answer one", "question two", "answer two"]
    # messages API ordering / filtering / pagination
    desc = api.messages(c["id"], **{"$orderby": "created_at desc"})
    assert [m["content"] for m in desc] == ["answer two", "question two", "answer one", "question one"]
    users = api.messages(c["id"], **{"$filter": "role eq 'user'"})
    assert [m["content"] for m in users] == ["question one", "question two"]
    r = api.get(f"/chats/{c['id']}/messages", params={"limit": 3})
    page = r.json()
    assert len(page["items"]) == 3 and page["page_info"]["next_cursor"]
    rest = api.get(f"/chats/{c['id']}/messages", params={"limit": 3, "cursor": page["page_info"]["next_cursor"]}).json()
    assert [m["content"] for m in rest["items"]] == ["answer two"]


def test_preflight_validation_never_calls_provider(api, mock, db):
    c = api.create_chat()
    r = api.send(c["id"], "")
    assert r.status == 400 and field_reasons(r.problem) == ["EMPTY_CONTENT"]
    r = api.send(c["id"], "   ")
    assert r.status == 400
    r = api.send(c["id"], "hi", attachment_ids=[str(uuid.uuid4())])
    assert r.status in (400, 404), r
    r = api.send(str(uuid.uuid4()), "hi")
    assert r.status == 404
    assert mock.chat_requests() == []
    assert db.turns(c["id"]) == []
    assert db.messages(c["id"]) == []


def test_reasoning_and_incomplete_response(api, mock, db):
    c = api.create_chat()
    mock.push({"text": "cut off answ", "end": "incomplete", "usage": {"input_tokens": 5, "output_tokens": 3}})
    res = api.send(c["id"], "long one please")
    assert res.names[-1] == "done"
    assert db.turn(c["id"], res.request_id)["state"] == "completed"
    assert api.messages(c["id"])[-1]["content"] == "cut off answ"


@pytest.mark.parametrize(
    "spec,code",
    [
        ({"end": "failed", "message": "upstream failed"}, "provider_error"),
        ({"end": "error", "message": "bad thing"}, "provider_error"),
        ({"status": 500, "message": "internal boom"}, "provider_error"),
        ({"status": 429, "message": "slow down", "retry_after": 1}, "rate_limited"),
    ],
)
def test_provider_errors_become_terminal_error_event(api, mock, db, spec, code):
    c = api.create_chat()
    mock.push(spec)
    res = api.send(c["id"], "hi")
    assert res.status == 200, res
    assert res.names[0] == "stream_started"
    assert res.names[-1] == "error"
    assert res.names.count("error") == 1 and "done" not in res.names
    assert res.error["code"] == code
    assert isinstance(res.error["message"], str)
    turn = db.turn(c["id"], res.request_id)
    assert turn["state"] == "failed"
    assert turn["error_code"] == code
    status = api.turn_status(c["id"], res.request_id).json()
    assert status["state"] == "error" and status["error_code"] == code
    assert "assistant_message_id" not in status
    # the chat is usable again afterwards
    assert api.send(c["id"], "again").names[-1] == "done"


def test_provider_error_message_is_sanitized(api, mock):
    c = api.create_chat()
    leak = (
        "failure for resp_abcdef1234567890 file-abcdefabcdef1234 vs_0123456789abcdef "
        "see https://internal.example.com/x?y=1 key sk-ABCDEFGHIJKLMNOPQRST"
    )
    mock.push({"end": "failed", "message": leak})
    res = api.send(c["id"], "hi")
    msg = res.error["message"]
    for secret in ("resp_abcdef", "file-abcdef", "vs_0123", "internal.example.com", "sk-ABCDEF"):
        assert secret not in msg, msg
    assert "[provider_id]" in msg and "[url]" in msg and "[credential]" in msg
    assert "failure for" in msg


def test_ping_events_before_first_delta(api, mock):
    c = api.create_chat()
    mock.push({"text": "late", "delay_before": 5.6})
    res = api.send(c["id"], "slow please")
    names = res.names
    assert names[0] == "stream_started"
    assert "ping" in names
    first_delta = names.index("delta")
    assert all(n != "ping" for n in names[first_delta:])
    assert max(i for i, n in enumerate(names) if n == "ping") < first_delta
    assert names[-1] == "done"


def test_tool_and_citation_events(api, mock):
    """web_search tool events and url citations are relayed (DESIGN SSE contract)."""
    c = api.create_chat()  # gpt-4.1 supports web_search
    ann = {"type": "url_citation", "url": "https://example.org/page", "title": "Example", "start_index": 0, "end_index": 5}
    mock.push({
        "text": "Paris is the capital.",
        "pre_events": [
            ["response.web_search_call.searching", {"type": "response.web_search_call.searching", "item_id": "ws_1"}],
            ["response.web_search_call.completed", {"type": "response.web_search_call.completed", "item_id": "ws_1"}],
        ],
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "Paris is the capital.", "annotations": [ann]}]}],
    })
    res = api.send(c["id"], "capital of France?", web_search={"enabled": True})
    assert res.status == 200, res
    body = mock.chat_requests()[0]["json"]
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"]
    names = res.names
    tools = res.all("tool")
    assert [t["phase"] for t in tools] == ["start", "done"]
    assert all(t["name"] == "web_search" for t in tools)
    cits = res.first("citations")
    assert cits is not None
    item = cits["items"][0]
    assert item["source"] == "web" and item["url"] == "https://example.org/page" and item["title"] == "Example"
    # ordering: tool before citations before done
    assert names.index("tool") < names.index("citations") < names.index("done")


def test_reactions(api, api_a2):
    c = api.create_chat()
    res = api.send(c["id"], "hi")
    user_msg, asst_msg = api.messages(c["id"])
    r = api.put(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 200, r.text
    assert r.json()["reaction"] == "like" and r.json()["message_id"] == asst_msg["id"]
    # idempotent / switch
    assert api.put(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction", json={"reaction": "dislike"}).status_code == 200
    assert api.messages(c["id"])[1]["my_reaction"] == "dislike"
    r = api.put(f"/chats/{c['id']}/messages/{user_msg['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 400
    r = api.put(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction", json={"reaction": "love"})
    assert r.status_code == 400 and field_reasons(r.json()) == ["INVALID_REACTION"]
    assert api_a2.put(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction", json={"reaction": "like"}).status_code == 404
    assert api.delete(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction").status_code == 204
    assert api.delete(f"/chats/{c['id']}/messages/{asst_msg['id']}/reaction").status_code == 204
    assert api.messages(c["id"])[1]["my_reaction"] is None
    assert res.request_id


def test_models_api(api):
    r = api.get("/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert ids == ["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-mini-tiny-ctx"]
    for m in items:
        assert set(m) <= {"model_id", "display_name", "tier", "multiplier_display", "description", "multimodal_capabilities", "context_window"}
    assert items[0]["tier"] == "premium" and items[1]["tier"] == "standard"
    one = api.get("/models/gpt-4.1-mini").json()
    assert one["model_id"] == "gpt-4.1-mini" and one["context_window"] == 1047576
    r = api.get("/models/nope")
    assert r.status_code == 404
    assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


def test_chat_delete_cleans_provider_resources(api, mock, db):
    c = api.create_chat()
    r = api.upload(c["id"], "doc.txt", b"some document text " * 10, "text/plain")
    assert r.status_code == 201, r.text
    assert len(mock.files) == 1 and len(mock.vector_stores) == 1
    fid, vs = mock.files[0], mock.vector_stores[0]
    assert api.delete(f"/chats/{c['id']}").status_code == 204
    wait_until(lambda: mock.calls("DELETE", f"/vector_stores/{vs}"), timeout=20, desc="vector store delete")
    wait_until(lambda: mock.calls("DELETE", f"/files/{fid}"), timeout=20, desc="file delete")
    assert api.get(f"/chats/{c['id']}").status_code == 404
