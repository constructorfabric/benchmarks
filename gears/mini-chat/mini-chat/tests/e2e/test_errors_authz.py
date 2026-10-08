"""Canonical error contract and owner/tenant isolation across all resources."""

from __future__ import annotations

import uuid

from conftest import assert_problem, ok_stream

CHAT_RT = "gts.cf.core.mini_chat.chat.v1~"


def _populated(api):
    chat = api.create_chat(title="private")
    rid = str(uuid.uuid4())
    att = api.upload(chat["id"], b"secret", "s.txt", "text/plain").json()
    ok_stream(api.send(chat["id"], "secret question", request_id=rid))
    asst = api.messages(chat["id"])[1]
    return chat, rid, att, asst


def test_problem_shape(api):
    r = api.get(f"/chats/{uuid.uuid4()}")
    body = assert_problem(r, 404)
    assert body["type"].startswith("gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~")
    assert body["title"]
    assert body["instance"].startswith("/mini-chat/v1/chats/")
    assert body["context"]["resource_type"] == CHAT_RT


def test_unauthenticated_everywhere(server):
    anon = server.anonymous()
    cid = str(uuid.uuid4())
    calls = [
        ("GET", "/chats"),
        ("POST", "/chats"),
        ("GET", f"/chats/{cid}"),
        ("PATCH", f"/chats/{cid}"),
        ("DELETE", f"/chats/{cid}"),
        ("GET", f"/chats/{cid}/messages"),
        ("POST", f"/chats/{cid}/messages:stream"),
        ("POST", f"/chats/{cid}/attachments"),
        ("GET", f"/chats/{cid}/attachments/{cid}"),
        ("DELETE", f"/chats/{cid}/attachments/{cid}"),
        ("GET", f"/chats/{cid}/turns/{cid}"),
        ("POST", f"/chats/{cid}/turns/{cid}/retry"),
        ("PATCH", f"/chats/{cid}/turns/{cid}"),
        ("DELETE", f"/chats/{cid}/turns/{cid}"),
        ("PUT", f"/chats/{cid}/messages/{cid}/reaction"),
        ("DELETE", f"/chats/{cid}/messages/{cid}/reaction"),
        ("GET", "/models"),
        ("GET", "/models/gpt-standard"),
        ("GET", "/quota/status"),
    ]
    for method, path in calls:
        if path.endswith("/attachments"):
            r = anon.req(method, path, files={"file": ("a.txt", b"x", "text/plain")})
        else:
            r = anon.req(method, path, json={})
        assert r.status_code == 401, (method, path, r.status_code)
    bad = server.api(0, 0)
    bad.http.headers["Authorization"] = "Bearer not-a-token"
    assert bad.get("/chats").status_code == 401


def test_invalid_path_params(api):
    chat = api.create_chat()
    for method, path in (
        ("GET", "/chats/xyz"),
        ("DELETE", "/chats/xyz"),
        ("GET", "/chats/xyz/messages"),
        ("GET", f"/chats/{chat['id']}/turns/not-a-uuid"),
        ("GET", f"/chats/{chat['id']}/attachments/not-a-uuid"),
        ("PUT", f"/chats/{chat['id']}/messages/nope/reaction"),
    ):
        r = api.req(method, path, json={"reaction": "like"})
        assert_problem(r, 400, category="invalid_argument")


def test_cross_tenant_and_cross_user_isolation(api, other_user, tenant_b_user):
    chat, rid, att, asst = _populated(api)
    cid = chat["id"]
    for intruder in (other_user, tenant_b_user):
        assert_problem(intruder.get(f"/chats/{cid}"), 404, resource_type=CHAT_RT)
        assert_problem(intruder.get(f"/chats/{cid}/messages"), 404, resource_type=CHAT_RT)
        assert_problem(intruder.send(cid, "hijack"), 404, resource_type=CHAT_RT)
        assert_problem(intruder.turn(cid, rid), 404)
        assert_problem(intruder.retry(cid, rid), 404)
        assert_problem(intruder.edit(cid, rid, "x"), 404)
        assert_problem(intruder.delete(f"/chats/{cid}/turns/{rid}"), 404)
        assert_problem(intruder.get(f"/chats/{cid}/attachments/{att['id']}"), 404)
        assert_problem(intruder.delete(f"/chats/{cid}/attachments/{att['id']}"), 404)
        assert_problem(intruder.upload(cid, b"x", "x.txt", "text/plain"), 404)
        assert_problem(intruder.put(f"/chats/{cid}/messages/{asst['id']}/reaction", json={"reaction": "like"}), 404)
        assert_problem(intruder.patch(f"/chats/{cid}", json={"title": "x"}), 404)
        assert_problem(intruder.delete(f"/chats/{cid}"), 404)
        assert cid not in [c["id"] for c in intruder.get("/chats").json()["items"]]
    # the owner still sees everything unchanged
    assert api.get(f"/chats/{cid}").json()["title"] == "private"
    assert len(api.messages(cid)) == 2


def test_attachment_of_other_user_in_shared_scope_is_invisible(api, other_user):
    # the uploader check applies even when the ids are known
    chat = api.create_chat()
    att = api.upload(chat["id"], b"x", "x.txt", "text/plain").json()
    other_chat = other_user.create_chat()
    assert_problem(other_user.get(f"/chats/{other_chat['id']}/attachments/{att['id']}"), 404)
    r = other_user.send(other_chat["id"], "x", attachment_ids=[att["id"]])
    assert_problem(r, 400, reason="invalid_attachment")


def test_quota_status_is_per_user(api, other_user):
    chat = api.create_chat()
    ok_stream(api.send(chat["id"], "x"))
    mine = api.get("/quota/status").json()
    theirs = other_user.get("/quota/status").json()
    used = lambda s: sum(p["used_credits_micro"] for t in s["tiers"] for p in t["periods"])  # noqa: E731
    assert used(mine) > 0
    assert used(theirs) == 0


def test_streaming_and_rest_errors_share_codes(api, mock):
    """A pre-stream rejection is a REST Problem; a provider failure is a terminal SSE error."""
    chat = api.create_chat()
    r = api.send(chat["id"], "")
    assert r.events == []
    assert_problem(r, 400, reason="EMPTY_CONTENT")
    mock.script({"http_status": 503})
    r = api.send(chat["id"], "x")
    assert r.status == 200
    assert r.names[-1] == "error"
    assert set(r.first("error")) == {"code", "message"}


ODATA_RT = "gts.cf.core.odata.query.v1~"


def test_odata_error_contract(api):
    chat = api.create_chat()
    for path in ("/chats", f"/chats/{chat['id']}/messages"):
        cases = [
            ({"limit": 0}, "INVALID_LIMIT"),
            ({"$filter": "nosuch eq 1"}, "INVALID_FILTER"),
            ({"$orderby": "nosuch desc"}, "INVALID_ORDERBY_FIELD"),
            ({"cursor": "%%%garbage"}, "INVALID_CURSOR"),
            ({"$skip": "1"}, "UNSUPPORTED_QUERY_PARAM"),
        ]
        for params, reason in cases:
            r = api.get(path, params=params)
            assert_problem(r, 400, category="invalid_argument", reason=reason, resource_type=ODATA_RT)


def test_platform_extractor_reasons(api):
    chat = api.create_chat()
    assert_problem(api.post("/chats", json={"title": 5}), 422, reason="invalid_json_body")
    assert_problem(
        api.post("/chats", content=b"{oops", headers={"content-type": "application/json"}), 400, reason="json_syntax_error"
    )
    assert_problem(
        api.post("/chats", content=b"{}", headers={"content-type": "text/plain"}), 415, reason="missing_json_content_type"
    )
    assert_problem(api.get("/chats/nope"), 400, reason="invalid_path_params")
    r = api.send(chat["id"], "x", attachment_ids=["not-a-uuid"])
    assert_problem(r, 422, reason="invalid_json_body")


def test_whitespace_content_is_empty(api, mock):
    chat = api.create_chat()
    assert_problem(api.send(chat["id"], "   \n\t "), 400, reason="EMPTY_CONTENT", field="content")
    assert mock.chat_requests(chat["id"]) == []


def test_too_many_attachment_ids(api):
    chat = api.create_chat()
    ids = [str(uuid.uuid4()) for _ in range(55)]
    assert_problem(api.send(chat["id"], "x", attachment_ids=ids), 400, reason="invalid_attachment")


def test_duplicate_attachment_ids(api):
    chat = api.create_chat()
    a = api.upload(chat["id"], b"x", "x.txt", "text/plain").json()
    assert_problem(api.send(chat["id"], "x", attachment_ids=[a["id"], a["id"]]), 400, reason="invalid_attachment")


def test_retry_after_on_storage_failure(api, mock):
    chat = api.create_chat()
    mock.config(files_upload_status=502)
    r = api.upload(chat["id"], b"x", "x.txt", "text/plain")
    body = assert_problem(r, 503, category="service_unavailable")
    assert r.headers["retry-after"] == "10"
    assert body["context"].get("retry_after_seconds") == 10
