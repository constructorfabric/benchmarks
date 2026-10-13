"""Turn lifecycle black-box: ``GET /v1/chats/{id}/turns/{request_id}`` and what a client
disconnect does to a running turn.

Source: DESIGN 3.3 "Turn Status API" (fields, omitted-when-null rules, internal-to-API state
mapping, 404 for a soft-deleted turn or another user's chat), 3.6 "Streaming Cancellation",
3.3 "SSE stream close rules", 4 "Turn Lifecycle" (state model, Non-Goals: the partial answer is a
one-time snapshot that takes part in later context), 5.7 "Terminal outcome taxonomy", the REST
error table and the OpenAPI schemas ``TurnStatusResponse`` / ``TurnStatusState``.
"""

from __future__ import annotations

import uuid

import pytest

from . import catalog
from .mock_openai_provider import error_response, sse_text
from .support import (
    RESOURCE_TYPES,
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_B_ID,
    assert_conflict,
    assert_problem,
    field_violation_reasons,
    get_turn,
    open_stream,
    parse_ts,
    reject_message,
    send_message,
    uuid_text,
    wait_for,
    wire_ts,
)

PREMIUM = catalog.PREMIUM_ID
STATUS_KEYS = {"request_id", "state", "error_code", "assistant_message_id", "updated_at"}


def new_chat(seed, model=PREMIUM, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)


def chat_requests(mock):
    return mock.requests_to("POST", "/v1/responses")


def wait_state(seed, chat, rid, state):
    return wait_for(
        lambda: (lambda t: t if t is not None and t["state"] == state else None)(
            seed.turn(chat, rid)),
        f"turn {rid} to be {state}")


# --------------------------------------------------------------------------- #
# the status of real turns
# --------------------------------------------------------------------------- #


def test_a_running_turn_reports_running_without_error_code_or_message_id(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["a", "b"], pause_after_deltas=1))
    live = open_stream(api, chat, {"content": "q", "request_id": rid})
    try:
        live.expect("stream_started")
        live.next_non_ping()
        mock_provider.wait_paused()
        resp = get_turn(api, chat, rid)
        assert resp.status_code == 200, resp.text
        assert resp.headers["content-type"].startswith("application/json")
        body = resp.json()
        assert body["request_id"] == rid and body["state"] == "running"
        assert set(body) == {"request_id", "state", "updated_at"}
        parse_ts(body["updated_at"])
        mock_provider.release()
        live.drain()
    finally:
        live.close()


def test_a_completed_turn_is_done_and_names_the_assistant_message(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = send_message(api, chat, "q")
    body = get_turn(api, chat, sent.request_id).json()
    assert body["state"] == "done"
    assert body["assistant_message_id"] == sent.message_id
    assert set(body) == {"request_id", "state", "assistant_message_id", "updated_at"}
    found = api.get(f"/v1/chats/{chat}/messages?$filter=id eq {sent.message_id}").json()
    assert [i["id"] for i in found["items"]] == [sent.message_id]


def test_a_failed_turn_is_error_with_its_error_code_and_no_message_id(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", error_response(429, "slow down"))
    sent = send_message(api, chat, "q")
    body = get_turn(api, chat, sent.request_id).json()
    assert body["state"] == "error" and body["error_code"] == "rate_limited"
    assert set(body) == {"request_id", "state", "error_code", "updated_at"}


@pytest.mark.parametrize("state,api_state,error_code,with_message", [
    ("running", "running", None, False),
    ("completed", "done", None, True),
    ("failed", "error", "orphan_timeout", False),
    ("failed", "error", "provider_timeout", False),
    ("cancelled", "cancelled", None, False),
    ("cancelled", "cancelled", None, True),
])
def test_every_internal_state_maps_to_its_api_state(api, seed, mock_provider, state, api_state,
                                                    error_code, with_message):
    chat = new_chat(seed)
    assistant = seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant",
                                    content="partial", created=3)
    rid = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=rid, state=state,
                     user=USER_A_ID, error_code=error_code,
                     assistant_message=assistant if with_message else None)
    resp = get_turn(api, chat, rid)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["state"] == api_state and body["request_id"] == rid
    assert set(body) <= STATUS_KEYS
    if error_code:
        assert body["error_code"] == error_code
    else:
        assert "error_code" not in body
    if with_message:
        assert body["assistant_message_id"] == assistant
    else:
        assert "assistant_message_id" not in body
    assert parse_ts(body["updated_at"]) == parse_ts(wire_ts(12, 345_678))


def test_an_unknown_request_id_is_404_for_the_turn_resource(api, seed, mock_provider):
    chat = new_chat(seed)
    problem = assert_problem(get_turn(api, chat, str(uuid.uuid4())), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]


def test_a_soft_deleted_turn_is_404(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=rid, state="completed",
                     deleted=True)
    problem = assert_problem(get_turn(api, chat, rid), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]


def test_the_turn_of_a_foreign_unknown_or_deleted_chat_is_404(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_A_ID, chat=chat, request_id=rid, state="completed")
    for user in ("a_reviewer", "b"):
        resp = get_turn(api, chat, rid, user=user)
        assert_problem(resp, 404, "not_found")
        assert "completed" not in resp.text
    problem = assert_problem(get_turn(api, str(uuid.uuid4()), rid), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]

    deleted = new_chat(seed, deleted=True)
    rid_deleted = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_A_ID, chat=deleted, request_id=rid_deleted, state="completed")
    assert_problem(get_turn(api, deleted, rid_deleted), 404, "not_found")

    other = new_chat(seed)
    problem = assert_problem(get_turn(api, other, rid), 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["turn"]

    b_chat = seed.insert_chat(tenant=TENANT_B_ID, user=USER_B_ID, model=PREMIUM)
    b_rid = str(uuid.uuid4())
    seed.insert_turn(tenant=TENANT_B_ID, chat=b_chat, request_id=b_rid, state="completed",
                     user=USER_B_ID)
    assert_problem(get_turn(api, b_chat, b_rid), 404, "not_found")


def test_non_uuid_path_parameters_are_400_invalid_path_params(api, seed, mock_provider):
    chat = new_chat(seed)
    for path in (f"/v1/chats/{chat}/turns/not-a-uuid",
                 f"/v1/chats/not-a-uuid/turns/{uuid.uuid4()}"):
        problem = assert_problem(api.get(path), 400, "invalid_argument")
        assert "invalid_path_params" in field_violation_reasons(problem)


def test_a_missing_token_is_401(api, seed, mock_provider):
    chat = new_chat(seed)
    resp = api.get(f"/v1/chats/{chat}/turns/{uuid.uuid4()}", user=None)
    assert resp.status_code == 401


def test_reading_the_status_changes_nothing_and_never_calls_the_provider(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = send_message(api, chat, "q")
    calls = len(chat_requests(mock_provider))
    counts = seed.counts(chat)
    turn = dict(seed.turn(chat, sent.request_id))
    chat_row = dict(seed.chat(chat))
    for _ in range(3):
        get_turn(api, chat, sent.request_id)
    assert len(chat_requests(mock_provider)) == calls
    assert seed.counts(chat) == counts
    assert dict(seed.turn(chat, sent.request_id)) == turn
    assert dict(seed.chat(chat)) == chat_row


# --------------------------------------------------------------------------- #
# client disconnect
# --------------------------------------------------------------------------- #


def test_a_disconnect_mid_stream_cancels_the_turn_and_persists_the_partial_answer(
        api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["Hel", "lo", " world"], pause_after_deltas=2))
    live = open_stream(api, chat, {"content": "greet me", "request_id": rid})
    started = live.expect("stream_started").data
    live.next_non_ping()
    live.next_non_ping()
    mock_provider.wait_paused()
    live.close()  # the client goes away

    turn = wait_state(seed, chat, rid, "cancelled")
    mock_provider.wait_client_disconnected()  # the provider request was cancelled
    assert turn["completed_at"] is not None
    assert turn["error_code"] is None
    assert uuid_text(turn["assistant_message_id"]) == started["message_id"]

    user, assistant = seed.ordered_messages(chat)
    assert uuid_text(assistant["id"]) == started["message_id"]
    assert assistant["role"] == "assistant" and assistant["content"] == "Hello"
    assert uuid_text(assistant["request_id"]) == rid
    assert assistant["model"] == PREMIUM

    body = get_turn(api, chat, rid).json()
    assert body["state"] == "cancelled"
    assert body["assistant_message_id"] == started["message_id"]
    assert "error_code" not in body
    items = api.get(f"/v1/chats/{chat}/messages").json()["items"]
    assert items[1]["content"] == "Hello"


def test_a_disconnect_before_any_content_cancels_without_an_assistant_message(api, seed,
                                                                             mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["never seen"], pause_after_deltas=0))
    live = open_stream(api, chat, {"content": "q", "request_id": rid})
    live.expect("stream_started")
    mock_provider.wait_paused()
    live.close()

    turn = wait_state(seed, chat, rid, "cancelled")
    mock_provider.wait_client_disconnected()
    assert turn["assistant_message_id"] is None
    assert [r["role"] for r in seed.ordered_messages(chat)] == ["user"]
    body = get_turn(api, chat, rid).json()
    assert body["state"] == "cancelled" and "assistant_message_id" not in body


def test_a_disconnect_after_the_terminal_event_changes_nothing(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    live = open_stream(api, chat, {"content": "q", "request_id": rid})
    assert live.drain()[-1].event == "done"
    live.close()
    turn = seed.turn(chat, rid)
    assert turn["state"] == "completed" and turn["assistant_message_id"] is not None
    assert len(seed.ordered_messages(chat)) == 2


def test_a_cancelled_turn_frees_the_chat_but_keeps_its_request_id_taken(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["x"], pause_after_deltas=0))
    live = open_stream(api, chat, {"content": "q", "request_id": rid})
    live.expect("stream_started")
    mock_provider.wait_paused()
    live.close()
    wait_state(seed, chat, rid, "cancelled")

    assert_conflict(reject_message(api, chat, {"content": "q", "request_id": rid}),
                    "request_id_conflict")
    assert send_message(api, chat, "a fresh message").terminal.event == "done"


def test_the_partial_answer_of_a_cancelled_turn_is_part_of_the_next_context(api, seed,
                                                                           mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(["Par", "tial"], pause_after_deltas=2))
    live = open_stream(api, chat, {"content": "first", "request_id": rid})
    live.expect("stream_started")
    live.next_non_ping()
    live.next_non_ping()
    mock_provider.wait_paused()
    live.close()
    wait_state(seed, chat, rid, "cancelled")

    send_message(api, chat, "second")
    assert chat_requests(mock_provider)[-1].conversation_texts() == [
        "user:first", "assistant:Partial", "user:second"]
