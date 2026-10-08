"""Turn status and tail-only mutations (retry / edit / delete)."""

import threading
import uuid

from harness import parse_sse, ubytes, wait_for


def names(ev):
    return [n for n, _ in ev]


def turns(stack, chat_id):
    return stack.query(
        "select * from chat_turns where chat_id = ? order by started_at", (ubytes(chat_id),)
    )


def reason(r):
    return r.json()["context"].get("reason")


def test_retry_latest_turn(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    first = client.send(c["id"], "question one")
    rid1 = first[0][1]["request_id"]
    r = client.req("POST", f"/v1/chats/{c['id']}/turns/{rid1}/retry")
    assert r.status_code == 200, r.text
    ev = parse_sse(r.text)
    assert names(ev)[0] == "stream_started" and names(ev)[-1] == "done"
    rid2 = ev[0][1]["request_id"]
    assert rid2 != rid1 and uuid.UUID(rid2).version == 4
    assert ev[0][1]["is_new_turn"] is True
    rows = turns(stack, c["id"])
    old = [t for t in rows if uuid.UUID(bytes=t["request_id"]) == uuid.UUID(rid1)][0]
    new = [t for t in rows if uuid.UUID(bytes=t["request_id"]) == uuid.UUID(rid2)][0]
    assert old["deleted_at"] is not None
    assert uuid.UUID(bytes=old["replaced_by_request_id"]) == uuid.UUID(rid2)
    assert new["state"] == "completed" and new["deleted_at"] is None and new["reserve_tokens"] > 0
    msgs = client.messages(c["id"])["items"]
    assert [m["request_id"] for m in msgs] == [rid2, rid2]
    assert msgs[0]["content"] == "question one"
    # the provider got the original content and no deleted history
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    assert [m["content"][0]["text"] for m in body["input"]] == ["question one"]
    # old turn is gone from status, replay of its request id conflicts
    assert client.req("GET", f"/v1/chats/{c['id']}/turns/{rid1}").status_code == 404
    r = client.stream(c["id"], "x", request_id=rid1)
    assert r.status_code == 409 and reason(r) == "request_id_conflict"
    m = wait_for(lambda: [e for e in logs("mutation") if e.get("new_request_id") == rid2])
    assert m[0]["event_type"] == "turn_retry" and m[0]["original_request_id"] == rid1
    assert m[0]["actor_user_id"] == "00000000-0000-0000-0000-0000000000a1"


def test_edit_latest_turn(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    client.send(c["id"], "keep me")
    ev = client.send(c["id"], "old text")
    rid = ev[0][1]["request_id"]
    r = client.req("PATCH", f"/v1/chats/{c['id']}/turns/{rid}", json={"content": "new text"})
    assert r.status_code == 200, r.text
    ev2 = parse_sse(r.text)
    assert names(ev2)[-1] == "done"
    msgs = client.messages(c["id"])["items"]
    assert [m["content"] for m in msgs if m["role"] == "user"] == ["keep me", "new text"]
    assert len(msgs) == 4
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    assert [m["content"][0]["text"] for m in body["input"]][-1] == "new text"
    assert "old text" not in str(body["input"])
    r = client.req("PATCH", f"/v1/chats/{c['id']}/turns/{ev2[0][1]['request_id']}", json={"content": "  "})
    assert r.status_code == 400
    assert r.json()["context"]["field_violations"][0]["reason"] == "EMPTY_CONTENT"
    r = client.req("PATCH", f"/v1/chats/{c['id']}/turns/{ev2[0][1]['request_id']}", json={})
    assert r.status_code == 422


def test_delete_latest_turn(stack, client, mock, logs):
    c = client.create_chat(model="gpt-standard")
    ev1 = client.send(c["id"], "one")
    ev2 = client.send(c["id"], "two")
    rid1, rid2 = ev1[0][1]["request_id"], ev2[0][1]["request_id"]
    # only the latest turn can be mutated
    for method, path, body in (
        ("DELETE", f"/v1/chats/{c['id']}/turns/{rid1}", None),
        ("POST", f"/v1/chats/{c['id']}/turns/{rid1}/retry", None),
        ("PATCH", f"/v1/chats/{c['id']}/turns/{rid1}", {"content": "z"}),
    ):
        r = client.req(method, path, json=body)
        assert r.status_code == 409 and reason(r) == "NOT_LATEST_TURN", r.text
    assert client.req("DELETE", f"/v1/chats/{c['id']}/turns/{rid2}").status_code == 204
    msgs = client.messages(c["id"])["items"]
    assert [m["request_id"] for m in msgs] == [rid1, rid1]
    # deleted turn: 404 status, 409 on mutation, conflict on replay
    assert client.req("GET", f"/v1/chats/{c['id']}/turns/{rid2}").status_code == 404
    r = client.req("DELETE", f"/v1/chats/{c['id']}/turns/{rid2}")
    assert r.status_code == 409 and reason(r) == "NOT_LATEST_TURN"
    # previous turn is the latest again
    assert client.req("DELETE", f"/v1/chats/{c['id']}/turns/{rid1}").status_code == 204
    assert client.messages(c["id"])["items"] == []
    assert client.req("GET", f"/v1/chats/{c['id']}").json()["message_count"] == 0
    m = wait_for(lambda: [e for e in logs("mutation") if e.get("request_id") == rid2])
    assert m[0]["event_type"] == "turn_delete"
    r = client.req("DELETE", f"/v1/chats/{c['id']}/turns/{uuid.uuid4()}")
    assert r.status_code == 404


def test_mutation_of_running_turn(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    mock.mock_script([{"hang": True}])
    with client.c.stream("POST", f"/v1/chats/{c['id']}/messages:stream",
                         json={"content": "slow"}, headers=client.h) as resp:
        it = resp.iter_text()
        buf = next(it)
        rid = parse_sse(buf)[0][1]["request_id"]
        for method, path, body in (
            ("DELETE", f"/v1/chats/{c['id']}/turns/{rid}", None),
            ("POST", f"/v1/chats/{c['id']}/turns/{rid}/retry", None),
            ("PATCH", f"/v1/chats/{c['id']}/turns/{rid}", {"content": "z"}),
        ):
            r = client.req(method, path, json=body)
            assert r.status_code == 400, r.text
            v = r.json()["context"]["violations"][0]
            assert v["subject"] == "turn_state" and v["type"] == "STATE"
    wait_for(lambda: turns(stack, c["id"])[0]["state"] != "running", timeout=20)


def test_mutation_other_user_and_bad_ids(client, client_a2, mock):
    c = client.create_chat(model="gpt-standard")
    rid = client.send(c["id"], "x")[0][1]["request_id"]
    assert client_a2.req("POST", f"/v1/chats/{c['id']}/turns/{rid}/retry").status_code == 404
    assert client_a2.req("DELETE", f"/v1/chats/{c['id']}/turns/{rid}").status_code == 404
    assert client.req("POST", f"/v1/chats/{c['id']}/turns/not-a-uuid/retry").status_code == 400


def test_concurrent_mutations_resolve_deterministically(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    rid = client.send(c["id"], "x")[0][1]["request_id"]
    results = []

    def go():
        r = client.req("POST", f"/v1/chats/{c['id']}/turns/{rid}/retry")
        results.append((r.status_code, r.json().get("context", {}).get("reason") if r.status_code != 200 else None))

    threads = [threading.Thread(target=go) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    ok = [r for r in results if r[0] == 200]
    rejected = [r for r in results if r[0] != 200]
    assert len(ok) == 1, results
    assert all(r[0] == 409 and r[1] in ("NOT_LATEST_TURN", "GENERATION_IN_PROGRESS") for r in rejected), results
    live = [t for t in turns(stack, c["id"]) if t["deleted_at"] is None]
    assert len(live) == 1


def test_preflight_rejection_keeps_previous_turn(tmp_root):
    """A preflight rejection of retry (web search kill switch) changes nothing."""
    import harness

    s = harness.Stack(f"{tmp_root}/ks-retry", kill_switches={"disable_web_search": True})
    s.start()
    try:
        cl = harness.Client(s)
        c = cl.create_chat(model="gpt-standard")
        r = cl.stream(c["id"], "x", web_search={"enabled": True})
        assert r.status_code == 400
        v = r.json()["context"]["violations"][0]
        assert v == {"subject": "web_search", "type": "FEATURE_DISABLED", "description": v["description"]}
        ev = cl.send(c["id"], "x")
        rid = ev[0][1]["request_id"]
        # turn stored without web search -> retry passes the preflight
        r = cl.req("POST", f"/v1/chats/{c['id']}/turns/{rid}/retry")
        assert r.status_code == 200
        # force the stored flag on, then retry is rejected and nothing changes
        new_rid = parse_sse(r.text)[0][1]["request_id"]
        s.execute("update chat_turns set web_search_enabled = 1 where request_id = ?", (ubytes(new_rid),))
        r = cl.req("POST", f"/v1/chats/{c['id']}/turns/{new_rid}/retry")
        assert r.status_code == 400
        live = [t for t in s.query("select * from chat_turns where chat_id = ?", (ubytes(c["id"]),)) if t["deleted_at"] is None]
        assert len(live) == 1 and uuid.UUID(bytes=live[0]["request_id"]) == uuid.UUID(new_rid)
        assert [m["request_id"] for m in cl.messages(c["id"])["items"]] == [new_rid, new_rid]
    finally:
        s.stop()


def test_retry_carries_attachments_and_web_search(stack, client, mock):
    c = client.create_chat(model="gpt-standard")
    up = client.upload(c["id"], "pic.png", harness_png(), "image/png")
    assert up.status_code == 201, up.text
    att = up.json()
    ev = client.send(c["id"], "look", attachment_ids=[att["id"]], web_search={"enabled": True})
    rid = ev[0][1]["request_id"]
    mock.mock_reset()
    r = client.req("POST", f"/v1/chats/{c['id']}/turns/{rid}/retry")
    assert r.status_code == 200
    new_rid = parse_sse(r.text)[0][1]["request_id"]
    msgs = client.messages(c["id"])["items"]
    assert msgs[0]["request_id"] == new_rid
    assert [a["attachment_id"] for a in msgs[0]["attachments"]] == [att["id"]]
    body = stack.mock_requests("/v1/responses")[-1]["json"]
    parts = body["input"][-1]["content"]
    assert parts[0] == {"type": "input_text", "text": "look"}
    assert parts[1]["type"] == "input_image" and parts[1]["file_id"].startswith("file-")
    assert any(t["type"] == "web_search" for t in body["tools"])
    new = [t for t in turns(stack, c["id"]) if uuid.UUID(bytes=t["request_id"]) == uuid.UUID(new_rid)][0]
    assert new["web_search_enabled"] == 1


def harness_png():
    import io
    import struct
    import zlib

    w, h = 4, 4
    raw = b"".join(b"\x00" + b"\xff\x00\x00" * w for _ in range(h))

    def chunk(t, data):
        return struct.pack(">I", len(data)) + t + data + struct.pack(">I", zlib.crc32(t + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
    return io.BytesIO(png).getvalue()
