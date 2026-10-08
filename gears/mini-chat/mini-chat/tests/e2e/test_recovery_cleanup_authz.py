"""Chat-deletion cleanup, orphan watchdog, error contract, authorization."""

from __future__ import annotations

import uuid

import httpx

import prov
from harness import PREFIX, TOKEN_A, BackgroundStream, auth, from_blob, ub, wait_until

OLD = "2020-01-01T00:00:00.000000001+00:00"


def test_chat_deletion_cleans_provider_resources(env):
    c = env.a
    chat = c.create_chat(model="premium-1")
    doc = c.upload(chat["id"], "d.txt", b"doc", "text/plain").json()
    img = c.upload(chat["id"], "i.png", b"\x89PNG\r\n\x1a\n" + b"0" * 10, "image/png").json()
    c.send(chat["id"], "uses doc", attachment_ids=[doc["id"]])
    vs = env.server.query("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = ?", (ub(chat["id"]),))[0]["vector_store_id"]
    files = {from_blob(r["id"]): r["provider_file_id"] for r in env.server.query("SELECT id, provider_file_id FROM attachments WHERE chat_id = ?", (ub(chat["id"]),))}
    assert c.delete(f"/chats/{chat['id']}").status_code == 204
    # child rows are kept; provider resources are removed asynchronously
    wait_until(lambda: not env.server.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(chat["id"]),)), 20, msg="vector store cleanup")
    rows = env.server.query("SELECT cleanup_status FROM attachments WHERE chat_id = ?", (ub(chat["id"]),))
    assert [r["cleanup_status"] for r in rows] == ["done", "done"]
    deleted = {r["path"] for r in env.mock.requests(method="DELETE")}
    assert f"/v1/files/{files[doc['id']]}" in deleted and f"/v1/files/{files[img['id']]}" in deleted
    assert f"/v1/vector_stores/{vs}" in deleted
    assert env.server.query("SELECT count(*) AS n FROM messages WHERE chat_id = ?", (ub(chat["id"]),))[0]["n"] == 2
    st = env.mock.state()
    assert vs not in st["vector_stores"]


def test_chat_cleanup_with_failing_file_delete(make_env):
    e = make_env({"gears": {"mini-chat": {"config": {"cleanup_worker": {"max_attempts": 2}}}}})
    c = e.a
    chat = c.create_chat()
    a = c.upload(chat["id"], "d.txt", b"doc", "text/plain").json()
    e.mock.config(file_delete_status=500)
    assert c.delete(f"/chats/{chat['id']}").status_code == 204
    wait_until(lambda: e.server.query("SELECT cleanup_status, cleanup_attempts FROM attachments WHERE id = ?", (ub(a["id"]),))[0]["cleanup_status"] == "failed", 40, 0.5, "terminal failure")
    row = e.server.query("SELECT cleanup_attempts, last_cleanup_error FROM attachments WHERE id = ?", (ub(a["id"]),))[0]
    assert row["cleanup_attempts"] == 2 and row["last_cleanup_error"]
    # the vector store is still deleted once every attachment reached a terminal state
    wait_until(lambda: not e.server.query("SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(chat["id"]),)), 30, 0.5, "vs cleanup")


def test_orphan_watchdog_finalizes_stale_turn(env):
    c = env.a
    chat = c.create_chat(model="gpt-4.1-mini")
    env.mock.script([prov.sse(prov.created(), prov.delta("partial"), prov.sleep(6000), prov.completed("partial"))])
    bg = BackgroundStream(c, "POST", f"/chats/{chat['id']}/messages:stream", {"content": "will be orphaned"})
    rid = bg.wait_event("delta") and bg.events[0].data["request_id"]
    # a running turn with recent progress is left alone even when it started long ago
    started = env.server.query("SELECT started_at FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]["started_at"]
    env.server.execute("UPDATE chat_turns SET started_at = ? WHERE request_id = ?", (OLD, ub(rid)))
    import time

    time.sleep(2.5)
    assert c.turn(chat["id"], rid).json()["state"] == "running"
    # stale progress: finalized as failed / orphan_timeout with an estimated settlement
    # (started_at restored: the settlement period is derived from it)
    env.server.execute("UPDATE chat_turns SET started_at = ?, last_progress_at = ? WHERE request_id = ?", (started, OLD, ub(rid)))
    t = wait_until(lambda: (lambda j: j if j["state"] == "error" else None)(c.turn(chat["id"], rid).json()), 10, msg="orphan")
    assert t["error_code"] == "orphan_timeout"
    row = env.server.query("SELECT * FROM chat_turns WHERE request_id = ?", (ub(rid),))[0]
    assert row["state"] == "failed" and row["completed_at"] is not None
    line = wait_until(lambda: [l for l in env.server.log_text().splitlines() if "usage event published" in l and uuid.UUID(rid).hex in l], msg="usage")
    assert "billing_outcome=aborted" in line[0] and "settlement_method=estimated" in line[0]
    # the stream later loses the CAS and ends with stream_interrupted
    bg.join(30)
    assert bg.events[-1].event == "error"
    assert bg.events[-1].data["code"] == "stream_interrupted"
    assert "done" not in bg.names
    # the chat accepts new turns
    assert c.send(chat["id"], "next").names[-1] == "done"


def test_error_contract(env):
    c = env.a
    r = c.get(f"/chats/{uuid.uuid4()}")
    assert r.status_code == 404
    assert r.headers["content-type"].startswith("application/problem+json")
    p = r.json()
    assert {"type", "title", "status", "detail", "context"} <= set(p)
    assert "code" not in p
    assert p["status"] == 404 and p["type"] == "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~"
    assert p["instance"].endswith(r.request.url.path)
    r = c.post("/chats", json={"title": ""})
    p = r.json()
    assert p["type"].endswith("invalid_argument.v1~") and p["context"]["field_violations"][0]["reason"] == "INVALID_TITLE"


def test_isolation_between_users_and_tenants(env):
    a, a2, b = env.a, env.a2, env.b
    chat = a.create_chat(title="private")
    rid = a.send(chat["id"], "secret").started["request_id"]
    att = a.upload(chat["id"], "x.txt", b"x", "text/plain").json()
    msg = a.messages(chat["id"])[1]
    for other in (a2, b):
        assert other.get(f"/chats/{chat['id']}").status_code == 404
        assert other.patch(f"/chats/{chat['id']}", json={"title": "x"}).status_code == 404
        assert other.get(f"/chats/{chat['id']}/messages").status_code == 404
        assert other.send(chat["id"], "hi").status == 404
        assert other.turn(chat["id"], rid).status_code == 404
        assert other.retry(chat["id"], rid).status == 404
        assert other.delete(f"/chats/{chat['id']}/turns/{rid}").status_code == 404
        assert other.get(f"/chats/{chat['id']}/attachments/{att['id']}").status_code == 404
        assert other.upload(chat["id"], "y.txt", b"y", "text/plain").status_code == 404
        assert other.put(f"/chats/{chat['id']}/messages/{msg['id']}/reaction", json={"reaction": "like"}).status_code == 404
        assert other.delete(f"/chats/{chat['id']}").status_code == 404
        ids = [x["id"] for x in other.get("/chats", params={"limit": 100}).json()["items"]]
        assert chat["id"] not in ids
    # the owner still sees everything
    assert a.get(f"/chats/{chat['id']}").status_code == 200
    # quota status is per user
    b_status = b.quota()
    assert all(p["used_credits_micro"] == 0 for t in b_status["tiers"] for p in t["periods"])
    # tenant scoping in storage
    row = env.server.query("SELECT tenant_id, user_id FROM chats WHERE id = ?", (ub(chat["id"]),))[0]
    assert from_blob(row["tenant_id"]) == "00000000-df51-5b42-9538-d2b56b7ee953"
    # b's own chat lives in b's tenant
    bc = b.create_chat()
    row = env.server.query("SELECT tenant_id FROM chats WHERE id = ?", (ub(bc["id"]),))[0]
    assert from_blob(row["tenant_id"]) == "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
    assert b.send(bc["id"], "tenant b works").names[-1] == "done"
