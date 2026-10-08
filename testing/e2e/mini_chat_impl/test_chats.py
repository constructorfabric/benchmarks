"""Chat CRUD (acceptance: Chat CRUD; Principles: model immutable)."""

import uuid

import pytest

from mchelpers import (
    DEFAULT_MODEL,
    DISABLED_MODEL,
    RT_CHAT,
    assert_problem,
    nonce,
    parse_ts,
)

CHAT_KEYS_REQUIRED = {"id", "model", "is_temporary", "message_count", "created_at", "updated_at"}
CHAT_KEYS_ALLOWED = CHAT_KEYS_REQUIRED | {"title"}


def _assert_chat_detail(c: dict):
    assert CHAT_KEYS_REQUIRED <= set(c), c
    assert set(c) <= CHAT_KEYS_ALLOWED, f"unexpected ChatDetail fields: {set(c) - CHAT_KEYS_ALLOWED}"
    assert "user_id" not in c and "tenant_id" not in c
    uuid.UUID(c["id"])
    parse_ts(c["created_at"])
    parse_ts(c["updated_at"])


# Acceptance: Chat CRUD — create with default model, Location header, title omitted
def test_create_chat_default_model(api):
    r = api.post("/v1/chats", json={})
    assert r.status_code == 201, r.text
    c = r.json()
    _assert_chat_detail(c)
    assert c["model"] == DEFAULT_MODEL  # first enabled entry with preference.is_default
    assert "title" not in c or c["title"] is None
    assert c["is_temporary"] is False
    assert c["message_count"] == 0
    loc = r.headers.get("location")
    assert loc, "201 must carry a Location header"
    assert loc.endswith(f"/v1/chats/{c['id']}")
    assert loc == f"/mini-chat/v1/chats/{c['id']}"


# Acceptance: Chat CRUD — title trimmed, explicit model
def test_create_chat_with_title_and_model(api):
    c = api.create_chat(title="   My chat  ", model="std")
    assert c["title"] == "My chat"
    assert c["model"] == "std"
    got = api.chat(c["id"])
    assert got["title"] == "My chat" and got["model"] == "std"


# Acceptance: Chat CRUD — null title creates an untitled chat
def test_create_chat_null_title(api):
    c = api.create_chat(title=None)
    assert "title" not in c or c["title"] is None


# Acceptance: Chat CRUD — title validation (1..255 after trim)
@pytest.mark.parametrize("title", ["", "    ", "x" * 256, "  " + "y" * 256 + "  "])
def test_create_chat_invalid_title(api, title):
    r = api.post("/v1/chats", json={"title": title})
    body = assert_problem(r, 400, "invalid_argument", field_reason="INVALID_TITLE", field="title")
    assert body["context"]["field_violations"][0]["field"] == "title"


# Acceptance: Chat CRUD — boundary titles accepted
def test_create_chat_title_boundaries(api):
    c = api.create_chat(title="z" * 255)
    assert c["title"] == "z" * 255
    c = api.create_chat(title="   " + "w" * 255 + "   ")
    assert c["title"] == "w" * 255
    c = api.create_chat(title="a")
    assert c["title"] == "a"


# Acceptance: Chat CRUD — model validation (unknown / disabled)
@pytest.mark.parametrize("model", ["no-such-model", DISABLED_MODEL])
def test_create_chat_invalid_model(api, model):
    r = api.post("/v1/chats", json={"model": model})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_MODEL", field="model")


# Acceptance: Chat CRUD — the title is validated before the model lookup
def test_create_chat_title_checked_before_model(api):
    r = api.post("/v1/chats", json={"title": "  ", "model": "no-such-model"})
    assert_problem(r, 400, "invalid_argument", field_reason="INVALID_TITLE")


# Acceptance: Error mapping — schema mismatch / malformed JSON / content type on POST /chats
def test_create_chat_body_errors(api):
    r = api.post("/v1/chats", json={"title": 12})
    assert_problem(r, 422, "invalid_argument")
    r = api.post("/v1/chats", content=b"{not json", headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument", field_reason="json_syntax_error")
    r = api.post("/v1/chats", content=b"{}", headers={"Content-Type": "text/plain"})
    assert_problem(r, 415, "invalid_argument", field_reason="missing_json_content_type")


# Acceptance: Chat CRUD — get returns ChatDetail without embedded messages
def test_get_chat(api):
    c = api.create_chat(title="Get me")
    got = api.chat(c["id"])
    _assert_chat_detail(got)
    assert got["id"] == c["id"] and got["title"] == "Get me"
    assert "messages" not in got


# Acceptance: Chat CRUD — missing chat is 404 with chat resource type
def test_get_unknown_chat(api):
    r = api.get(f"/v1/chats/{uuid.uuid4()}")
    assert_problem(r, 404, "not_found", resource_type=RT_CHAT)


# Acceptance: Chat CRUD — rename keeps model, ignores unknown fields, bumps updated_at
def test_patch_rename_keeps_model(api):
    c = api.create_chat(title="Before", model="std")
    r = api.patch(f"/v1/chats/{c['id']}", json={"title": "  Renamed  ", "model": "prem", "is_temporary": True})
    assert r.status_code == 200, r.text
    u = r.json()
    _assert_chat_detail(u)
    assert u["title"] == "Renamed"
    assert u["model"] == "std"
    assert u["is_temporary"] is False
    assert parse_ts(u["updated_at"]) >= parse_ts(c["updated_at"])
    assert api.chat(c["id"])["title"] == "Renamed"


# Acceptance: Chat CRUD — PATCH title validation and schema errors
def test_patch_invalid(api):
    c = api.create_chat(title="Valid")
    for bad in ["", "   ", "q" * 256]:
        r = api.patch(f"/v1/chats/{c['id']}", json={"title": bad})
        assert_problem(r, 400, "invalid_argument", field_reason="INVALID_TITLE", field="title")
    for body in [{}, {"title": None}, {"title": 5}]:
        r = api.patch(f"/v1/chats/{c['id']}", json=body)
        assert_problem(r, 422, "invalid_argument")
    r = api.patch(f"/v1/chats/{c['id']}", content=b"{", headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument")
    assert api.chat(c["id"])["title"] == "Valid"
    r = api.patch(f"/v1/chats/{uuid.uuid4()}", json={"title": "x"})
    assert_problem(r, 404, "not_found", resource_type=RT_CHAT)


# Acceptance: Chat CRUD — delete is soft, second delete is 404
def test_delete_chat_twice(api, db):
    c = api.create_chat(title="Delete me " + nonce())
    r = api.delete(f"/v1/chats/{c['id']}")
    assert r.status_code == 204, r.text
    assert r.content == b""
    r = api.delete(f"/v1/chats/{c['id']}")
    assert_problem(r, 404, "not_found", resource_type=RT_CHAT)
    assert_problem(api.get(f"/v1/chats/{c['id']}"), 404, "not_found", resource_type=RT_CHAT)
    assert_problem(api.patch(f"/v1/chats/{c['id']}", json={"title": "x"}), 404, "not_found")
    assert_problem(api.get(f"/v1/chats/{c['id']}/messages"), 404, "not_found", resource_type=RT_CHAT)
    # soft delete: row kept with deleted_at
    from mchelpers import ub

    row = db.one("SELECT * FROM chats WHERE id = ?", (ub(c["id"]),))
    assert row is not None and row["deleted_at"] is not None


# Acceptance: Chat CRUD — deleted chats are not listed
def test_deleted_chat_not_listed(api_for):
    a = api_for("tok-l4")
    keep = a.create_chat(title="keep")
    gone = a.create_chat(title="gone")
    assert a.delete(f"/v1/chats/{gone['id']}").status_code == 204
    ids = [c["id"] for c in a.list_chats(limit=100).json()["items"]]
    assert keep["id"] in ids and gone["id"] not in ids


# Acceptance: Principles — a chat's model is immutable once set
def test_model_immutable_across_turns_retry_and_rename(api):
    c = api.create_chat(model="std")
    s = api.stream(c["id"], "first " + nonce())
    assert s.done
    s2 = api.retry(c["id"], s.request_id)
    assert s2.done
    api.patch(f"/v1/chats/{c['id']}", json={"title": "renamed", "model": "prem"})
    assert api.chat(c["id"])["model"] == "std"
    assert s2.done["selected_model"] == "std"


# Acceptance: Chat CRUD — message_count reflects non-deleted messages
def test_message_count_after_turn(api):
    c = api.create_chat()
    assert api.stream(c["id"], "count me " + nonce()).done
    assert api.chat(c["id"])["message_count"] == 2
