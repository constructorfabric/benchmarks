"""S2 chats: Chat CRUD, listing, authorization / isolation and OData errors.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "Create Chat", "List Chats", "Get Chat", "Update Chat Title",
  "Delete Chat" (``ChatDetailDto``, ``Page_ChatDetailDto``, ``PageInfo``,
  ``CreateChatReq``, ``UpdateChatReq``; operation ids ``mini_chat.create_chat``,
  ``list_chats``, ``get_chat``, ``update_chat``, ``delete_chat``);
* §3.3 "Error Codes": ``INVALID_TITLE`` / ``INVALID_MODEL`` field violations,
  platform JSON extractor (400 ``json_syntax_error``, 422 ``invalid_json_body``,
  415 ``missing_json_content_type``), path extractor (``invalid_path_params``),
  OData errors with ``context.resource_type = gts.cf.core.odata.query.v1~``;
* §3.8 owner-only access, 404 masking of other users' / tenants' chats;
* "Cleanup on Chat Deletion" + ADR-0009: soft delete, attachments marked
  ``cleanup_status = pending`` and a chat-cleanup outbox message
  (queue ``mini-chat.chat_cleanup``) in the same transaction, children
  untouched, second ``DELETE`` is 404.

Every test creates its own chats; list assertions are scoped with a unique
title (``$filter=title eq '<token>'``) so the shared server's other chats do
not interfere.
"""

from __future__ import annotations

import time
import uuid
from typing import Any

import httpx
import pytest

from mc_helpers import (
    RT_CHAT,
    RT_ODATA_QUERY,
    assert_problem,
    db_uuid,
    problem_category,
    uuid_from_db,
    wait_until,
)
from mc_seed import (
    CHAT_CLEANUP_QUEUE,
    is_rfc3339,
    outbox_payloads_for_chat,
    outbox_snapshot,
    parse_ts_ns,
    seed_attachment,
    seed_message,
    seed_reaction,
    seed_turn,
)

CHATS = "/mini-chat/v1/chats"
DEFAULT_MODEL = "gpt-4.1"
CHAT_DETAIL_REQUIRED = {"id", "model", "is_temporary", "message_count", "created_at", "updated_at"}
CHAT_DETAIL_FIELDS = CHAT_DETAIL_REQUIRED | {"title"}
CLEANUP_PAYLOAD_FIELDS = {"tenant_id", "chat_id", "system_request_id", "reason", "chat_deleted_at"}


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _chat_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}"


def _token(prefix: str = "chat") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:12]}"


def _create(mc, user: str = "A1", **body: Any) -> dict[str, Any]:
    resp = mc.post(CHATS, user, json=body)
    assert resp.status_code == 201, resp.text
    return resp.json()


def _rename(mc, chat_id: str, title: str, user: str = "A1") -> dict[str, Any]:
    resp = mc.patch(_chat_path(chat_id), user, json={"title": title})
    assert resp.status_code == 200, resp.text
    return resp.json()


def _list(mc, user: str = "A1", **params: Any) -> dict[str, Any]:
    resp = mc.get(CHATS, user, params=params)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    _assert_page_shape(body)
    return body


def _list_titled(mc, title: str, user: str = "A1", **params: Any) -> dict[str, Any]:
    return _list(mc, user, **{"$filter": f"title eq '{title}'", **params})


def _ids(page: dict[str, Any]) -> list[str]:
    return [item["id"] for item in page["items"]]


def _assert_page_shape(body: Any) -> None:
    assert isinstance(body, dict) and set(body) == {"items", "page_info"}, body
    assert isinstance(body["items"], list)
    page_info = body["page_info"]
    assert isinstance(page_info, dict) and "limit" in page_info, page_info
    assert set(page_info) <= {"limit", "next_cursor", "prev_cursor"}, page_info
    for item in body["items"]:
        _assert_chat_detail(item)


def _assert_chat_detail(item: dict[str, Any]) -> None:
    """``item`` is exactly a ``ChatDetailDto`` (title omitted when absent)."""
    keys = set(item)
    assert CHAT_DETAIL_REQUIRED <= keys, f"missing ChatDetail fields {CHAT_DETAIL_REQUIRED - keys}: {item}"
    assert keys <= CHAT_DETAIL_FIELDS, f"unexpected ChatDetail fields {keys - CHAT_DETAIL_FIELDS}: {item}"
    assert uuid.UUID(item["id"])
    assert isinstance(item["model"], str) and item["model"]
    assert isinstance(item["is_temporary"], bool)
    assert isinstance(item["message_count"], int) and not isinstance(item["message_count"], bool)
    assert item["message_count"] >= 0
    if "title" in item:
        assert isinstance(item["title"], str) and item["title"], f"title must be omitted, not null/empty: {item}"
    assert is_rfc3339(item["created_at"]), item["created_at"]
    assert is_rfc3339(item["updated_at"]), item["updated_at"]
    assert parse_ts_ns(item["updated_at"]) >= parse_ts_ns(item["created_at"]), item


def _chat_row(mc, chat_id: str) -> dict[str, Any] | None:
    rows = mc.query("SELECT * FROM chats WHERE id = ?", (db_uuid(chat_id),))
    return rows[0] if rows else None


def _count_user_chats(mc, user: str) -> int:
    ident = mc.identity(user)
    return mc.query(
        "SELECT COUNT(*) AS n FROM chats WHERE user_id = ?", (db_uuid(ident.user_id),)
    )[0]["n"]


def _assert_extractor_problem(resp: httpx.Response, status: int, reason: str) -> dict[str, Any]:
    """Platform JSON extractor rejection: ``invalid_argument`` with a ``body`` field violation.

    422 / 415 are not the canonical status of ``invalid_argument``, so the
    category is checked separately from :func:`assert_problem`.
    """
    body = assert_problem(resp, status, field_reason=reason, field_name="body")
    assert problem_category(body) == "invalid_argument", body
    return body


def _assert_odata_problem(resp: httpx.Response, field_reason: str | None = None) -> dict[str, Any]:
    return assert_problem(
        resp,
        400,
        category="invalid_argument",
        field_reason=field_reason,
        resource_type=RT_ODATA_QUERY,
    )


# --------------------------------------------------------------------------
# Create
# --------------------------------------------------------------------------


def test_create_chat_without_fields_returns_default_model_chat_detail(mc):
    resp = mc.post(CHATS, "A1", json={})

    assert resp.status_code == 201, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    body = resp.json()
    _assert_chat_detail(body)
    assert resp.headers.get("location") == f"/mini-chat/v1/chats/{body['id']}"
    assert body["model"] == DEFAULT_MODEL
    assert body["is_temporary"] is False
    assert body["message_count"] == 0
    assert "title" not in body, f"an untitled chat omits 'title': {body}"
    for hidden in ("user_id", "tenant_id", "deleted_at"):
        assert hidden not in body, body


def test_create_chat_persists_caller_tenant_and_user(mc):
    body = _create(mc, "A1")

    row = _chat_row(mc, body["id"])
    assert row is not None
    ident = mc.identity("A1")
    assert uuid_from_db(row["tenant_id"]) == ident.tenant_id
    assert uuid_from_db(row["user_id"]) == ident.user_id
    assert row["model"] == DEFAULT_MODEL
    assert row["title"] is None
    assert not row["is_temporary"]
    assert row["deleted_at"] is None
    raw = mc.query(
        "SELECT typeof(id) AS t_id, typeof(tenant_id) AS t_tenant, typeof(user_id) AS t_user "
        "FROM chats WHERE id = ?",
        (db_uuid(body["id"]),),
    )[0]
    assert raw == {"t_id": "blob", "t_tenant": "blob", "t_user": "blob"}, raw


def test_create_chat_for_tenant_b_user_stores_tenant_b(mc):
    body = _create(mc, "B1")

    row = _chat_row(mc, body["id"])
    assert uuid_from_db(row["tenant_id"]) == mc.identity("B1").tenant_id
    assert uuid_from_db(row["user_id"]) == mc.identity("B1").user_id


def test_create_chat_trims_title(mc):
    body = _create(mc, title="  Hello  ")

    assert body["title"] == "Hello"
    assert _chat_row(mc, body["id"])["title"] == "Hello"


def test_create_chat_accepts_255_character_title(mc):
    title = "t" * 255

    body = _create(mc, title=title)

    assert body["title"] == title


def test_create_chat_title_length_is_checked_after_trim(mc):
    title = "x" * 255

    body = _create(mc, title=f"   {title}\t ")

    assert body["title"] == title


def test_create_chat_with_null_title_is_untitled(mc):
    body = _create(mc, title=None)

    assert "title" not in body
    assert _chat_row(mc, body["id"])["title"] is None


@pytest.mark.parametrize(
    "title",
    [
        pytest.param("", id="empty"),
        pytest.param("   ", id="spaces"),
        pytest.param(" \t\n ", id="whitespace-mix"),
        pytest.param("x" * 256, id="256-chars"),
    ],
)
def test_create_chat_with_invalid_title_is_rejected(mc, title):
    before = _count_user_chats(mc, "A1")

    resp = mc.post(CHATS, "A1", json={"title": title})

    assert_problem(resp, 400, category="invalid_argument", field_reason="INVALID_TITLE", field_name="title")
    assert _count_user_chats(mc, "A1") == before, "a rejected create must not insert a chat"


@pytest.mark.parametrize("model", ["gpt-4.1-mini", "gpt-text-only"])
def test_create_chat_with_explicit_model_keeps_it(mc, model):
    body = _create(mc, model=model)

    assert body["model"] == model
    assert _chat_row(mc, body["id"])["model"] == model
    assert mc.get(_chat_path(body["id"]), "A1").json()["model"] == model


@pytest.mark.parametrize("model", ["gpt-does-not-exist", "gpt-disabled", ""])
def test_create_chat_with_unknown_or_disabled_model_is_rejected(mc, model):
    before = _count_user_chats(mc, "A1")

    resp = mc.post(CHATS, "A1", json={"title": "Model check", "model": model})

    assert_problem(resp, 400, category="invalid_argument", field_reason="INVALID_MODEL", field_name="model")
    assert _count_user_chats(mc, "A1") == before


def test_create_chat_validates_title_before_model(mc):
    resp = mc.post(CHATS, "A1", json={"title": "   ", "model": "gpt-does-not-exist"})

    body = assert_problem(
        resp, 400, category="invalid_argument", field_reason="INVALID_TITLE", field_name="title"
    )
    reasons = {v.get("reason") for v in body["context"].get("field_violations") or []}
    assert "INVALID_MODEL" not in reasons, body


def test_create_chat_never_takes_owner_or_flags_from_the_body(mc):
    resp = mc.post(
        CHATS,
        "A1",
        json={"title": "Extra", "is_temporary": True, "user_id": mc.identity("A2").user_id,
              "tenant_id": mc.identity("B1").tenant_id},
    )

    # CreateChatReq has only `title` and `model`; extra fields are either
    # ignored (201) or rejected by the JSON extractor (422), never applied.
    if resp.status_code == 422:
        _assert_extractor_problem(resp, 422, "invalid_json_body")
        return
    assert resp.status_code == 201, resp.text
    body = resp.json()
    assert body["is_temporary"] is False
    row = _chat_row(mc, body["id"])
    assert uuid_from_db(row["user_id"]) == mc.identity("A1").user_id
    assert uuid_from_db(row["tenant_id"]) == mc.identity("A1").tenant_id


def test_create_chat_with_malformed_json_is_400(mc):
    resp = mc.post(
        CHATS, "A1", content=b'{"title": "broken"', headers={"Content-Type": "application/json"}
    )

    _assert_extractor_problem(resp, 400, "json_syntax_error")


def test_create_chat_with_wrong_title_type_is_422(mc):
    resp = mc.post(CHATS, "A1", json={"title": 5})

    _assert_extractor_problem(resp, 422, "invalid_json_body")


def test_create_chat_without_json_content_type_is_415(mc):
    resp = mc.post(CHATS, "A1", content=b'{"title": "no content type"}')

    _assert_extractor_problem(resp, 415, "missing_json_content_type")


def test_create_chat_without_token_is_unauthenticated(mc):
    resp = mc.post(CHATS, user=None, json={})

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")


# --------------------------------------------------------------------------
# Get
# --------------------------------------------------------------------------


def test_get_chat_returns_the_created_chat_detail(mc):
    created = _create(mc, title="Get me", model="gpt-4.1-mini")

    resp = mc.get(_chat_path(created["id"]), "A1")

    assert resp.status_code == 200, resp.text
    body = resp.json()
    _assert_chat_detail(body)
    assert body == created


def test_get_untitled_chat_omits_title(mc):
    created = _create(mc)

    body = mc.get(_chat_path(created["id"]), "A1").json()

    assert "title" not in body
    assert body == created


@pytest.mark.parametrize("other", ["A2", "B1"])
def test_get_other_users_chat_is_not_found(mc, other):
    created = _create(mc, "A1", title="Private")

    resp = mc.get(_chat_path(created["id"]), other)

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_foreign_and_unknown_chats_are_indistinguishable(mc):
    created = _create(mc, "A1", title="Masked")

    foreign = assert_problem(
        mc.get(_chat_path(created["id"]), "B1"), 404, category="not_found", resource_type=RT_CHAT
    )
    unknown = assert_problem(
        mc.get(_chat_path(str(uuid.uuid4())), "B1"), 404, category="not_found", resource_type=RT_CHAT
    )

    assert foreign["type"] == unknown["type"]
    assert foreign["title"] == unknown["title"]
    assert set(foreign["context"]) == set(unknown["context"])
    assert created["id"] not in foreign["detail"]


def test_get_unknown_chat_is_not_found(mc):
    resp = mc.get(_chat_path(str(uuid.uuid4())), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


@pytest.mark.parametrize("bad_id", ["not-a-uuid", "12345"])
def test_get_chat_with_non_uuid_id_is_invalid_path_params(mc, bad_id):
    resp = mc.get(_chat_path(bad_id), "A1")

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_get_chat_without_token_is_unauthenticated(mc):
    created = _create(mc)

    resp = mc.get(_chat_path(created["id"]), user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")


def test_get_chat_message_count_counts_non_deleted_messages(mc):
    created = _create(mc, title="Counting")
    ident = mc.identity("A1")
    seed_turn(mc, created["id"], ident.tenant_id, ident.user_id, offset_ms=-50)
    seed_turn(mc, created["id"], ident.tenant_id, ident.user_id, state="failed", offset_ms=-30)
    seed_message(mc, created["id"], ident.tenant_id, role="user", content="gone", deleted=True)

    body = mc.get(_chat_path(created["id"]), "A1").json()

    assert body["message_count"] == 3, body
    listed = _list(mc, **{"$filter": f"id eq {created['id']}"})
    assert [item["message_count"] for item in listed["items"]] == [3]


# --------------------------------------------------------------------------
# Update (PATCH)
# --------------------------------------------------------------------------


def test_rename_chat_updates_title_and_updated_at(mc):
    created = _create(mc, title="Before", model="gpt-4.1-mini")
    time.sleep(0.02)

    resp = mc.patch(_chat_path(created["id"]), "A1", json={"title": "  After  "})

    assert resp.status_code == 200, resp.text
    body = resp.json()
    _assert_chat_detail(body)
    assert body["title"] == "After"
    assert body["id"] == created["id"]
    assert body["model"] == "gpt-4.1-mini"
    assert body["is_temporary"] == created["is_temporary"]
    assert body["message_count"] == created["message_count"]
    assert body["created_at"] == created["created_at"]
    assert parse_ts_ns(body["updated_at"]) > parse_ts_ns(created["updated_at"])
    assert mc.get(_chat_path(created["id"]), "A1").json() == body
    assert _chat_row(mc, created["id"])["title"] == "After"


def test_rename_chat_ignores_model_and_unknown_fields(mc):
    created = _create(mc, title="Locked model")

    resp = mc.patch(
        _chat_path(created["id"]),
        "A1",
        json={"title": "Renamed", "model": "gpt-4.1-mini", "is_temporary": True, "foo": 1},
    )

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["title"] == "Renamed"
    assert body["model"] == DEFAULT_MODEL
    assert body["is_temporary"] is False
    row = _chat_row(mc, created["id"])
    assert row["model"] == DEFAULT_MODEL
    assert not row["is_temporary"]


def test_rename_untitled_chat_sets_a_title(mc):
    created = _create(mc)

    body = _rename(mc, created["id"], "Now titled")

    assert body["title"] == "Now titled"


@pytest.mark.parametrize(
    "payload",
    [
        pytest.param({}, id="missing-title"),
        pytest.param({"title": None}, id="null-title"),
        pytest.param({"title": 42}, id="numeric-title"),
        pytest.param({"title": ["a"]}, id="array-title"),
    ],
)
def test_rename_with_schema_mismatch_is_422(mc, payload):
    created = _create(mc, title="Unchanged")

    resp = mc.patch(_chat_path(created["id"]), "A1", json=payload)

    _assert_extractor_problem(resp, 422, "invalid_json_body")
    assert _chat_row(mc, created["id"])["title"] == "Unchanged"


@pytest.mark.parametrize(
    "title",
    [
        pytest.param("", id="empty"),
        pytest.param("    ", id="spaces"),
        pytest.param("x" * 256, id="256-chars"),
    ],
)
def test_rename_with_invalid_title_is_400(mc, title):
    created = _create(mc, title="Unchanged")

    resp = mc.patch(_chat_path(created["id"]), "A1", json={"title": title})

    assert_problem(resp, 400, category="invalid_argument", field_reason="INVALID_TITLE", field_name="title")
    assert _chat_row(mc, created["id"])["title"] == "Unchanged"


def test_rename_with_255_character_title_is_accepted(mc):
    created = _create(mc)

    body = _rename(mc, created["id"], "r" * 255)

    assert body["title"] == "r" * 255


def test_rename_with_malformed_json_is_400(mc):
    created = _create(mc)

    resp = mc.patch(
        _chat_path(created["id"]), "A1", content=b'{"title": ', headers={"Content-Type": "application/json"}
    )

    _assert_extractor_problem(resp, 400, "json_syntax_error")


def test_rename_without_json_content_type_is_415(mc):
    created = _create(mc)

    resp = mc.patch(_chat_path(created["id"]), "A1", content=b'{"title": "x"}')

    _assert_extractor_problem(resp, 415, "missing_json_content_type")


@pytest.mark.parametrize("other", ["A2", "B1"])
def test_rename_foreign_chat_is_not_found(mc, other):
    created = _create(mc, "A1", title="Mine")

    resp = mc.patch(_chat_path(created["id"]), other, json={"title": "Stolen"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)
    assert _chat_row(mc, created["id"])["title"] == "Mine"
    assert mc.get(_chat_path(created["id"]), "A1").json() == created


def test_rename_unknown_chat_is_not_found(mc):
    resp = mc.patch(_chat_path(str(uuid.uuid4())), "A1", json={"title": "Ghost"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_rename_chat_with_non_uuid_id_is_invalid_path_params(mc):
    resp = mc.patch(_chat_path("not-a-uuid"), "A1", json={"title": "x"})

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


# --------------------------------------------------------------------------
# Delete
# --------------------------------------------------------------------------


def test_delete_chat_returns_204_and_hides_the_chat(mc):
    title = _token("delete")
    created = _create(mc, title=title)

    resp = mc.delete(_chat_path(created["id"]), "A1")

    assert resp.status_code == 204, resp.text
    assert resp.content == b""
    assert_problem(mc.get(_chat_path(created["id"]), "A1"), 404, category="not_found", resource_type=RT_CHAT)
    assert _ids(_list_titled(mc, title)) == []
    assert _ids(_list(mc, **{"$filter": f"id eq {created['id']}"})) == []


def test_second_delete_is_not_found(mc):
    created = _create(mc)
    assert mc.delete(_chat_path(created["id"]), "A1").status_code == 204

    resp = mc.delete(_chat_path(created["id"]), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_rename_deleted_chat_is_not_found(mc):
    created = _create(mc, title="Doomed")
    assert mc.delete(_chat_path(created["id"]), "A1").status_code == 204

    resp = mc.patch(_chat_path(created["id"]), "A1", json={"title": "Revived"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_delete_chat_is_a_soft_delete_that_leaves_children_untouched(mc):
    created = _create(mc, title="Soft delete")
    chat_id = created["id"]
    ident = mc.identity("A1")
    turn = seed_turn(mc, chat_id, ident.tenant_id, ident.user_id)
    attachment_id = seed_attachment(mc, chat_id, ident.tenant_id, ident.user_id, kind="document")
    seed_reaction(mc, turn.assistant_message_id, ident.tenant_id, ident.user_id, "like")

    resp = mc.delete(_chat_path(chat_id), "A1")

    assert resp.status_code == 204, resp.text
    row = _chat_row(mc, chat_id)
    assert row is not None, "the chat row must be soft-deleted, not removed"
    assert row["deleted_at"] is not None
    assert is_rfc3339(row["deleted_at"]), row["deleted_at"]
    assert row["title"] == "Soft delete"
    messages = mc.query("SELECT id, deleted_at FROM messages WHERE chat_id = ?", (db_uuid(chat_id),))
    assert {m["id"] for m in messages} == set(turn.message_ids)
    assert all(m["deleted_at"] is None for m in messages), messages
    turns = mc.query("SELECT state, deleted_at FROM chat_turns WHERE chat_id = ?", (db_uuid(chat_id),))
    assert turns == [{"state": "completed", "deleted_at": None}]
    reactions = mc.query(
        "SELECT reaction FROM message_reactions WHERE message_id = ?", (db_uuid(turn.assistant_message_id),)
    )
    assert reactions == [{"reaction": "like"}]
    attachment = mc.query(
        "SELECT deleted_at FROM attachments WHERE id = ?", (db_uuid(attachment_id),)
    )[0]
    assert attachment["deleted_at"] is None


def test_delete_chat_marks_its_attachments_for_cleanup(mc):
    created = _create(mc, title="Cleanup marks")
    chat_id = created["id"]
    ident = mc.identity("A1")
    document = seed_attachment(mc, chat_id, ident.tenant_id, ident.user_id, kind="document")
    image = seed_attachment(mc, chat_id, ident.tenant_id, ident.user_id, kind="image")
    other_chat = _create(mc, title="Untouched neighbour")
    neighbour = seed_attachment(mc, other_chat["id"], ident.tenant_id, ident.user_id)

    assert mc.delete(_chat_path(chat_id), "A1").status_code == 204

    rows = mc.query(
        "SELECT id, cleanup_status FROM attachments WHERE chat_id = ?", (db_uuid(chat_id),)
    )
    statuses = {r["id"]: r["cleanup_status"] for r in rows}
    assert set(statuses) == {document, image}
    # Marked `pending` in the delete transaction. Nothing moves the mark on yet:
    # the chat-cleanup queue is served by the deferred (Retry) handler until the
    # chat-cleanup handler slice lands, which relaxes this to the terminal
    # states as well.
    assert statuses == {document: "pending", image: "pending"}, statuses
    neighbour_row = mc.query(
        "SELECT cleanup_status FROM attachments WHERE id = ?", (db_uuid(neighbour),)
    )[0]
    assert neighbour_row["cleanup_status"] is None


def test_delete_chat_enqueues_a_chat_cleanup_outbox_message(mc):
    created = _create(mc, title="Outbox")
    chat_id = created["id"]
    ident = mc.identity("A1")
    seed_attachment(mc, chat_id, ident.tenant_id, ident.user_id)
    before = outbox_snapshot(mc, CHAT_CLEANUP_QUEUE)

    assert mc.delete(_chat_path(chat_id), "A1").status_code == 204
    payloads = outbox_payloads_for_chat(mc, chat_id, since_body_id=before.body_high_water)

    def enqueued():
        snap = outbox_snapshot(mc, CHAT_CLEANUP_QUEUE)
        return snap if snap.activity > before.activity else None

    after = wait_until(
        enqueued, timeout=10, description=f"a message on outbox queue {CHAT_CLEANUP_QUEUE!r} ({before})"
    )
    assert after.registered_partitions > 0, f"queue {CHAT_CLEANUP_QUEUE!r} is not registered"
    assert after.activity - before.activity == 1, (before, after)
    # The deferred (Retry) handler of the chat-cleanup queue never acknowledges
    # the message, so its body is never vacuumed and stays readable.
    assert len(payloads) == 1, payloads
    payload = payloads[0]
    assert CLEANUP_PAYLOAD_FIELDS <= set(payload), payload
    assert str(payload["chat_id"]).lower() == chat_id
    assert str(payload["tenant_id"]).lower() == ident.tenant_id
    assert payload["reason"] == "chat_soft_delete", payload
    assert uuid.UUID(str(payload["system_request_id"]))
    assert is_rfc3339(payload["chat_deleted_at"]), payload
    deleted_at = _chat_row(mc, chat_id)["deleted_at"]
    assert parse_ts_ns(payload["chat_deleted_at"]) == parse_ts_ns(deleted_at), (payload, deleted_at)


def test_delete_foreign_chat_is_not_found_and_keeps_it(mc):
    created = _create(mc, "A1", title="Keep me")

    for other in ("A2", "B1"):
        resp = mc.delete(_chat_path(created["id"]), other)
        assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)

    assert _chat_row(mc, created["id"])["deleted_at"] is None
    assert mc.get(_chat_path(created["id"]), "A1").status_code == 200


def test_delete_unknown_chat_is_not_found(mc):
    resp = mc.delete(_chat_path(str(uuid.uuid4())), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_delete_chat_with_non_uuid_id_is_invalid_path_params(mc):
    resp = mc.delete(_chat_path("not-a-uuid"), "A1")

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_delete_chat_without_token_is_unauthenticated(mc):
    created = _create(mc)

    resp = mc.delete(_chat_path(created["id"]), user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")
    assert _chat_row(mc, created["id"])["deleted_at"] is None


# --------------------------------------------------------------------------
# List
# --------------------------------------------------------------------------


def test_list_chats_returns_page_of_chat_details_with_default_limit(mc):
    _create(mc, title=_token("list"))

    body = _list(mc)

    assert body["page_info"]["limit"] == 20
    assert 1 <= len(body["items"]) <= 20


def test_list_chats_items_equal_get_chat(mc):
    title = _token("same")
    created = _create(mc, title=title, model="gpt-4.1-mini")

    body = _list_titled(mc, title)

    assert body["items"] == [created]


def test_list_chats_only_returns_callers_non_deleted_chats(mc):
    title = _token("mine")
    kept = _create(mc, "A1", title=title)
    deleted = _create(mc, "A1", title=title)
    _create(mc, "A2", title=title)
    _create(mc, "B1", title=title)
    assert mc.delete(_chat_path(deleted["id"]), "A1").status_code == 204

    body = _list_titled(mc, title, "A1")

    assert _ids(body) == [kept["id"]]


def test_list_chats_is_isolated_between_users_and_tenants(mc):
    title = _token("iso")
    a1_chat = _create(mc, "A1", title=title)
    a2_chat = _create(mc, "A2", title=title)
    b1_chat = _create(mc, "B1", title=title)

    assert _ids(_list_titled(mc, title, "A1")) == [a1_chat["id"]]
    assert _ids(_list_titled(mc, title, "A2")) == [a2_chat["id"]]
    assert _ids(_list_titled(mc, title, "B1")) == [b1_chat["id"]]
    for other in ("A2", "B1"):
        assert _ids(_list(mc, other, **{"$filter": f"id eq {a1_chat['id']}"})) == []
        assert a1_chat["id"] not in _ids(_list(mc, other, limit=100))


def test_list_chats_default_order_is_updated_at_desc(mc):
    title = _token("order")
    first = _create(mc, title=title)
    time.sleep(0.01)
    second = _create(mc, title=title)
    time.sleep(0.01)
    third = _create(mc, title=title)
    assert _ids(_list_titled(mc, title)) == [third["id"], second["id"], first["id"]]
    time.sleep(0.01)

    _rename(mc, first["id"], title)

    body = _list_titled(mc, title)
    assert _ids(body) == [first["id"], third["id"], second["id"]]
    stamps = [parse_ts_ns(item["updated_at"]) for item in body["items"]]
    assert stamps == sorted(stamps, reverse=True)


def test_list_chats_whole_page_is_sorted_by_updated_at_desc(mc):
    _create(mc, title=_token("sorted"))

    body = _list(mc, limit=100)

    stamps = [parse_ts_ns(item["updated_at"]) for item in body["items"]]
    assert stamps == sorted(stamps, reverse=True)


def test_list_chats_cursor_pagination(mc):
    title = _token("page")
    chats = []
    for _ in range(3):
        chats.append(_create(mc, title=title))
        time.sleep(0.01)
    expected = [c["id"] for c in reversed(chats)]

    first = _list_titled(mc, title, limit=2)

    assert first["page_info"]["limit"] == 2
    assert _ids(first) == expected[:2]
    assert first["page_info"].get("next_cursor"), first["page_info"]
    assert first["page_info"].get("prev_cursor") is None

    second = _list_titled(mc, title, limit=2, cursor=first["page_info"]["next_cursor"])

    assert _ids(second) == expected[2:]
    assert second["page_info"].get("next_cursor") is None
    assert second["page_info"].get("prev_cursor"), second["page_info"]
    assert not set(_ids(first)) & set(_ids(second))

    back = _list_titled(mc, title, limit=2, cursor=second["page_info"]["prev_cursor"])

    assert _ids(back) == expected[:2]


def test_list_chats_limit_above_max_is_clamped_to_100(mc):
    body = _list(mc, limit=500)

    assert body["page_info"]["limit"] == 100
    assert len(body["items"]) <= 100


def test_list_chats_with_zero_limit_is_invalid_limit(mc):
    resp = mc.get(CHATS, "A1", params={"limit": 0})

    _assert_odata_problem(resp, "INVALID_LIMIT")


def test_list_chats_filter_by_title(mc):
    title = _token("filter")
    match = _create(mc, title=title)
    _create(mc, title=f"{title}-other")

    body = _list_titled(mc, title)

    assert _ids(body) == [match["id"]]
    assert body["items"][0]["title"] == title


def test_list_chats_filter_by_id(mc):
    created = _create(mc, title=_token("by-id"))
    _create(mc, title=_token("by-id"))

    body = _list(mc, **{"$filter": f"id eq {created['id']}"})

    assert body["items"] == [created]


def test_list_chats_filter_on_unknown_field_is_rejected(mc):
    resp = mc.get(CHATS, "A1", params={"$filter": "model eq 'gpt-4.1'"})

    _assert_odata_problem(resp)


def test_list_chats_malformed_filter_is_invalid_filter(mc):
    resp = mc.get(CHATS, "A1", params={"$filter": "title eq"})

    _assert_odata_problem(resp, "INVALID_FILTER")


def test_list_chats_orderby_title(mc):
    token = _token("sort")
    names = [f"{token}-b", f"{token}-c", f"{token}-a"]
    ids = {name: _create(mc, title=name)["id"] for name in names}
    flt = " or ".join(f"title eq '{name}'" for name in names)

    asc = _list(mc, **{"$filter": flt, "$orderby": "title asc"})
    desc = _list(mc, **{"$filter": flt, "$orderby": "title desc"})

    assert _ids(asc) == [ids[n] for n in sorted(names)]
    assert _ids(desc) == [ids[n] for n in sorted(names, reverse=True)]


@pytest.mark.parametrize("direction", ["asc", "desc"])
def test_list_chats_orderby_title_pages_untitled_chats_exactly_once(mc, direction):
    token = _token("untitled-sort")
    untitled = [_create(mc)["id"] for _ in range(3)]
    titled = {name: _create(mc, title=name)["id"] for name in (f"{token}-b", f"{token}-a")}
    flt = " or ".join(f"id eq {chat_id}" for chat_id in [*untitled, *titled.values()])
    # Untitled chats sort as the empty title; ties fall to the `id desc`
    # tiebreaker (canonical UUID text order is the stored byte order).
    untitled_order = sorted(untitled, reverse=True)
    titled_order = [titled[name] for name in sorted(titled, reverse=direction == "desc")]
    expected = untitled_order + titled_order if direction == "asc" else titled_order + untitled_order

    page = _list(mc, **{"$filter": flt, "$orderby": f"title {direction}", "limit": 2})
    pages = [_ids(page)]
    while page["page_info"].get("next_cursor"):
        assert len(pages) < 5, pages
        page = _list(mc, **{"$filter": flt, "limit": 2, "cursor": page["page_info"]["next_cursor"]})
        pages.append(_ids(page))

    assert [chat_id for ids in pages for chat_id in ids] == expected, pages
    assert [len(ids) for ids in pages] == [2, 2, 1]


def test_list_chats_updated_at_filter_matches_the_echoed_value(mc):
    created = _create(mc, title=_token("echo"))
    stamp = created["updated_at"]
    scoped = f"id eq {created['id']}"

    same = _list(mc, **{"$filter": f"{scoped} and updated_at eq {stamp}"})
    later = _list(mc, **{"$filter": f"{scoped} and updated_at gt {stamp}"})
    not_earlier = _list(mc, **{"$filter": f"{scoped} and updated_at ge {stamp}"})

    assert _ids(same) == [created["id"]]
    assert _ids(later) == []
    assert _ids(not_earlier) == [created["id"]]


def test_list_chats_orderby_unknown_field_is_rejected(mc):
    resp = mc.get(CHATS, "A1", params={"$orderby": "model asc"})

    _assert_odata_problem(resp, "INVALID_ORDERBY_FIELD")


def test_list_chats_malformed_cursor_is_invalid_cursor(mc):
    resp = mc.get(CHATS, "A1", params={"cursor": "not-a-valid-cursor"})

    _assert_odata_problem(resp, "INVALID_CURSOR")


def test_list_chats_select_is_accepted_and_ignored(mc):
    title = _token("select")
    created = _create(mc, title=title)

    body = _list(mc, **{"$filter": f"title eq '{title}'", "$select": "id"})

    assert body["items"] == [created]


def test_list_chats_skip_is_unsupported(mc):
    resp = mc.get(CHATS, "A1", params={"$skip": "1"})

    _assert_odata_problem(resp, "UNSUPPORTED_QUERY_PARAM")


def test_list_chats_message_count_reflects_seeded_messages(mc):
    title = _token("count")
    created = _create(mc, title=title)
    empty = _create(mc, title=title)
    ident = mc.identity("A1")
    seed_turn(mc, created["id"], ident.tenant_id, ident.user_id, offset_ms=-20)
    seed_turn(mc, created["id"], ident.tenant_id, ident.user_id, state="running")
    seed_turn(mc, created["id"], ident.tenant_id, ident.user_id, deleted=True, offset_ms=-40)

    counts = {item["id"]: item["message_count"] for item in _list_titled(mc, title)["items"]}

    assert counts == {created["id"]: 3, empty["id"]: 0}


def test_list_chats_without_token_is_unauthenticated(mc):
    resp = mc.get(CHATS, user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")
