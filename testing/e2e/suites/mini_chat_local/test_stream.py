"""``POST /v1/chats/{id}/messages:stream`` black-box: the SSE contract of a normal turn,
persistence, the request the gear sends to the provider through OAGW, tool events and
citations.

The server runs against the mock provider of ``conftest.py`` (an OpenAI Responses API
look-alike behind a provisioned OAGW upstream); every provider request is recorded.

Source: DESIGN 3.3 "Streaming Contract" / "SSE Event Definitions" / "SSE Event Ordering" /
"Provider Event Translation", 3.2 "OAGW provisioning", 2.2 "No Buffering", 4 "Provider Request
Metadata" / "Web Search Configuration" / "File Search Tool Availability" / "Citation File ID and
Title Resolution", 3.7 ``messages`` / ``chat_turns`` and the OpenAPI schemas
``StreamMessageRequest`` / ``StreamStartedData`` / ``DeltaData`` / ``ToolData`` /
``CitationsData`` / ``DoneData``.
"""

from __future__ import annotations

import time
import uuid

import pytest

from . import catalog
from .mock_openai_provider import (
    code_interpreter_events,
    file_annotation,
    file_search_events,
    nameless_frames,
    noise_events,
    sse_frames,
    sse_text,
    text_stream_events,
    web_annotation,
    web_search_events,
)
from .support import (
    OPENAI_SECRET_VALUE,
    TENANT_A_ID,
    USER_A_ID,
    open_stream,
    parse_ts,
    read_all,
    send_message,
    stored_ts,
    uuid_blob,
    uuid_text,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
WEB_GUARD = ("Use web_search only if the answer cannot be obtained from the provided context "
             "or your training data. Never use it for general knowledge questions. At most one "
             "web_search call per request.")


def new_chat(seed, model=PREMIUM, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)


def chat_requests(mock):
    return mock.requests_to("POST", "/v1/responses")


def last_request(mock):
    requests = chat_requests(mock)
    assert requests, "the provider was never called"
    return requests[-1]


# --------------------------------------------------------------------------- #
# the SSE contract of a normal turn
# --------------------------------------------------------------------------- #


def test_send_answers_event_stream_and_starts_with_stream_started(api, seed, mock_provider):
    chat = new_chat(seed)
    rid = str(uuid.uuid4())
    live = open_stream(api, chat, {"content": "Hi", "request_id": rid})
    assert live.status_code == 200
    assert live.headers["content-type"].startswith("text/event-stream"), live.headers
    sent = read_all(live)
    sent.assert_well_formed()
    assert sent.names_no_ping() == ["stream_started", "delta", "delta", "delta", "delta", "delta",
                                    "done"]
    started = sent.started
    assert started["request_id"] == rid
    uuid.UUID(started["message_id"])
    assert started["is_new_turn"] is True
    assert set(started) <= {"request_id", "message_id", "is_new_turn", "thread_summary_applied"}
    assert not started.get("thread_summary_applied")


def test_server_generates_a_v4_request_id_when_omitted(api, seed, mock_provider):
    chat = new_chat(seed)
    first = send_message(api, chat, "no id")
    assert uuid.UUID(first.request_id).version == 4
    second = read_all(open_stream(api, chat, {"content": "null id", "request_id": None}))
    assert uuid.UUID(second.request_id).version == 4
    assert first.request_id != second.request_id
    assert seed.turn(chat, first.request_id) is not None


@pytest.mark.parametrize("rid", [
    "6ba7b810-9dad-11d1-80b4-00c04fd430c8",   # v1
    "01890a5d-ac96-774b-bcce-b302099a8057",   # v7
    "00000000-0000-0000-0000-000000000001",   # not a versioned UUID
])
def test_any_uuid_version_is_accepted_as_request_id(api, seed, mock_provider, rid):
    chat = new_chat(seed)
    sent = send_message(api, chat, "hello", request_id=rid)
    assert sent.request_id == rid
    sent.assert_well_formed()


def test_delta_events_carry_text_type_and_the_fragment(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["Al", "pha ", "beta"]))
    sent = send_message(api, chat)
    assert [e.data for e in sent.of("delta")] == [
        {"type": "text", "content": "Al"},
        {"type": "text", "content": "pha "},
        {"type": "text", "content": "beta"},
    ]
    assert sent.text() == "Alpha beta"


def test_the_first_delta_arrives_before_the_provider_finishes(api, seed, mock_provider):
    """No buffering: the provider holds the answer back after its first delta."""
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["first", " second"], pause_after_deltas=1))
    live = open_stream(api, chat, {"content": "go"})
    try:
        live.expect("stream_started")
        first = live.next_non_ping()
        assert first is not None and first.event == "delta"
        assert first.data["content"] == "first"
        (paused,) = mock_provider.wait_paused()
        assert paused.paused, "the provider is still mid-answer while the client has its first delta"
        mock_provider.release()
        rest = live.drain()
    finally:
        live.close()
    assert [e.event for e in rest] == ["delta", "done"]
    assert rest[0].data["content"] == " second"


def test_the_stream_ends_right_after_the_terminal_event(api, seed, mock_provider):
    chat = new_chat(seed)
    live = open_stream(api, chat, {"content": "go"})
    events = live.drain()  # returns only when the server closed the stream
    live.close()
    assert events[-1].event == "done"
    assert [e.event for e in events].count("done") == 1


def test_one_ping_is_sent_while_the_provider_is_thinking(api, seed, mock_provider):
    """`streaming.sse_ping_interval_seconds = 5` in the generated config; the provider keeps
    the answer back for 6.5 s."""
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["late answer"], delay=6.5))
    started = time.monotonic()
    sent = send_message(api, chat)
    elapsed = time.monotonic() - started
    sent.assert_well_formed()
    names = sent.names()
    assert names[0] == "stream_started"
    pings = sent.of("ping")
    assert len(pings) >= 1, names
    assert all(p.data == {} for p in pings)
    assert names.index("ping") < names.index("delta"), "pings only before the first content"
    assert elapsed >= 5.0
    assert sent.text() == "late answer"


def test_done_reports_usage_models_and_the_allow_decision(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["ok"], input_tokens=42, output_tokens=7, response_id="resp_secretprovider0001"))
    sent = send_message(api, chat)
    done = sent.done
    assert done["usage"] == {"input_tokens": 42, "output_tokens": 7}
    assert done["effective_model"] == PREMIUM
    assert done["selected_model"] == PREMIUM
    assert done["quota_decision"] == "allow"
    assert not done.get("downgrade_from") and not done.get("downgrade_reason")
    assert set(done) <= {"usage", "effective_model", "selected_model", "quota_decision",
                         "downgrade_from", "downgrade_reason", "quota_warnings"}
    assert "resp_secretprovider0001" not in sent.blob()
    assert catalog.SECRET_PROVIDER_MODEL_PREFIX not in sent.blob()


def test_a_standard_chat_reports_the_standard_model(api, seed, mock_provider):
    chat = new_chat(seed, model=STANDARD)
    sent = send_message(api, chat)
    assert sent.done["selected_model"] == STANDARD
    assert sent.done["effective_model"] == STANDARD
    assert last_request(mock_provider).json["model"] == catalog.SECRET_PROVIDER_MODEL_PREFIX + STANDARD


def test_an_incomplete_answer_is_a_completed_turn_without_citations(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["trunc", "ated"], terminal="response.incomplete", input_tokens=9, output_tokens=2,
        annotations=[web_annotation("https://e.example/", "t", 0, 3)]))
    sent = send_message(api, chat)
    sent.assert_well_formed()
    assert sent.terminal.event == "done"
    assert sent.done["usage"] == {"input_tokens": 9, "output_tokens": 2}
    assert not sent.of("citations")
    turn = seed.turn(chat, sent.request_id)
    assert turn["state"] == "completed"
    assert turn["error_code"] is None


def test_an_answer_without_text_is_a_completed_turn_with_an_empty_message(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text([]))
    sent = send_message(api, chat, "silent please")
    assert sent.names_no_ping() == ["stream_started", "done"]
    rows = seed.ordered_messages(chat)
    assert [r["role"] for r in rows] == ["user", "assistant"]
    assert rows[1]["content"] == ""
    assert seed.turn(chat, sent.request_id)["state"] == "completed"


# --------------------------------------------------------------------------- #
# persistence
# --------------------------------------------------------------------------- #


def test_a_completed_turn_persists_both_messages_and_the_turn(api, seed, mock_provider):
    chat = new_chat(seed, created=0)
    rid = str(uuid.uuid4())
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["Hello", " world"], input_tokens=42, output_tokens=7,
        response_id="resp_persisted000001"))
    sent = send_message(api, chat, "Hello there", request_id=rid)

    user, assistant = seed.ordered_messages(chat)
    assert user["role"] == "user" and user["content"] == "Hello there"
    assert uuid_text(user["request_id"]) == rid
    assert user["model"] is None
    assert uuid_text(user["tenant_id"]) == TENANT_A_ID

    assert assistant["role"] == "assistant"
    assert assistant["content"] == "Hello world"
    assert uuid_text(assistant["request_id"]) == rid
    assert uuid_text(assistant["id"]) == sent.message_id
    assert assistant["model"] == PREMIUM
    assert (assistant["input_tokens"], assistant["output_tokens"]) == (42, 7)
    assert assistant["provider_response_id"] == "resp_persisted000001"
    assert assistant["created_at"] > user["created_at"]

    turn = seed.turn(chat, rid)
    assert turn["state"] == "completed"
    assert uuid_text(turn["assistant_message_id"]) == sent.message_id
    assert turn["error_code"] is None
    assert turn["effective_model"] == PREMIUM
    assert turn["provider_response_id"] == "resp_persisted000001"
    assert turn["completed_at"] is not None
    assert turn["requester_type"] == "user"
    assert uuid_text(turn["requester_user_id"]) == USER_A_ID
    assert turn["deleted_at"] is None


def test_sending_bumps_updated_at_and_counts_two_messages_per_turn(api, seed, mock_provider):
    chat = new_chat(seed, created=0)
    assert seed.chat(chat)["updated_at"] == stored_ts(0)
    send_message(api, chat, "one")
    after = seed.chat(chat)
    assert after["updated_at"] > stored_ts(0)
    assert after["model"] == PREMIUM
    detail = api.get(f"/v1/chats/{chat}").json()
    assert detail["message_count"] == 2
    assert parse_ts(detail["updated_at"]) > parse_ts(stored_ts(0).replace("+00:00", "Z"))
    send_message(api, chat, "two")
    assert api.get(f"/v1/chats/{chat}").json()["message_count"] == 4


def test_the_message_list_shows_the_turn_in_chronological_order(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["answer"], input_tokens=11, output_tokens=3))
    sent = send_message(api, chat, "question")
    items = api.get(f"/v1/chats/{chat}/messages").json()["items"]
    assert [i["role"] for i in items] == ["user", "assistant"]
    assert [i["content"] for i in items] == ["question", "answer"]
    assert items[0]["request_id"] == items[1]["request_id"] == sent.request_id
    assert items[1]["id"] == sent.message_id
    assert items[1]["model"] == PREMIUM
    assert (items[1]["input_tokens"], items[1]["output_tokens"]) == (11, 3)
    assert "model" not in items[0]
    assert items[1]["my_reaction"] is None
    assert parse_ts(items[0]["created_at"]) < parse_ts(items[1]["created_at"])


def test_the_user_message_and_the_running_turn_are_durable_before_the_answer(api, seed,
                                                                           mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["x"], pause_after_deltas=0))
    rid = str(uuid.uuid4())
    live = open_stream(api, chat, {"content": "durable?", "request_id": rid})
    try:
        live.expect("stream_started")
        mock_provider.wait_paused()
        rows = seed.ordered_messages(chat)
        assert [r["role"] for r in rows] == ["user"]
        turn = seed.turn(chat, rid)
        assert turn["state"] == "running"
        assert turn["completed_at"] is None and turn["last_progress_at"] is not None
        mock_provider.release()
        live.drain()
    finally:
        live.close()


def test_a_valid_attachment_is_linked_to_the_user_message(api, seed, mock_provider):
    chat = new_chat(seed)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="report.pdf")
    sent = send_message(api, chat, "see the doc", attachment_ids=[doc])
    assert sent.terminal.event == "done"
    links = seed.query("SELECT * FROM message_attachments WHERE chat_id = ?", (uuid_blob(chat),))
    assert len(links) == 1 and uuid_text(links[0]["attachment_id"]) == doc
    user_msg = seed.ordered_messages(chat)[0]
    assert links[0]["message_id"] == user_msg["id"]
    items = api.get(f"/v1/chats/{chat}/messages").json()["items"]
    assert [(a["attachment_id"], a["filename"], a["kind"], a["status"])
            for a in items[0]["attachments"]] == [(doc, "report.pdf", "document", "ready")]
    assert items[1]["attachments"] == []


def test_the_model_of_the_chat_is_used_on_every_turn(api, seed, mock_provider):
    chat = new_chat(seed, model=STANDARD)
    for i in range(3):
        sent = send_message(api, chat, f"turn {i}", model=PREMIUM)  # a body `model` is ignored
        assert sent.done["selected_model"] == STANDARD
        assert sent.done["effective_model"] == STANDARD
    assert {r.json["model"] for r in chat_requests(mock_provider)} == {
        catalog.SECRET_PROVIDER_MODEL_PREFIX + STANDARD}
    assert seed.chat(chat)["model"] == STANDARD
    assert api.get(f"/v1/chats/{chat}").json()["model"] == STANDARD


# --------------------------------------------------------------------------- #
# the request sent to the provider
# --------------------------------------------------------------------------- #


def test_the_request_goes_through_oagw_with_the_injected_credentials(api, seed, mock_provider):
    chat = new_chat(seed)
    send_message(api, chat, "hello")
    request = last_request(mock_provider)
    assert request.method == "POST" and request.path == "/v1/responses"
    assert request.header("content-type", "").startswith("application/json")
    assert request.header("authorization") == f"Bearer {OPENAI_SECRET_VALUE}", \
        "OAGW injects the API key of the credstore secret (upstream provisioned at start)"
    assert len(chat_requests(mock_provider)) == 1


def test_the_body_names_the_provider_model_limits_identity_and_metadata(api, seed, mock_provider):
    chat = new_chat(seed)
    send_message(api, chat, "hello")
    body = last_request(mock_provider).json
    assert body["model"] == catalog.SECRET_PROVIDER_MODEL_PREFIX + PREMIUM
    assert body["stream"] is True
    assert body["max_output_tokens"] == 4096  # min(catalog 4096, streaming.max_output_tokens)
    assert body["max_tool_calls"] == 4
    user = body["user"]
    assert user == TENANT_A_ID.replace("-", "") + USER_A_ID.replace("-", "")
    assert len(user) == 64
    meta = body["metadata"]
    assert meta["tenant_id"].replace("-", "") == TENANT_A_ID.replace("-", "")
    assert meta["user_id"].replace("-", "") == USER_A_ID.replace("-", "")
    assert meta["chat_id"].replace("-", "") == chat.replace("-", "")
    assert meta["request_type"] == "chat" and meta["feature"] == "none"
    assert (body["temperature"], body["top_p"]) == (0.7, 1.0)
    assert (body["frequency_penalty"], body["presence_penalty"]) == (0.0, 0.0)
    assert "tools" not in body or body["tools"] == []


def test_the_system_prompt_comes_from_the_catalog_and_the_message_is_last(api, seed, mock_provider):
    chat = new_chat(seed)
    send_message(api, chat, "Current question?")
    request = last_request(mock_provider)
    assert catalog.SECRET_SYSTEM_PROMPT in request.system_text()
    assert request.conversation() == [("user", "Current question?")]


def test_history_is_sent_in_order_with_the_current_message_last(api, seed, mock_provider):
    chat = new_chat(seed)
    for i in range(2):
        rid = str(uuid.uuid4())
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", request_id=rid,
                            content=f"question {i + 1}", created=i * 2 + 1)
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", request_id=rid,
                            content=f"answer {i + 1}", created=i * 2 + 2, model=PREMIUM)
    send_message(api, chat, "Current question?")
    assert last_request(mock_provider).conversation_texts() == [
        "user:question 1", "assistant:answer 1", "user:question 2", "assistant:answer 2",
        "user:Current question?"]


def test_the_second_turn_carries_the_first(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["First", " answer"]))
    send_message(api, chat, "First question")
    send_message(api, chat, "Second question")
    assert last_request(mock_provider).conversation_texts() == [
        "user:First question", "assistant:First answer", "user:Second question"]


def test_web_search_is_sent_with_the_catalog_context_size_and_its_guard(api, seed, mock_provider):
    chat = new_chat(seed)
    sent = send_message(api, chat, "news?", web_search={"enabled": True})
    assert sent.terminal.event == "done"
    request = last_request(mock_provider)
    tool = request.tool("web_search")
    assert tool is not None, request.json
    assert tool["search_context_size"] == "low"
    assert WEB_GUARD in request.system_text()
    assert request.json["metadata"]["feature"] == "web_search"
    assert seed.turn(chat, sent.request_id)["web_search_enabled"] == 1


def test_web_search_is_not_sent_unless_requested(api, seed, mock_provider):
    chat = new_chat(seed)
    send_message(api, chat, "a", web_search={"enabled": False})
    send_message(api, chat, "b")
    for request in chat_requests(mock_provider):
        assert request.tool("web_search") is None
        assert WEB_GUARD not in request.system_text()
        assert request.json["metadata"]["feature"] == "none"


def test_web_search_is_skipped_for_a_model_without_support(api, seed, mock_provider):
    chat = new_chat(seed, model=STANDARD)  # tool_support.web_search = false
    sent = send_message(api, chat, "news?", web_search={"enabled": True})
    assert sent.terminal.event == "done"
    request = last_request(mock_provider)
    assert request.tool("web_search") is None
    assert WEB_GUARD not in request.system_text()


def _chat_with_document(seed, vector_store="vs_e2e_seeded_0001"):
    chat = new_chat(seed)
    doc = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                 filename="contract.pdf")
    seed.insert_vector_store(tenant=TENANT_A_ID, chat=chat, vector_store_id=vector_store)
    return chat, doc


def test_file_search_references_the_chat_vector_store(api, seed, mock_provider):
    chat, _doc = _chat_with_document(seed)
    sent = send_message(api, chat, "what does the contract say?")
    assert sent.terminal.event == "done"
    request = last_request(mock_provider)
    tool = request.tool("file_search")
    assert tool is not None, request.json
    assert tool["vector_store_ids"] == ["vs_e2e_seeded_0001"]
    assert tool["max_num_results"] == 5
    assert "filters" not in tool
    assert "file_search" in request.system_text()
    assert request.json["metadata"]["feature"] == "file_search"
    assert "vs_e2e_seeded_0001" not in sent.blob()


def test_file_search_is_omitted_without_a_ready_document_and_vector_store(api, seed,
                                                                         mock_provider):
    plain = new_chat(seed)
    send_message(api, plain, "a")
    only_doc = new_chat(seed)
    seed.insert_attachment(tenant=TENANT_A_ID, chat=only_doc, user=USER_A_ID, filename="a.pdf")
    send_message(api, only_doc, "b")
    pending = new_chat(seed)
    seed.insert_attachment(tenant=TENANT_A_ID, chat=pending, user=USER_A_ID, filename="p.pdf",
                           status="pending")
    seed.insert_vector_store(tenant=TENANT_A_ID, chat=pending, vector_store_id="vs_e2e_seeded_0002")
    send_message(api, pending, "c")
    for request in chat_requests(mock_provider):
        assert request.tool("file_search") is None


def test_file_and_web_search_are_reported_together(api, seed, mock_provider):
    chat, _doc = _chat_with_document(seed, "vs_e2e_seeded_0003")
    send_message(api, chat, "both", web_search={"enabled": True})
    request = last_request(mock_provider)
    assert request.json["metadata"]["feature"] == "file_search+web_search"
    assert request.tool("file_search") and request.tool("web_search")


# --------------------------------------------------------------------------- #
# tool events and event-name handling
# --------------------------------------------------------------------------- #


def test_file_search_phases_become_tool_events(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["answer"], inserts=[(0, file_search_events())]))
    sent = send_message(api, chat)
    sent.assert_well_formed()
    assert sent.names_no_ping() == ["stream_started", "tool", "tool", "delta", "done"]
    assert [e.data for e in sent.of("tool")] == [
        {"phase": "start", "name": "file_search", "details": {}},
        {"phase": "done", "name": "file_search", "details": {"files_searched": 0}},
    ]


def test_files_searched_counts_the_results_of_the_completed_event(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["answer"], inserts=[(0, file_search_events(results=3))]))
    sent = send_message(api, chat)
    assert sent.of("tool")[1].data["details"] == {"files_searched": 3}


def test_web_search_phases_become_tool_events(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["answer"], inserts=[(0, web_search_events())]))
    sent = send_message(api, chat)
    assert [e.data for e in sent.of("tool")] == [
        {"phase": "start", "name": "web_search", "details": {}},
        {"phase": "done", "name": "web_search", "details": {}},
    ]


def test_code_interpreter_reports_start_and_done_with_the_log_output(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["answer"], inserts=[(0, code_interpreter_events("42\nforty-two"))]))
    sent = send_message(api, chat)
    assert [e.data for e in sent.of("tool")] == [
        {"phase": "start", "name": "code_interpreter", "details": {}},
        {"phase": "done", "name": "code_interpreter", "details": {"output": "42\nforty-two"}},
    ]


def test_long_code_interpreter_output_is_truncated(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["answer"], inserts=[(0, code_interpreter_events("z" * 20_000))]))
    output = send_message(api, chat).of("tool")[1].data["details"]["output"]
    assert output.endswith("...[truncated]")
    assert len(output) <= 8192 + len("...[truncated]")


def test_tool_and_delta_events_interleave_in_provider_order(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["one ", "two ", "three"],
        inserts=[(1, web_search_events()), (2, file_search_events())]))
    sent = send_message(api, chat)
    sent.assert_well_formed()
    assert sent.names_no_ping() == ["stream_started", "delta", "tool", "tool", "delta", "tool",
                                    "tool", "delta", "done"]


def test_completed_tool_calls_are_counted_on_the_turn(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(["answer"], inserts=[(0, (
        file_search_events() + web_search_events() + code_interpreter_events("ok")))]))
    sent = send_message(api, chat)
    turn = seed.turn(chat, sent.request_id)
    assert turn["file_search_completed_count"] == 1
    assert turn["web_search_completed_count"] == 1
    assert turn["code_interpreter_completed_count"] == 1


def test_provider_noise_produces_no_client_event(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses",
                         sse_text(["only text"], inserts=[(0, noise_events())]))
    sent = send_message(api, chat)
    assert sent.names_no_ping() == ["stream_started", "delta", "done"]


@pytest.mark.parametrize("line", [None, "message"])
def test_the_event_name_falls_back_to_the_type_field(api, seed, mock_provider, line):
    chat = new_chat(seed)
    frames = nameless_frames(text_stream_events(["na", "meless"]), line=line)
    mock_provider.script("POST", "/v1/responses", sse_frames(*frames))
    sent = send_message(api, chat)
    sent.assert_well_formed()
    assert sent.names_no_ping() == ["stream_started", "delta", "delta", "done"]
    assert sent.text() == "nameless"


# --------------------------------------------------------------------------- #
# citations
# --------------------------------------------------------------------------- #


def test_a_web_annotation_becomes_one_citations_event_before_done(api, seed, mock_provider):
    chat = new_chat(seed)
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["Rust 1.80 was released ", "in July 2024."],
        annotations=[web_annotation("https://example.com/rust", "Rust blog", 0, 8)]))
    sent = send_message(api, chat)
    sent.assert_well_formed()
    assert sent.names_no_ping() == ["stream_started", "delta", "delta", "citations", "done"]
    assert sent.of("citations")[0].data == {"items": [{
        "source": "web", "title": "Rust blog", "url": "https://example.com/rust",
        "snippet": "Rust 1.8", "span": {"start": 0, "end": 8}}]}


def test_no_annotations_means_no_citations_event(api, seed, mock_provider):
    chat = new_chat(seed)
    assert not send_message(api, chat).of("citations")


def test_a_file_annotation_maps_to_the_attachment_and_its_filename(api, seed, mock_provider):
    chat, doc = _chat_with_document(seed, "vs_e2e_seeded_0004")
    provider_file = f"file-{doc}"  # what `GearSeeder.insert_attachment` stores
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["see the report"], annotations=[file_annotation(provider_file, "provider-name.pdf")]))
    sent = send_message(api, chat, "what is in it?")
    assert sent.of("citations")[0].data == {"items": [{
        "source": "file", "title": "contract.pdf", "attachment_id": doc, "snippet": ""}]}
    assert provider_file not in sent.blob()


def test_file_citations_of_unknown_deleted_or_foreign_files_are_omitted(api, seed, mock_provider):
    chat, kept = _chat_with_document(seed, "vs_e2e_seeded_0005")
    deleted = seed.insert_attachment(tenant=TENANT_A_ID, chat=chat, user=USER_A_ID,
                                     filename="deleted.pdf", deleted=True)
    other = new_chat(seed)
    foreign = seed.insert_attachment(tenant=TENANT_A_ID, chat=other, user=USER_A_ID,
                                     filename="foreign.pdf")
    mock_provider.script("POST", "/v1/responses", sse_text(["answer"], annotations=[
        file_annotation("file-unknown-unknown-0000", "x.pdf"),
        file_annotation(f"file-{deleted}", "deleted.pdf"),
        file_annotation(f"file-{foreign}", "foreign.pdf"),
        file_annotation(f"file-{kept}", "kept.pdf"),
    ]))
    items = send_message(api, chat, "q").of("citations")[0].data["items"]
    assert [i["attachment_id"] for i in items] == [kept]


def test_nothing_mappable_means_no_citations_event(api, seed, mock_provider):
    chat, _doc = _chat_with_document(seed, "vs_e2e_seeded_0006")
    mock_provider.script("POST", "/v1/responses", sse_text(
        ["answer"], annotations=[file_annotation("file-unknown-unknown-0001", "x.pdf")]))
    sent = send_message(api, chat, "q")
    assert not sent.of("citations") and sent.terminal.event == "done"


def test_web_and_file_citations_share_one_event_in_annotation_order(api, seed, mock_provider):
    chat, doc = _chat_with_document(seed, "vs_e2e_seeded_0007")
    mock_provider.script("POST", "/v1/responses", sse_text(["0123456789"], annotations=[
        web_annotation("https://one.example/", "One", 0, 3),
        file_annotation(f"file-{doc}", "ignored.pdf"),
        web_annotation("https://two.example/", "Two", 3, 6),
    ]))
    sent = send_message(api, chat, "q", web_search={"enabled": True})
    assert len(sent.of("citations")) == 1
    items = sent.of("citations")[0].data["items"]
    assert [i["source"] for i in items] == ["web", "file", "web"]
    assert items[0]["snippet"] == "012"
    assert items[2]["span"] == {"start": 3, "end": 6}
