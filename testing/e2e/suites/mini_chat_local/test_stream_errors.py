"""``messages:stream`` black-box: preflight rejections (JSON problems, no SSE, the provider is
never called, nothing is stored) and failures after the stream opened (terminal ``error``
event, failed turn, sanitized messages).

Source: DESIGN 3.3 "Streaming Contract" (attachment validation, ``request_id``), the REST error
table (``EMPTY_CONTENT``, ``INVALID_MODEL``, ``invalid_attachment``, ``INPUT_TOO_LONG``,
``CONTEXT_BUDGET_EXCEEDED``, ``FEATURE_DISABLED``, 404 / 415 / 422 / 400 extractor rows),
"Provider Event Translation", "Streaming error codes", "Provider identifier non-exposure
invariant", 5.7 "Terminal SSE Event Emission Guard" and the Turn Status API.
"""

from __future__ import annotations

import re
import uuid

import pytest

from . import catalog
from .mock_openai_provider import (
    ProviderResponse,
    error_event_stream,
    error_response,
    failed_stream_events,
    file_annotation,
    json_response,
    sse_text,
)
from .support import (
    RESOURCE_TYPES,
    TENANT_A_ID,
    TENANT_B_ID,
    USER_A_ID,
    USER_A_REVIEWER_ID,
    USER_B_ID,
    assert_field_violation,
    assert_precondition,
    assert_problem,
    field_violation_reasons,
    get_turn,
    open_stream,
    read_all,
    reject_message,
    send_message,
)

PREMIUM = catalog.PREMIUM_ID


def new_chat(seed, model=PREMIUM, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)


def chat_requests(mock):
    return mock.requests_to("POST", "/v1/responses")


def assert_untouched(db, mock, chat, before):
    assert not chat_requests(mock), "the provider must not be called"
    assert db.counts(chat) == before, "a rejected request leaves no rows behind"


# --------------------------------------------------------------------------- #
# preflight: JSON problem, provider untouched, nothing stored
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("content", ["", " ", "   \n\t  \r\n"])
def test_empty_or_whitespace_content_is_400_empty_content(api, seed, mock_provider, content):
    chat = new_chat(seed)
    before = seed.counts(chat)
    resp = reject_message(api, chat, {"content": content})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "content", "EMPTY_CONTENT")
    assert_untouched(seed, mock_provider, chat, before)


@pytest.mark.parametrize("payload", [
    {},
    {"content": 5},
    {"content": None},
    {"content": "x", "request_id": "not-a-uuid"},
    {"content": "x", "attachment_ids": ["not-a-uuid"]},
    {"content": "x", "attachment_ids": "nope"},
    {"content": "x", "web_search": {"enabled": "yes"}},
    {"content": "x", "web_search": {}},
    [],
])
def test_a_body_that_does_not_match_the_schema_is_422(api, seed, mock_provider, payload):
    chat = new_chat(seed)
    before = seed.counts(chat)
    resp = reject_message(api, chat, payload)
    problem = assert_problem(resp, 422, "invalid_argument")
    assert_field_violation(problem, "body", "invalid_json_body")
    assert_untouched(seed, mock_provider, chat, before)


def test_malformed_json_is_400_json_syntax_error(api, seed, mock_provider):
    chat = new_chat(seed)
    resp = reject_message(api, chat, None, content='{"content": ',
                          headers={"Content-Type": "application/json"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "body", "json_syntax_error")
    assert not chat_requests(mock_provider)


@pytest.mark.parametrize("headers", [{}, {"Content-Type": "text/plain"}])
def test_a_body_without_a_json_content_type_is_415(api, seed, mock_provider, headers):
    chat = new_chat(seed)
    resp = reject_message(api, chat, None, content='{"content": "x"}', headers=headers)
    problem = assert_problem(resp, 415, "invalid_argument")
    assert_field_violation(problem, "body", "missing_json_content_type")
    assert not chat_requests(mock_provider)


def test_a_missing_token_is_401(api, seed, mock_provider):
    chat = new_chat(seed)
    live = open_stream(api, chat, {"content": "x"}, user=None)
    assert live.status_code == 401
    live.read()
    assert not chat_requests(mock_provider)


def test_an_unknown_foreign_or_deleted_chat_is_404(api, seed, mock_provider):
    other_user = seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_REVIEWER_ID, model=PREMIUM)
    other_tenant = seed.insert_chat(tenant=TENANT_B_ID, user=USER_B_ID, model=PREMIUM)
    deleted = new_chat(seed, deleted=True)
    for chat in (str(uuid.uuid4()), other_user, other_tenant, deleted):
        resp = reject_message(api, chat, {"content": "hello"})
        problem = assert_problem(resp, 404, "not_found")
        assert problem["context"]["resource_type"] == RESOURCE_TYPES["chat"]
    assert not chat_requests(mock_provider)
    assert seed.ordered_messages(other_user) == []


def test_a_non_uuid_chat_id_is_400_invalid_path_params(api, seed, mock_provider):
    resp = reject_message(api, "x", {"content": "hello"}, path="/mini-chat/v1/chats/not-a-uuid/messages:stream")
    problem = assert_problem(resp, 400, "invalid_argument")
    assert "invalid_path_params" in field_violation_reasons(problem)


def test_a_chat_model_that_left_the_catalog_is_400_invalid_model(api, seed, mock_provider):
    chat = new_chat(seed, model="model-that-left-the-catalog")
    before = seed.counts(chat)
    resp = reject_message(api, chat, {"content": "hello"})
    problem = assert_problem(resp, 400, "invalid_argument")
    assert_field_violation(problem, "model", "INVALID_MODEL")
    assert_untouched(seed, mock_provider, chat, before)


def test_invalid_duplicate_foreign_or_not_ready_attachments_are_400_invalid_attachment(
        api, seed, mock_provider):
    chat = new_chat(seed)
    other = new_chat(seed)
    ready = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID, filename="r.pdf")
    foreign = seed.insert_attachment(tenant=TENANT_A_ID, chat=other, user=USER_A_ID,
                                     filename="f.pdf")
    pending = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                     filename="p.pdf", status="pending")
    uploaded = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                      filename="u.pdf", status="uploaded")
    failed = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                    filename="x.pdf", status="failed")
    deleted = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                     filename="d.pdf", deleted=True)
    cases = {
        "unknown": [str(uuid.uuid4())],
        "duplicate": [ready, ready],
        "another chat": [foreign],
        "pending": [pending],
        "uploaded": [uploaded],
        "failed": [failed],
        "soft-deleted": [deleted],
        "one bad among good": [ready, str(uuid.uuid4())],
    }
    before = seed.counts(chat)
    for name, ids in cases.items():
        resp = reject_message(api, chat, {"content": "with files", "attachment_ids": ids})
        problem = assert_problem(resp, 400, "invalid_argument")
        assert_field_violation(problem, "attachment", "invalid_attachment")
        assert_untouched(seed, mock_provider, chat, before)


# --- budget rules and the kill switch need the restricted stack ------------- #


def test_an_oversized_message_is_400_input_too_long(restricted):
    api = restricted.api()
    try:
        seed = restricted.seeder()
        chat = new_chat(seed, model=catalog.TINY_INPUT_ID)
        before = seed.counts(chat)
        resp = reject_message(api, chat, {"content": "x" * 4000})
        problem = assert_problem(resp, 400, "out_of_range")
        assert "INPUT_TOO_LONG" in field_violation_reasons(problem)
        assert_untouched(seed, restricted.mock, chat, before)
        # below the limit the same model works: the input budget is `max_input_tokens` minus the
        # fixed overhead (DESIGN 2.2) = 300 tokens; the system prompt (120) and a 200-byte
        # message (165) fit it
        assert send_message(api, chat, "y" * 200).terminal.event == "done"
    finally:
        api.close()


@pytest.mark.parametrize("model", [catalog.TINY_WINDOW_ID, catalog.BIG_SYSTEM_ID])
def test_mandatory_context_that_does_not_fit_is_400_context_budget_exceeded(restricted, model):
    api = restricted.api()
    try:
        seed = restricted.seeder()
        chat = new_chat(seed, model=model)
        before = seed.counts(chat)
        resp = reject_message(api, chat, {"content": "hi"})
        problem = assert_problem(resp, 400, "out_of_range")
        assert "CONTEXT_BUDGET_EXCEEDED" in field_violation_reasons(problem)
        assert_untouched(seed, restricted.mock, chat, before)
    finally:
        api.close()


def test_the_web_search_kill_switch_rejects_a_request_that_enables_web_search(restricted):
    api = restricted.api()
    try:
        seed = restricted.seeder()
        chat = new_chat(seed)
        before = seed.counts(chat)
        resp = reject_message(api, chat, {"content": "search", "web_search": {"enabled": True}})
        problem = assert_problem(resp, 400, "failed_precondition")
        assert_precondition(problem, "web_search", "FEATURE_DISABLED")
        assert_untouched(seed, restricted.mock, chat, before)

        for body in ({"content": "plain"}, {"content": "off", "web_search": {"enabled": False}}):
            assert read_all(open_stream(api, chat, body)).terminal.event == "done", body
        assert all(r.tool("web_search") is None for r in chat_requests(restricted.mock))
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# failures after the stream opened
# --------------------------------------------------------------------------- #

LEAKY = ("Request resp_abcdef123456 failed: file-abcdef123456 and vs_abcdef123456 are broken, "
         "see https://platform.openai.com/docs/errors for details; key sk-abcdefghij1234567 was "
         "rejected, header Bearer eyJhbGciOi.abc.def")
LEAK_MARKERS = ["resp_abcdef123456", "file-abcdef123456", "vs_abcdef123456",
                "platform.openai.com", "sk-abcdefghij", "eyJhbGciOi"]
IDENTIFIER_PATTERN = re.compile(
    r"(resp_[A-Za-z0-9]{6,}|chatcmpl-[A-Za-z0-9]{6,}|file[-_][A-Za-z0-9]{12,}|"
    r"vs_[A-Za-z0-9]{12,}|sk-[A-Za-z0-9]{10,}|Bearer\s)")


def assert_sanitized(text):
    for raw in LEAK_MARKERS:
        assert raw not in text, f"`{raw}` leaked: {text}"
    assert not IDENTIFIER_PATTERN.search(text), f"identifier pattern leaked: {text}"


def failing_turn(api, chat, mock, response):
    mock.script("POST", "/v1/responses", response)
    sent = read_all(open_stream(api, chat, {"content": "please fail",
                                            "request_id": str(uuid.uuid4())}))
    sent.assert_well_formed()
    return sent


def assert_error_event(sent, code):
    assert sent.terminal.event == "error", sent.names()
    assert sent.error["code"] == code, sent.error
    assert isinstance(sent.error["message"], str) and sent.error["message"]
    assert set(sent.error) == {"code", "message"}
    assert not sent.of("done")


def assert_failed_turn(api, seed, chat, sent, code):
    turn = seed.turn(chat, sent.request_id)
    assert turn["state"] == "failed"
    assert turn["error_code"] == code
    assert turn["completed_at"] is not None
    assert turn["assistant_message_id"] is None
    assert [r["role"] for r in seed.ordered_messages(chat)] == ["user"], \
        "the user message stays, no assistant message is written"
    status = get_turn(api, chat, sent.request_id)
    assert status.status_code == 200, status.text
    assert status.json()["state"] == "error" and status.json()["error_code"] == code
    assert "assistant_message_id" not in status.json()


@pytest.mark.parametrize("status", [400, 500, 502, 503])
def test_a_provider_error_status_ends_the_stream_with_provider_error(api, seed, mock_provider,
                                                                     status):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, error_response(status, "upstream said no"))
    assert sent.names_no_ping() == ["stream_started", "error"]
    assert_error_event(sent, "provider_error")
    assert_failed_turn(api, seed, chat, sent, "provider_error")
    assert len(chat_requests(mock_provider)) == 1, "a stream is never retried"


GATEWAY_INTERNALS = ["{", "gts", "gts://", "/v1/responses", "instance", "retry_after",
                     "127.0.0.1", "localhost"]


def test_a_gateway_failure_never_leaks_the_problem_document(api, seed, mock_provider):
    """The provider is unreachable: OAGW answers with its own RFC 9457 problem naming the
    upstream host / path / type URIs; the client only sees a fixed generic text."""
    chat = new_chat(seed)
    port = str(mock_provider.port)
    with mock_provider.unavailable():
        sent = read_all(open_stream(api, chat, {"content": "please fail",
                                                "request_id": str(uuid.uuid4())}))
    sent.assert_well_formed()
    assert_error_event(sent, "provider_error")
    message = sent.error["message"]
    assert message == "The provider is currently unavailable", message
    for raw in GATEWAY_INTERNALS + [port]:
        assert raw not in message, f"`{raw}` leaked: {message}"
    assert_failed_turn(api, seed, chat, sent, "provider_error")
    for text in (get_turn(api, chat, sent.request_id).text,
                 api.get(f"/v1/chats/{chat}/messages").text):
        for raw in ("gts://", port, "/v1/responses"):
            assert raw not in text, f"`{raw}` leaked: {text}"


@pytest.mark.parametrize("body", [
    {"type": "gts://internal~x~", "instance": "/host/v1/responses", "status": 500},
    {"error": {"type": "server_error", "code": "internal-code"}},
    {"error": {"message": ""}, "detail": "raw detail body"},
])
def test_a_provider_body_without_a_message_is_never_forwarded_raw(api, seed, mock_provider,
                                                                  body):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, json_response(body, status=500))
    assert_error_event(sent, "provider_error")
    assert sent.error["message"] == "The provider returned HTTP 500", sent.error
    assert_failed_turn(api, seed, chat, sent, "provider_error")


def test_a_provider_429_is_rate_limited(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider,
                        error_response(429, "slow down", headers={"Retry-After": "37"}))
    assert sent.names_no_ping() == ["stream_started", "error"]
    assert_error_event(sent, "rate_limited")
    assert_failed_turn(api, seed, chat, sent, "rate_limited")
    assert len(chat_requests(mock_provider)) == 1


def test_a_dropped_provider_connection_is_a_provider_error(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, ProviderResponse(drop_connection=True))
    assert_error_event(sent, "provider_error")
    assert_failed_turn(api, seed, chat, sent, "provider_error")


def test_response_failed_is_a_provider_error_and_partial_text_is_not_an_answer(api, seed,
                                                                              mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, ProviderResponse(
        sse_events=failed_stream_events("server_error", "The model failed",
                                        partial_chunks=["par", "tial"])))
    assert sent.names_no_ping() == ["stream_started", "delta", "delta", "error"]
    assert_error_event(sent, "provider_error")
    assert "The model failed" in sent.error["message"]
    assert_failed_turn(api, seed, chat, sent, "provider_error")


def test_response_failed_with_usage_is_still_a_failed_turn(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, ProviderResponse(
        sse_events=failed_stream_events("server_error", "overloaded", usage=(30, 4))))
    assert_error_event(sent, "provider_error")
    assert '"usage"' not in sent.blob()
    assert_failed_turn(api, seed, chat, sent, "provider_error")


@pytest.mark.parametrize("name,data", [
    ("nested", {"type": "error", "error": {"code": "x", "message": "nested boom"}}),
    ("flat", {"type": "error", "code": "x", "message": "flat boom"}),
    ("unparseable", "plain text boom"),
])
def test_an_error_event_is_a_provider_error_in_every_shape(api, seed, mock_provider, name, data):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider,
                        ProviderResponse(sse_events=error_event_stream(data)))
    assert_error_event(sent, "provider_error")
    assert "boom" in sent.error["message"], name
    assert_failed_turn(api, seed, chat, sent, "provider_error")


def test_a_stream_without_a_terminal_event_is_a_provider_error(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider,
                        sse_text(["half", " an answer"], terminal=None))
    assert sent.names_no_ping() == ["stream_started", "delta", "delta", "error"]
    assert_error_event(sent, "provider_error")
    assert_failed_turn(api, seed, chat, sent, "provider_error")


@pytest.mark.parametrize("name,response", [
    ("http error body", error_response(500, LEAKY)),
    ("response.failed", ProviderResponse(sse_events=failed_stream_events("server_error", LEAKY))),
    ("error event", ProviderResponse(sse_events=error_event_stream(
        {"type": "error", "message": LEAKY}))),
    ("unparseable error event", ProviderResponse(sse_events=error_event_stream(LEAKY))),
])
def test_provider_messages_are_sanitized(api, seed, mock_provider, name, response):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, response)
    message = sent.error["message"]
    assert_sanitized(message)
    assert_sanitized(sent.blob())
    assert "[provider_id]" in message and "[url]" in message and "[credential]" in message, \
        f"{name}: {message}"
    assert_sanitized(get_turn(api, chat, sent.request_id).text)
    assert_sanitized(api.get(f"/v1/chats/{chat}/messages").text)


def test_a_429_message_never_carries_provider_identifiers(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, error_response(429, LEAKY))
    assert_error_event(sent, "rate_limited")
    assert_sanitized(sent.blob())


def test_text_around_the_identifiers_survives_sanitization(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = failing_turn(api, chat, mock_provider, error_response(
        500, "The server had an error while processing resp_abcdef123456. Try later."))
    message = sent.error["message"]
    assert "The server had an error while processing" in message
    assert "Try later" in message and "[provider_id]" in message


def test_provider_ids_of_a_successful_turn_never_reach_the_client(api, seed, mock_provider):
    chat = new_chat(seed)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="contract.pdf")
    seed.insert_vector_store(tenant=TENANT_A_ID, chat=chat, vector_store_id="vs_e2e_secret_0099")
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["answer"], response_id="resp_secretrunid000001",
        annotations=[file_annotation(f"file-{doc}", "provider-side-name.pdf")]))
    sent = send_message(api, chat, "question")
    assert sent.terminal.event == "done"
    for text in (sent.blob(), api.get(f"/v1/chats/{chat}/messages").text,
                 get_turn(api, chat, sent.request_id).text, api.get(f"/v1/chats/{chat}").text):
        for raw in ("resp_secretrunid000001", f"file-{doc}", "vs_e2e_secret_0099",
                    catalog.SECRET_PROVIDER_MODEL_PREFIX):
            assert raw not in text, f"`{raw}` leaked: {text}"


def test_the_terminal_event_is_sent_only_after_the_turn_is_finalized(api, seed, mock_provider):
    chat = new_chat(seed)
    live = open_stream(api, chat, {"content": "go", "request_id": str(uuid.uuid4())})
    try:
        rid = live.expect("stream_started").data["request_id"]
        while live.next_event().event != "done":
            pass
        assert seed.turn(chat, rid)["state"] == "completed", "done follows the commit"
        assert len(seed.ordered_messages(chat)) == 2
    finally:
        live.close()

    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", error_response(500, "nope"))
    live = open_stream(api, chat, {"content": "go", "request_id": str(uuid.uuid4())})
    try:
        rid = live.expect("stream_started").data["request_id"]
        live.expect("error")
        turn = seed.turn(chat, rid)
        assert turn["state"] == "failed" and turn["error_code"] == "provider_error"
    finally:
        live.close()


def test_a_failed_turn_releases_the_chat(api, seed, mock_provider):
    chat = new_chat(seed)
    first = failing_turn(api, chat, mock_provider, error_response(500, "nope"))
    assert first.terminal.event == "error"
    second = send_message(api, chat, "try again")
    second.assert_well_formed()
    assert second.terminal.event == "done"
    assert first.request_id != second.request_id
