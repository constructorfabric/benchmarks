"""Models API, reactions, message listing contract and error contract."""

import uuid

from harness import Client, parse_sse, ubytes, wait_for


def test_models_list_and_get(client):
    r = client.req("GET", "/v1/models")
    assert r.status_code == 200
    items = r.json()["items"]
    ids = [m["model_id"] for m in items]
    assert "gpt-disabled" not in ids and "gpt-premium" in ids
    prem = [m for m in items if m["model_id"] == "gpt-premium"][0]
    assert prem == {
        "model_id": "gpt-premium",
        "display_name": "GPT-PREMIUM",
        "tier": "premium",
        "multiplier_display": "1x",
        "description": "gpt-premium description",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128000,
    }
    for m in items:
        for forbidden in ("provider", "provider_id", "provider_model_id", "is_default",
                          "input_tokens_credit_multiplier_micro", "max_output_tokens", "policy_version"):
            assert forbidden not in m
    r = client.req("GET", "/v1/models/gpt-standard")
    assert r.status_code == 200 and r.json()["tier"] == "standard"
    for missing in ("gpt-disabled", "nope"):
        r = client.req("GET", f"/v1/models/{missing}")
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


def test_unauthenticated(stack):
    import httpx

    r = httpx.get(stack.base + "/mini-chat/v1/chats")
    assert r.status_code == 401
    r = httpx.get(stack.base + "/mini-chat/v1/chats", headers={"Authorization": "Bearer unknown"})
    assert r.status_code == 401


def test_reactions(stack, client, client_a2, mock):
    c = client.create_chat(model="gpt-standard")
    ev = client.send(c["id"], "hello")
    msgs = client.messages(c["id"])["items"]
    user_msg, asst = msgs[0], msgs[1]
    assert asst["my_reaction"] is None and "my_reaction" in user_msg
    r = client.req("PUT", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction", json={"reaction": "like"})
    assert r.status_code == 200
    body = r.json()
    assert body["message_id"] == asst["id"] and body["reaction"] == "like" and body["created_at"].endswith("Z")
    assert client.messages(c["id"])["items"][1]["my_reaction"] == "like"
    # upsert
    r = client.req("PUT", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction", json={"reaction": "dislike"})
    assert r.status_code == 200 and r.json()["reaction"] == "dislike"
    rows = stack.query("select * from message_reactions where message_id = ?", (ubytes(asst["id"]),))
    assert len(rows) == 1 and rows[0]["reaction"] == "dislike"
    assert client.messages(c["id"])["items"][1]["my_reaction"] == "dislike"
    # validation and targets
    r = client.req("PUT", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction", json={"reaction": "love"})
    assert r.status_code == 400 and r.json()["context"]["field_violations"][0]["reason"] == "INVALID_REACTION"
    r = client.req("PUT", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction", json={})
    assert r.status_code == 422
    for method in ("PUT", "DELETE"):
        r = client.req(method, f"/v1/chats/{c['id']}/messages/{user_msg['id']}/reaction",
                       json={"reaction": "like"} if method == "PUT" else None)
        assert r.status_code == 400
        v = r.json()["context"]["violations"][0]
        assert v["subject"] == "reaction_target" and v["type"] == "STATE"
        r = client.req(method, f"/v1/chats/{c['id']}/messages/{uuid.uuid4()}/reaction",
                       json={"reaction": "like"} if method == "PUT" else None)
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.message.v1~"
        r = client_a2.req(method, f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction",
                          json={"reaction": "like"} if method == "PUT" else None)
        assert r.status_code == 404
        assert r.json()["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"
    # delete is idempotent
    assert client.req("DELETE", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction").status_code == 204
    assert client.req("DELETE", f"/v1/chats/{c['id']}/messages/{asst['id']}/reaction").status_code == 204
    assert client.messages(c["id"])["items"][1]["my_reaction"] is None


def test_messages_list_query(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    for i in range(3):
        client.send(c["id"], f"q{i}")
    page = client.messages(c["id"], limit=2)
    assert len(page["items"]) == 2 and page["page_info"]["limit"] == 2
    allm = page["items"]
    cursor = page["page_info"]["next_cursor"]
    while cursor:
        p = client.messages(c["id"], limit=2, cursor=cursor)
        allm += p["items"]
        cursor = p["page_info"]["next_cursor"]
    assert len(allm) == 6
    assert [m["role"] for m in allm] == ["user", "assistant"] * 3
    assert [m["content"] for m in allm if m["role"] == "user"] == ["q0", "q1", "q2"]
    stamps = [m["created_at"] for m in allm]
    assert stamps == sorted(stamps)
    for m in allm:
        assert set(m) >= {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}
        assert m["attachments"] == []
    r = client.messages(c["id"], **{"$filter": "role eq 'assistant'"})
    assert {m["role"] for m in r["items"]} == {"assistant"} and len(r["items"]) == 3
    r = client.messages(c["id"], **{"$orderby": "created_at desc"})
    assert [m["id"] for m in r["items"]] == [m["id"] for m in reversed(allm)]
    target = allm[3]["id"]
    r = client.messages(c["id"], **{"$filter": f"id eq '{target}'"})
    assert [m["id"] for m in r["items"]] == [target]
    r = client.messages(c["id"], **{"$filter": f"id eq {target}"})
    assert [m["id"] for m in r["items"]] == [target]
    r = client.messages(c["id"], **{"$filter": f"created_at gt {allm[1]['created_at']}"})
    assert [m["id"] for m in r["items"]] == [m["id"] for m in allm[2:]]
    r = client.messages(c["id"], **{"$filter": f"created_at ge {allm[1]['created_at']}"})
    assert [m["id"] for m in r["items"]] == [m["id"] for m in allm[1:]]
    for params in ({"$filter": "content eq 'x'"}, {"$orderby": "content"}, {"limit": 0}, {"cursor": "zz"}):
        r = client.req("GET", f"/v1/chats/{c['id']}/messages", params=params)
        assert r.status_code == 400, params
        assert r.json()["context"]["resource_type"] == "gts.cf.core.odata.query.v1~"
    assert client.req("GET", f"/v1/chats/{uuid.uuid4()}/messages").status_code == 404


def test_error_contract_shape(client):
    r = client.req("GET", f"/v1/chats/{uuid.uuid4()}")
    p = r.json()
    assert r.headers["content-type"].startswith("application/problem+json")
    for k in ("type", "title", "status", "detail", "context"):
        assert k in p
    assert "code" not in p and p["status"] == 404


def test_thread_summary_generated_and_used(tmp_root):
    """Truncation of the context triggers a thread summary that is then sent
    in the next turn and reported in stream_started."""
    import harness

    s = harness.Stack(f"{tmp_root}/summary", mini_chat_overrides={"context": {"recent_messages_limit": 100}})
    s.start()
    try:
        cl = Client(s)
        c = cl.create_chat(model="gpt-tiny")
        # each message is ~1000 bytes (~300 tokens); the tiny budget (1400) truncates soon
        for i in range(5):
            s.mock_script([{"events": [{"type": "response.output_text.delta", "delta": "A" * 1000},
                                       {"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}]}])
            cl.send(c["id"], f"{i}" * 1000)
        summary = wait_for(lambda: s.query("select * from thread_summaries where chat_id = ?", (ubytes(c["id"]),)), timeout=30)
        assert summary and summary[0]["summary_text"] == "Summary of the conversation."
        summary_reqs = [r for r in s.mock_requests("/v1/responses") if not r["json"].get("stream")]
        assert summary_reqs
        body = summary_reqs[0]["json"]
        assert body["metadata"]["request_type"] == "summary" and body["model"] == "gpt-4.1-mini"
        prompt = body["input"][0]["content"][0]["text"]
        assert prompt.startswith("Summarize the following conversation:") and "User: " in prompt
        compressed = s.query("select count(*) n from messages where chat_id = ? and is_compressed = 1", (ubytes(c["id"]),))
        assert compressed[0]["n"] > 0
        s.mock_reset()
        ev = cl.send(c["id"], "next")
        assert ev[0][1]["thread_summary_applied"]["token_estimate"] == 20
        body = [r for r in s.mock_requests("/v1/responses") if r["json"].get("stream")][-1]["json"]
        first = body["input"][0]
        assert first["role"] == "user" and "have been summarized" in first["content"][0]["text"]
        # messages stay visible in history
        assert len(cl.messages(c["id"], limit=100)["items"]) == 12
        # deleting the latest turn twice drops a summary covering it
        msgs = cl.messages(c["id"], limit=100)["items"]
        last = msgs[-1]["request_id"]
        cl.req("DELETE", f"/v1/chats/{c['id']}/turns/{last}")
        for _ in range(5):
            msgs = cl.messages(c["id"], limit=100)["items"]
            if not msgs:
                break
            cl.req("DELETE", f"/v1/chats/{c['id']}/turns/{msgs[-1]['request_id']}")
        assert s.query("select * from thread_summaries where chat_id = ?", (ubytes(c["id"]),)) == []
        assert s.query("select count(*) n from messages where chat_id = ? and is_compressed = 1", (ubytes(c["id"]),))[0]["n"] == 0
    finally:
        s.stop()


def test_chat_completions_adapter(stack, client, mock):
    c = client.create_chat(model="gpt-chat")
    ev = client.send(c["id"], "hello")
    assert [n for n, _ in ev][-1] == "done"
    assert "".join(p["content"] for n, p in ev if n == "delta") == "Hi there"
    assert ev[-1][1]["usage"] == {"input_tokens": 9, "output_tokens": 3}
    req = stack.mock_requests("/chat/completions")[-1]["json"]
    assert req["model"] == "gpt-chat" and req["stream"] is True
    assert req["stream_options"] == {"include_usage": True}
    assert req["messages"][0] == {"role": "system", "content": "You are gpt-chat."}
    assert req["messages"][-1] == {"role": "user", "content": "hello"}
    assert len(req["user"]) == 64
    # attachments of a chat-completions chat go to its rag_provider
    r = client.upload(c["id"], "d.txt", b"x", "text/plain")
    assert r.status_code == 201
    row = stack.query("select storage_backend from attachments where id = ?", (ubytes(r.json()["id"]),))[0]
    assert row["storage_backend"] == "openai"
