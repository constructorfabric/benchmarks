"""S2 messages, reactions and turn status.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "List Messages" (``Page_MiniChatMessageDto``, ``MiniChatMessageDto``,
  ``AttachmentSummaryDto``, ``ImgThumbnailDto``; ``mini_chat.list_messages``):
  ``created_at asc`` default order, required ``request_id`` / ``attachments`` /
  ``my_reaction``, optional ``model`` / ``input_tokens`` / ``output_tokens``
  omitted when absent or 0, OData ``$filter`` / ``$orderby`` over
  ``created_at``, ``id``, ``role``;
* §3.3 "Message Reaction API" (``SetReactionReq``, ``MiniChatReactionDto``;
  ``mini_chat.put_reaction`` / ``delete_reaction``): upsert, idempotent
  delete, assistant-only (``failed_precondition`` ``reaction_target`` /
  ``STATE``), ``INVALID_REACTION`` checked before authorization;
* §3.3 "Turn Status API" (``TurnStatusResponse``; ``mini_chat.get_turn``):
  internal-to-API state mapping, ``error_code`` / ``assistant_message_id``
  omitted when null, soft-deleted turn is 404 (turn type);
* §3.3 "Error Codes" and §3.8 (owner-only, 404 masking with the chat type).

Messages, turns, attachments and reactions are seeded straight into SQLite
(:mod:`mc_seed`); chats are created through the API.
"""

from __future__ import annotations

import base64
import uuid
from typing import Any

import httpx
import pytest

from mc_helpers import (
    RT_CHAT,
    RT_MESSAGE,
    RT_ODATA_QUERY,
    RT_TURN,
    assert_problem,
    db_uuid,
    problem_category,
    uuid_from_db,
)
from mc_seed import (
    SAMPLE_THUMBNAIL_HEIGHT,
    SAMPLE_THUMBNAIL_WIDTH,
    SAMPLE_WEBP_THUMBNAIL,
    SeededTurn,
    is_rfc3339,
    link_message_attachment,
    parse_ts_ns,
    seed_attachment,
    seed_message,
    seed_reaction,
    seed_turn,
)

CHATS = "/mini-chat/v1/chats"
MESSAGE_REQUIRED = {"id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"}
MESSAGE_FIELDS = MESSAGE_REQUIRED | {"model", "input_tokens", "output_tokens"}
ATTACHMENT_SUMMARY_REQUIRED = {"attachment_id", "kind", "filename", "status"}
ATTACHMENT_SUMMARY_FIELDS = ATTACHMENT_SUMMARY_REQUIRED | {"img_thumbnail"}
THUMBNAIL_FIELDS = {"content_type", "width", "height", "data_base64"}
REACTION_FIELDS = {"message_id", "reaction", "created_at"}
TURN_STATUS_REQUIRED = {"request_id", "state", "updated_at"}
TURN_STATUS_FIELDS = TURN_STATUS_REQUIRED | {"error_code", "assistant_message_id"}


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _messages_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages"


def _reaction_path(chat_id: str, message_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages/{message_id}/reaction"


def _turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def _new_chat(mc, user: str = "A1", **body: Any) -> str:
    resp = mc.post(CHATS, user, json={"title": "S2 messages", **body})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _seed(mc, chat_id: str, user: str = "A1", **kwargs: Any) -> SeededTurn:
    ident = mc.identity(user)
    return seed_turn(mc, chat_id, ident.tenant_id, ident.user_id, **kwargs)


def _list_messages(mc, chat_id: str, user: str = "A1", **params: Any) -> dict[str, Any]:
    resp = mc.get(_messages_path(chat_id), user, params=params)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert isinstance(body, dict) and set(body) == {"items", "page_info"}, body
    assert "limit" in body["page_info"], body["page_info"]
    for item in body["items"]:
        _assert_message_dto(item)
    return body


def _by_id(body: dict[str, Any]) -> dict[str, dict[str, Any]]:
    return {item["id"]: item for item in body["items"]}


def _ids(body: dict[str, Any]) -> list[str]:
    return [item["id"] for item in body["items"]]


def _assert_message_dto(item: dict[str, Any]) -> None:
    keys = set(item)
    assert MESSAGE_REQUIRED <= keys, f"missing Message fields {MESSAGE_REQUIRED - keys}: {item}"
    assert keys <= MESSAGE_FIELDS, f"unexpected Message fields {keys - MESSAGE_FIELDS}: {item}"
    assert uuid.UUID(item["id"]) and uuid.UUID(item["request_id"])
    assert item["role"] in {"user", "assistant", "system"}
    assert isinstance(item["content"], str)
    assert isinstance(item["attachments"], list)
    assert item["my_reaction"] in (None, "like", "dislike")
    assert is_rfc3339(item["created_at"]), item["created_at"]
    for key in ("model", "input_tokens", "output_tokens"):
        if key in item:
            assert item[key] is not None, f"{key} must be omitted, not null: {item}"
    for summary in item["attachments"]:
        _assert_attachment_summary(summary)


def _assert_attachment_summary(summary: dict[str, Any]) -> None:
    keys = set(summary)
    assert ATTACHMENT_SUMMARY_REQUIRED <= keys, summary
    assert keys <= ATTACHMENT_SUMMARY_FIELDS, f"unexpected AttachmentSummary fields: {summary}"
    assert summary["kind"] in {"document", "image"}
    assert summary["status"] in {"pending", "uploaded", "ready", "failed"}


def _assert_extractor_problem(resp: httpx.Response, status: int, reason: str) -> None:
    body = assert_problem(resp, status, field_reason=reason, field_name="body")
    assert problem_category(body) == "invalid_argument", body


def _reaction_rows(mc, message_id: str) -> list[dict[str, Any]]:
    return mc.query(
        "SELECT user_id, tenant_id, reaction, created_at FROM message_reactions WHERE message_id = ?",
        (db_uuid(message_id),),
    )


def _delete_chat(mc, chat_id: str, user: str = "A1") -> None:
    resp = mc.delete(f"{CHATS}/{chat_id}", user)
    assert resp.status_code == 204, resp.text


# --------------------------------------------------------------------------
# List messages
# --------------------------------------------------------------------------


def test_list_messages_of_completed_turn(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, user_text="What is 2+2?", assistant_text="4", input_tokens=21, output_tokens=7)

    body = _list_messages(mc, chat_id)

    assert body["page_info"]["limit"] == 20
    assert body["page_info"].get("next_cursor") is None
    assert _ids(body) == [turn.user_message_id, turn.assistant_message_id]
    user, assistant = body["items"]
    assert user["role"] == "user"
    assert user["content"] == "What is 2+2?"
    assert assistant["role"] == "assistant"
    assert assistant["content"] == "4"
    assert user["request_id"] == assistant["request_id"] == turn.request_id
    for message in (user, assistant):
        assert message["attachments"] == []
        assert message["my_reaction"] is None
    assert assistant["model"] == turn.model
    assert assistant["input_tokens"] == 21
    assert assistant["output_tokens"] == 7
    for key in ("model", "input_tokens", "output_tokens"):
        assert key not in user, f"user message must omit {key}: {user}"
    assert parse_ts_ns(user["created_at"]) == parse_ts_ns(turn.started_at)
    assert parse_ts_ns(user["created_at"]) < parse_ts_ns(assistant["created_at"])
    for hidden in ("chat_id", "tenant_id", "provider_response_id", "content_type", "deleted_at"):
        assert hidden not in user and hidden not in assistant


def test_list_messages_is_chronological_across_turns(mc):
    chat_id = _new_chat(mc)
    first = _seed(mc, chat_id, offset_ms=-300)
    second = _seed(mc, chat_id, state="failed", offset_ms=-200)
    third = _seed(mc, chat_id, offset_ms=-100)

    body = _list_messages(mc, chat_id)

    assert _ids(body) == first.message_ids + second.message_ids + third.message_ids
    stamps = [parse_ts_ns(item["created_at"]) for item in body["items"]]
    assert stamps == sorted(stamps)


def test_list_messages_omits_zero_token_counts(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, input_tokens=0, output_tokens=0)

    assistant = _by_id(_list_messages(mc, chat_id))[turn.assistant_message_id]

    assert assistant["model"] == turn.model
    assert "input_tokens" not in assistant
    assert "output_tokens" not in assistant


def test_list_messages_excludes_soft_deleted_messages(mc):
    chat_id = _new_chat(mc)
    ident = mc.identity("A1")
    replaced = _seed(mc, chat_id, deleted=True, offset_ms=-200)
    live = _seed(mc, chat_id, offset_ms=-100)
    stray = seed_message(mc, chat_id, ident.tenant_id, role="user", content="removed", deleted=True)

    body = _list_messages(mc, chat_id)

    assert _ids(body) == live.message_ids
    for gone in replaced.message_ids + [stray]:
        assert gone not in _ids(body)


def test_list_messages_of_empty_chat(mc):
    chat_id = _new_chat(mc)

    body = _list_messages(mc, chat_id)

    assert body["items"] == []
    assert body["page_info"].get("next_cursor") is None


def test_list_messages_filter_by_role(mc):
    chat_id = _new_chat(mc)
    first = _seed(mc, chat_id, offset_ms=-200)
    second = _seed(mc, chat_id, offset_ms=-100)

    body = _list_messages(mc, chat_id, **{"$filter": "role eq 'assistant'"})

    assert _ids(body) == [first.assistant_message_id, second.assistant_message_id]
    assert {item["role"] for item in body["items"]} == {"assistant"}


def test_list_messages_filter_by_id(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    body = _list_messages(mc, chat_id, **{"$filter": f"id eq {turn.assistant_message_id}"})

    assert _ids(body) == [turn.assistant_message_id]


def test_list_messages_filter_by_quoted_id_as_documented_for_turn_recovery(mc):
    """DESIGN "Turn Status API": clients fetch the assistant message with
    ``$filter=id eq '{assistant_message_id}'``."""
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    body = _list_messages(mc, chat_id, **{"$filter": f"id eq '{turn.assistant_message_id}'"})

    assert _ids(body) == [turn.assistant_message_id]


def test_list_messages_orderby_created_at_desc(mc):
    chat_id = _new_chat(mc)
    first = _seed(mc, chat_id, offset_ms=-200)
    second = _seed(mc, chat_id, offset_ms=-100)

    body = _list_messages(mc, chat_id, **{"$orderby": "created_at desc"})

    assert _ids(body) == list(reversed(first.message_ids + second.message_ids))


def test_list_messages_cursor_pagination(mc):
    chat_id = _new_chat(mc)
    turns = [_seed(mc, chat_id, offset_ms=-100 * (3 - i)) for i in range(3)]
    expected = [mid for turn in turns for mid in turn.message_ids]

    first = _list_messages(mc, chat_id, limit=4)

    assert first["page_info"]["limit"] == 4
    assert _ids(first) == expected[:4]
    assert first["page_info"].get("next_cursor"), first["page_info"]

    second = _list_messages(mc, chat_id, limit=4, cursor=first["page_info"]["next_cursor"])

    assert _ids(second) == expected[4:]
    assert second["page_info"].get("next_cursor") is None
    assert second["page_info"].get("prev_cursor"), second["page_info"]
    assert not set(_ids(first)) & set(_ids(second))


def test_list_messages_limit_above_max_is_clamped(mc):
    chat_id = _new_chat(mc)

    body = _list_messages(mc, chat_id, limit=1000)

    assert body["page_info"]["limit"] == 100


@pytest.mark.parametrize(
    ("params", "reason"),
    [
        pytest.param({"$filter": "content eq 'x'"}, None, id="unknown-filter-field"),
        pytest.param({"$filter": "role eq"}, "INVALID_FILTER", id="malformed-filter"),
        pytest.param({"$orderby": "content asc"}, "INVALID_ORDERBY_FIELD", id="unknown-orderby-field"),
        pytest.param({"limit": "0"}, "INVALID_LIMIT", id="zero-limit"),
        pytest.param({"cursor": "garbage"}, "INVALID_CURSOR", id="malformed-cursor"),
        pytest.param({"$count": "true"}, "UNSUPPORTED_QUERY_PARAM", id="count"),
    ],
)
def test_list_messages_bad_odata_query_is_rejected(mc, params, reason):
    chat_id = _new_chat(mc)

    resp = mc.get(_messages_path(chat_id), "A1", params=params)

    assert_problem(
        resp, 400, category="invalid_argument", field_reason=reason, resource_type=RT_ODATA_QUERY
    )


def test_list_messages_select_is_accepted_and_ignored(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    body = _list_messages(mc, chat_id, **{"$select": "id"})

    assert _ids(body) == turn.message_ids
    assert body["items"][0]["content"] == turn.user_text


@pytest.mark.parametrize("other", ["A2", "B1"])
def test_list_messages_of_foreign_chat_is_not_found(mc, other):
    chat_id = _new_chat(mc, "A1")
    _seed(mc, chat_id)

    resp = mc.get(_messages_path(chat_id), other)

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_list_messages_of_unknown_chat_is_not_found(mc):
    resp = mc.get(_messages_path(str(uuid.uuid4())), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_list_messages_of_deleted_chat_is_not_found(mc):
    chat_id = _new_chat(mc)
    _seed(mc, chat_id)
    _delete_chat(mc, chat_id)

    resp = mc.get(_messages_path(chat_id), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_list_messages_with_non_uuid_chat_id_is_invalid_path_params(mc):
    resp = mc.get(_messages_path("not-a-uuid"), "A1")

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_list_messages_without_token_is_unauthenticated(mc):
    chat_id = _new_chat(mc)

    resp = mc.get(_messages_path(chat_id), user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")


def test_get_chat_message_count_matches_listed_messages(mc):
    chat_id = _new_chat(mc)
    _seed(mc, chat_id, offset_ms=-300)
    _seed(mc, chat_id, state="cancelled", assistant_text="partial", offset_ms=-200)
    _seed(mc, chat_id, deleted=True, offset_ms=-100)

    listed = _list_messages(mc, chat_id)
    chat = mc.get(f"{CHATS}/{chat_id}", "A1").json()

    assert len(listed["items"]) == 4
    assert chat["message_count"] == 4


# --------------------------------------------------------------------------
# Attachments embedded in messages
# --------------------------------------------------------------------------


def test_list_messages_embeds_attachment_summaries(mc):
    chat_id = _new_chat(mc)
    ident = mc.identity("A1")
    turn = _seed(mc, chat_id)
    image = seed_attachment(
        mc, chat_id, ident.tenant_id, ident.user_id, kind="image", filename="cat.png", status="ready"
    )
    document = seed_attachment(
        mc, chat_id, ident.tenant_id, ident.user_id, kind="document", filename="spec.pdf", status="ready"
    )
    unready_image = seed_attachment(
        mc, chat_id, ident.tenant_id, ident.user_id, kind="image", filename="dog.png", status="uploaded"
    )
    deleted = seed_attachment(
        mc, chat_id, ident.tenant_id, ident.user_id, kind="document", filename="gone.pdf", deleted=True
    )
    for attachment_id in (image, document, unready_image, deleted):
        link_message_attachment(mc, chat_id, ident.tenant_id, turn.user_message_id, attachment_id)

    resp = mc.get(_messages_path(chat_id), "A1")

    assert resp.status_code == 200, resp.text
    messages = _by_id(resp.json())
    assert messages[turn.assistant_message_id]["attachments"] == []
    summaries = {a["attachment_id"]: a for a in messages[turn.user_message_id]["attachments"]}
    assert set(summaries) == {image, document, unready_image}, summaries

    image_summary = summaries[image]
    assert image_summary["kind"] == "image"
    assert image_summary["filename"] == "cat.png"
    assert image_summary["status"] == "ready"
    thumbnail = image_summary["img_thumbnail"]
    assert set(thumbnail) == THUMBNAIL_FIELDS, thumbnail
    assert thumbnail["content_type"] == "image/webp"
    assert thumbnail["width"] == SAMPLE_THUMBNAIL_WIDTH
    assert thumbnail["height"] == SAMPLE_THUMBNAIL_HEIGHT
    assert base64.b64decode(thumbnail["data_base64"]) == SAMPLE_WEBP_THUMBNAIL

    document_summary = summaries[document]
    assert document_summary["kind"] == "document"
    assert document_summary["filename"] == "spec.pdf"
    assert document_summary["status"] == "ready"
    assert document_summary.get("img_thumbnail") is None

    assert summaries[unready_image]["status"] == "uploaded"
    assert summaries[unready_image].get("img_thumbnail") is None, "thumbnail only for a ready image"

    provider_ids = mc.query(
        "SELECT provider_file_id FROM attachments WHERE chat_id = ?", (db_uuid(chat_id),)
    )
    raw = resp.text
    assert "provider_file_id" not in raw
    for row in provider_ids:
        assert row["provider_file_id"] not in raw


# --------------------------------------------------------------------------
# Reactions
# --------------------------------------------------------------------------


def test_put_like_on_assistant_message(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), "A1", json={"reaction": "like"})

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert set(body) == REACTION_FIELDS, body
    assert body["message_id"] == turn.assistant_message_id
    assert body["reaction"] == "like"
    assert is_rfc3339(body["created_at"])
    rows = _reaction_rows(mc, turn.assistant_message_id)
    ident = mc.identity("A1")
    assert len(rows) == 1
    assert uuid_from_db(rows[0]["user_id"]) == ident.user_id
    assert uuid_from_db(rows[0]["tenant_id"]) == ident.tenant_id
    assert rows[0]["reaction"] == "like"


def test_put_reaction_replaces_the_previous_one(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)
    path = _reaction_path(chat_id, turn.assistant_message_id)
    assert mc.put(path, "A1", json={"reaction": "like"}).status_code == 200

    resp = mc.put(path, "A1", json={"reaction": "dislike"})

    assert resp.status_code == 200, resp.text
    assert resp.json()["reaction"] == "dislike"
    rows = _reaction_rows(mc, turn.assistant_message_id)
    assert [r["reaction"] for r in rows] == ["dislike"]
    again = mc.put(path, "A1", json={"reaction": "dislike"})
    assert again.status_code == 200, again.text
    assert len(_reaction_rows(mc, turn.assistant_message_id)) == 1


def test_my_reaction_shows_only_the_callers_reaction(mc):
    chat_id = _new_chat(mc)
    a2 = mc.identity("A2")
    first = _seed(mc, chat_id, offset_ms=-200)
    second = _seed(mc, chat_id, offset_ms=-100)
    seed_reaction(mc, first.assistant_message_id, a2.tenant_id, a2.user_id, "like")
    seed_reaction(mc, second.assistant_message_id, a2.tenant_id, a2.user_id, "like")
    assert (
        mc.put(_reaction_path(chat_id, first.assistant_message_id), "A1", json={"reaction": "dislike"})
        .status_code
        == 200
    )

    messages = _by_id(_list_messages(mc, chat_id))

    assert messages[first.assistant_message_id]["my_reaction"] == "dislike"
    assert messages[second.assistant_message_id]["my_reaction"] is None
    assert messages[first.user_message_id]["my_reaction"] is None
    assert messages[second.user_message_id]["my_reaction"] is None


def test_delete_reaction_is_idempotent(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)
    path = _reaction_path(chat_id, turn.assistant_message_id)
    assert mc.put(path, "A1", json={"reaction": "like"}).status_code == 200
    assert _by_id(_list_messages(mc, chat_id))[turn.assistant_message_id]["my_reaction"] == "like"

    resp = mc.delete(path, "A1")

    assert resp.status_code == 204, resp.text
    assert resp.content == b""
    assert _reaction_rows(mc, turn.assistant_message_id) == []
    assert _by_id(_list_messages(mc, chat_id))[turn.assistant_message_id]["my_reaction"] is None
    again = mc.delete(path, "A1")
    assert again.status_code == 204, again.text


def test_delete_reaction_only_removes_the_callers_row(mc):
    chat_id = _new_chat(mc)
    a2 = mc.identity("A2")
    turn = _seed(mc, chat_id)
    seed_reaction(mc, turn.assistant_message_id, a2.tenant_id, a2.user_id, "like")
    path = _reaction_path(chat_id, turn.assistant_message_id)
    assert mc.put(path, "A1", json={"reaction": "dislike"}).status_code == 200

    assert mc.delete(path, "A1").status_code == 204

    rows = _reaction_rows(mc, turn.assistant_message_id)
    assert [(uuid_from_db(r["user_id"]), r["reaction"]) for r in rows] == [(a2.user_id, "like")]


@pytest.mark.parametrize("value", ["love", "LIKE", ""])
def test_put_invalid_reaction_value_is_rejected(mc, value):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), "A1", json={"reaction": value})

    assert_problem(
        resp, 400, category="invalid_argument", field_reason="INVALID_REACTION", field_name="reaction"
    )
    assert _reaction_rows(mc, turn.assistant_message_id) == []


@pytest.mark.parametrize("other", ["A2", "B1"])
def test_invalid_reaction_value_is_checked_before_authorization(mc, other):
    chat_id = _new_chat(mc, "A1")
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), other, json={"reaction": "love"})

    assert_problem(resp, 400, category="invalid_argument", field_reason="INVALID_REACTION")


@pytest.mark.parametrize(
    "payload",
    [
        pytest.param({}, id="missing-reaction"),
        pytest.param({"reaction": None}, id="null-reaction"),
        pytest.param({"reaction": 1}, id="numeric-reaction"),
    ],
)
def test_put_reaction_with_schema_mismatch_is_422(mc, payload):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), "A1", json=payload)

    _assert_extractor_problem(resp, 422, "invalid_json_body")


def test_put_reaction_with_malformed_json_is_400(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(
        _reaction_path(chat_id, turn.assistant_message_id),
        "A1",
        content=b'{"reaction": ',
        headers={"Content-Type": "application/json"},
    )

    _assert_extractor_problem(resp, 400, "json_syntax_error")


def test_put_reaction_without_json_content_type_is_415(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), "A1", content=b'{"reaction": "like"}')

    _assert_extractor_problem(resp, 415, "missing_json_content_type")


@pytest.mark.parametrize("method", ["PUT", "DELETE"])
def test_reaction_on_user_message_is_failed_precondition(mc, method):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)
    path = _reaction_path(chat_id, turn.user_message_id)

    if method == "PUT":
        resp = mc.put(path, "A1", json={"reaction": "like"})
    else:
        resp = mc.delete(path, "A1")

    assert_problem(
        resp, 400, category="failed_precondition", violation_subject="reaction_target", violation_type="STATE"
    )
    assert _reaction_rows(mc, turn.user_message_id) == []


def test_reaction_on_system_message_is_failed_precondition(mc):
    chat_id = _new_chat(mc)
    ident = mc.identity("A1")
    system_message = seed_message(mc, chat_id, ident.tenant_id, role="system", content="system note")

    resp = mc.put(_reaction_path(chat_id, system_message), "A1", json={"reaction": "like"})

    assert_problem(
        resp, 400, category="failed_precondition", violation_subject="reaction_target", violation_type="STATE"
    )


@pytest.mark.parametrize("method", ["PUT", "DELETE"])
def test_reaction_on_unknown_message_is_not_found(mc, method):
    chat_id = _new_chat(mc)
    _seed(mc, chat_id)
    path = _reaction_path(chat_id, str(uuid.uuid4()))

    resp = mc.put(path, "A1", json={"reaction": "like"}) if method == "PUT" else mc.delete(path, "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_MESSAGE)


def test_reaction_on_message_of_another_chat_is_not_found(mc):
    chat_id = _new_chat(mc)
    other_chat = _new_chat(mc)
    other_turn = _seed(mc, other_chat)

    resp = mc.put(_reaction_path(chat_id, other_turn.assistant_message_id), "A1", json={"reaction": "like"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_MESSAGE)
    assert _reaction_rows(mc, other_turn.assistant_message_id) == []


def test_reaction_on_soft_deleted_message_is_not_found(mc):
    chat_id = _new_chat(mc)
    replaced = _seed(mc, chat_id, deleted=True)

    resp = mc.put(_reaction_path(chat_id, replaced.assistant_message_id), "A1", json={"reaction": "like"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_MESSAGE)


@pytest.mark.parametrize("other", ["A2", "B1"])
@pytest.mark.parametrize("method", ["PUT", "DELETE"])
def test_reaction_in_foreign_chat_is_not_found(mc, other, method):
    chat_id = _new_chat(mc, "A1")
    turn = _seed(mc, chat_id)
    path = _reaction_path(chat_id, turn.assistant_message_id)

    resp = mc.put(path, other, json={"reaction": "like"}) if method == "PUT" else mc.delete(path, other)

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)
    assert _reaction_rows(mc, turn.assistant_message_id) == []


def test_reaction_in_deleted_chat_is_not_found(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)
    _delete_chat(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), "A1", json={"reaction": "like"})

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


@pytest.mark.parametrize(
    ("chat_part", "message_part"),
    [pytest.param("not-a-uuid", None, id="chat-id"), pytest.param(None, "not-a-uuid", id="message-id")],
)
def test_reaction_with_non_uuid_path_is_invalid_path_params(mc, chat_part, message_part):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(
        _reaction_path(chat_part or chat_id, message_part or turn.assistant_message_id),
        "A1",
        json={"reaction": "like"},
    )

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_reaction_without_token_is_unauthenticated(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.put(_reaction_path(chat_id, turn.assistant_message_id), user=None, json={"reaction": "like"})

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")


# --------------------------------------------------------------------------
# Turn status
# --------------------------------------------------------------------------


def _get_turn(mc, chat_id: str, request_id: str, user: str = "A1") -> dict[str, Any]:
    resp = mc.get(_turn_path(chat_id, request_id), user)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    keys = set(body)
    assert TURN_STATUS_REQUIRED <= keys, body
    assert keys <= TURN_STATUS_FIELDS, f"unexpected TurnStatus fields {keys - TURN_STATUS_FIELDS}: {body}"
    assert "chat_id" not in body
    assert body["request_id"] == request_id
    assert is_rfc3339(body["updated_at"]), body
    for key in ("error_code", "assistant_message_id"):
        if key in body:
            assert body[key] is not None, f"{key} must be omitted, not null: {body}"
    return body


def test_turn_status_of_completed_turn_is_done(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "done"
    assert body["assistant_message_id"] == turn.assistant_message_id
    assert "error_code" not in body
    assert parse_ts_ns(body["updated_at"]) == parse_ts_ns(turn.updated_at)


def test_turn_status_of_failed_turn_is_error_with_code(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, state="failed", error_code="provider_error")

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "error"
    assert body["error_code"] == "provider_error"
    assert "assistant_message_id" not in body


def test_turn_status_reports_orphan_timeout_code(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, state="failed", error_code="orphan_timeout")

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "error"
    assert body["error_code"] == "orphan_timeout"


def test_turn_status_of_running_turn(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, state="running")

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "running"
    assert "assistant_message_id" not in body
    assert "error_code" not in body


def test_turn_status_of_cancelled_turn_with_partial_content(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, state="cancelled", assistant_text="Partial ans")

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "cancelled"
    assert body["assistant_message_id"] == turn.assistant_message_id
    assert "error_code" not in body


def test_turn_status_of_cancelled_turn_without_content(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, state="cancelled")

    body = _get_turn(mc, chat_id, turn.request_id)

    assert body["state"] == "cancelled"
    assert "assistant_message_id" not in body


def test_turn_status_does_not_expose_billing_or_provider_internals(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.get(_turn_path(chat_id, turn.request_id), "A1")

    assert resp.status_code == 200, resp.text
    raw = resp.text
    for internal in ("reserve", "credits", "policy_version", "provider", "effective_model", "resp_seed_"):
        assert internal not in raw, f"{internal!r} leaked: {raw}"


def test_turn_status_of_soft_deleted_turn_is_not_found(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id, deleted=True)

    resp = mc.get(_turn_path(chat_id, turn.request_id), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_TURN)


def test_turn_status_of_unknown_request_id_is_not_found(mc):
    chat_id = _new_chat(mc)
    _seed(mc, chat_id)

    resp = mc.get(_turn_path(chat_id, str(uuid.uuid4())), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_TURN)


def test_turn_status_of_turn_from_another_chat_is_not_found(mc):
    chat_id = _new_chat(mc)
    other_turn = _seed(mc, _new_chat(mc))

    resp = mc.get(_turn_path(chat_id, other_turn.request_id), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_TURN)


@pytest.mark.parametrize("other", ["A2", "B1"])
def test_turn_status_in_foreign_chat_is_not_found(mc, other):
    chat_id = _new_chat(mc, "A1")
    turn = _seed(mc, chat_id)

    resp = mc.get(_turn_path(chat_id, turn.request_id), other)

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_turn_status_in_deleted_chat_is_not_found(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)
    _delete_chat(mc, chat_id)

    resp = mc.get(_turn_path(chat_id, turn.request_id), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_CHAT)


def test_turn_status_with_non_uuid_request_id_is_invalid_path_params(mc):
    chat_id = _new_chat(mc)

    resp = mc.get(_turn_path(chat_id, "not-a-uuid"), "A1")

    assert_problem(resp, 400, category="invalid_argument", field_reason="invalid_path_params")


def test_turn_status_without_token_is_unauthenticated(mc):
    chat_id = _new_chat(mc)
    turn = _seed(mc, chat_id)

    resp = mc.get(_turn_path(chat_id, turn.request_id), user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")
