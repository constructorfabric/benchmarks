"""messages:stream — SSE contract, persistence, provider request, replay, 409s, errors."""

from __future__ import annotations

import json
import re
import threading
import time
import uuid

import pytest

from .conftest import USERS, assert_problem, field_reasons, quota_rows, ub, wait_until
from .mock_llm import DEFAULT_TEXT, DEFAULT_USAGE, held_stream, json_response, sse_events, text_stream

PROVIDER_ID_RE = re.compile(r"(resp_|file-|vs_|chatcmpl-)mock")


def assert_no_provider_ids(text: str) -> None:
    assert not PROVIDER_ID_RE.search(text), text


@pytest.mark.smoke
def test_stream_event_order_and_payloads(api, mock_llm, provider):
    a = api("A")
    model = provider["model"]
    chat = a.create_chat(title="stream", model=model)
    rid = str(uuid.uuid4())
    res = a.stream(chat["id"], "Hi there", request_id=rid)
    assert res.status == 200, res.body
    assert res.headers["content-type"].startswith("text/event-stream")

    names = [n for n in res.names if n != "ping"]
    assert names[0] == "stream_started"
    assert names[-1] == "done"
    assert set(names[1:-1]) == {"delta"}
    assert names.count("done") == 1 and "error" not in names

    started = res.started
    assert started["request_id"] == rid
    assert started["is_new_turn"] is True
    uuid.UUID(started["message_id"])
    assert "thread_summary_applied" not in started

    assert [d["type"] for d in res.all("delta")] == ["text"] * 3
    assert res.text == DEFAULT_TEXT

    done = res.done
    assert done["usage"] == DEFAULT_USAGE
    assert done["effective_model"] == model
    assert done["selected_model"] == model
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    assert "request_id" not in done and "message_id" not in done
    warnings = done["quota_warnings"]
    assert {(w["tier"], w["period"]) for w in warnings} == {
        ("premium", "daily"),
        ("premium", "monthly"),
        ("total", "daily"),
        ("total", "monthly"),
    }
    for w in warnings:
        assert w["warning"] is False and w["exhausted"] is False
        assert 0 <= w["remaining_percentage"] <= 100
    assert_no_provider_ids(res.body)


def test_server_generates_request_id_when_omitted(api):
    a = api("A")
    chat = a.create_chat()
    res = a.send(chat["id"], "no request id")
    rid = uuid.UUID(res.request_id)
    assert rid.version == 4
    assert a.turn(chat["id"], res.request_id).json()["state"] == "done"


def test_completed_turn_is_persisted(api, db):
    a = api("A")
    chat = a.create_chat()
    res = a.send(chat["id"], "Persist me")
    msgs = a.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    user, asst = msgs
    assert user["content"] == "Persist me"
    assert asst["content"] == DEFAULT_TEXT
    assert user["request_id"] == asst["request_id"] == res.request_id
    assert asst["id"] == res.started["message_id"]
    assert asst["model"] == "gpt-4.1"
    assert asst["input_tokens"] == 42 and asst["output_tokens"] == 7
    assert "model" not in user and "input_tokens" not in user
    for m in msgs:
        assert m["attachments"] == []
        assert "my_reaction" in m and m["my_reaction"] is None

    turn = a.turn(chat["id"], res.request_id).json()
    assert turn["state"] == "done"
    assert turn["assistant_message_id"] == asst["id"]
    assert "error_code" not in turn

    row = db.execute(
        "SELECT state, effective_model, reserved_credits_micro, provider_response_id FROM chat_turns WHERE request_id = ?",
        (ub(res.request_id),),
    ).fetchone()
    assert row["state"] == "completed"
    assert row["effective_model"] == "gpt-4.1"
    assert row["provider_response_id"].startswith("resp_")
    assert a.get(f"/chats/{chat['id']}").json()["message_count"] == 2


def test_message_count_and_order_across_turns(api):
    a = api("A")
    chat = a.create_chat()
    for i in range(3):
        a.send(chat["id"], f"turn {i}")
    msgs = a.messages(chat["id"])
    assert len(msgs) == 6
    assert [m["content"] for m in msgs if m["role"] == "user"] == ["turn 0", "turn 1", "turn 2"]
    stamps = [(m["created_at"], m["id"]) for m in msgs]
    assert stamps == sorted(stamps)
    assert a.get(f"/chats/{chat['id']}").json()["message_count"] == 6

    # paging, ordering and filtering of messages
    page = a.get(f"/chats/{chat['id']}/messages", params={"limit": 4}).json()
    assert len(page["items"]) == 4 and page["page_info"]["next_cursor"]
    rest = a.get(
        f"/chats/{chat['id']}/messages", params={"limit": 4, "cursor": page["page_info"]["next_cursor"]}
    ).json()
    assert [m["id"] for m in page["items"] + rest["items"]] == [m["id"] for m in msgs]
    desc = a.get(f"/chats/{chat['id']}/messages", params={"$orderby": "created_at desc", "limit": 100}).json()
    assert [m["id"] for m in desc["items"]] == [m["id"] for m in reversed(msgs)]
    only_asst = a.messages(chat["id"], **{"$filter": "role eq 'assistant'"})
    assert len(only_asst) == 3 and {m["role"] for m in only_asst} == {"assistant"}
    by_id = a.messages(chat["id"], **{"$filter": f"id eq {msgs[1]['id']}"})
    assert [m["id"] for m in by_id] == [msgs[1]["id"]]
    bad = a.get(f"/chats/{chat['id']}/messages", params={"$filter": "content eq 'x'"})
    assert "INVALID_FILTER" in field_reasons(assert_problem(bad, 400))
    assert_problem(a.get(f"/chats/{chat['id']}/messages", params={"limit": 0}), 400)


def test_provider_request_contents(api, mock_llm, provider):
    a = api("A")
    model = provider["model"]
    chat = a.create_chat(model=model)
    a.send(chat["id"], "first question")
    a.send(chat["id"], "second question")
    reqs = mock_llm.chat_requests(chat["id"])
    assert len(reqs) == 2
    body = reqs[1].json
    u = USERS["A"]
    expected_user = uuid.UUID(u["tenant"]).hex + uuid.UUID(u["id"]).hex
    assert body["user"] == expected_user and len(body["user"]) == 64
    assert body["metadata"] == {
        "tenant_id": u["tenant"],
        "user_id": u["id"],
        "chat_id": chat["id"],
        "request_type": "chat",
        "feature": "none",
    }
    assert body["model"] == model  # provider_model_id
    assert body["stream"] is True
    assert body["store"] is False
    assert body["max_output_tokens"] == 32768
    assert body["instructions"].startswith(f"You are the {model} test assistant.")
    assert "tools" not in body or body["tools"] == []
    # recent history then the new user message
    contents = [(m["role"], m["content"]) for m in body["input"]]
    assert contents[0] == ("user", "first question")
    assert contents[1][0] == "assistant" and DEFAULT_TEXT in str(contents[1][1])
    assert contents[-1] == ("user", "second question")
    assert reqs[1].listener == provider["name"]
    assert reqs[1].raw_path == provider["chat_path"]


def test_web_search_tool_events_citations_and_accounting(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    answer = "Rust 1.80 was released."
    mock_llm.script_chat(
        chat["id"],
        text_stream(
            chunks=[answer],
            before=[
                ("response.web_search_call.searching", {"type": "response.web_search_call.searching", "item_id": "ws_1"}),
                ("response.web_search_call.completed", {"type": "response.web_search_call.completed", "item_id": "ws_1"}),
            ],
            annotations=[
                {
                    "type": "url_citation",
                    "url": "https://blog.rust-lang.org/",
                    "title": "Rust Blog",
                    "start_index": 0,
                    "end_index": 9,
                }
            ],
        ),
    )
    res = a.send(chat["id"], "latest rust?", web_search={"enabled": True})
    names = [n for n in res.names if n != "ping"]
    assert names.index("tool") < names.index("citations") < names.index("done")
    tools = res.all("tool")
    assert [(t["phase"], t["name"]) for t in tools] == [("start", "web_search"), ("done", "web_search")]
    cites = res.first("citations")["items"]
    assert cites == [
        {
            "source": "web",
            "title": "Rust Blog",
            "url": "https://blog.rust-lang.org/",
            "snippet": answer[0:9],
            "span": {"start": 0, "end": 9},
        }
    ]
    body = mock_llm.chat_requests(chat["id"])[0].json
    assert {"type": "web_search", "search_context_size": "low"} in body["tools"]
    assert body["metadata"]["feature"] == "web_search"
    assert "web_search" in body["instructions"]  # guard text appended

    turn = db.execute(
        "SELECT web_search_enabled, web_search_completed_count FROM chat_turns WHERE request_id = ?",
        (ub(res.request_id),),
    ).fetchone()
    assert turn["web_search_enabled"] == 1 and turn["web_search_completed_count"] == 1
    rows = quota_rows(db, "A")
    assert rows[("daily", "total")]["web_search_calls"] >= 1


def test_web_search_calls_over_limit_fail_turn(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    ws = []
    for i in range(3):  # default quota.web_search_max_calls_per_message = 2
        ws.append(("response.web_search_call.searching", {"type": "response.web_search_call.searching", "item_id": f"ws_{i}"}))
        ws.append(("response.web_search_call.completed", {"type": "response.web_search_call.completed", "item_id": f"ws_{i}"}))
    mock_llm.script_chat(chat["id"], text_stream(before=ws))
    res = a.stream(chat["id"], "search a lot", web_search={"enabled": True})
    assert res.status == 200
    assert res.names[-1] == "error"
    assert res.error["code"] == "web_search_calls_exceeded"
    assert a.wait_turn(chat["id"], res.request_id, ("error",))["error_code"] == "web_search_calls_exceeded"


def test_replay_of_completed_turn_is_side_effect_free(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    first = a.send(chat["id"], "replay me")
    rid = first.request_id
    time.sleep(0.5)  # let usage/audit enqueue settle
    quota_before = quota_rows(db, "A")
    outbox_before = db.execute("SELECT COUNT(*) FROM toolkit_outbox_body").fetchone()[0]
    calls_before = len(mock_llm.chat_requests(chat["id"]))

    replay = a.stream(chat["id"], "replay me", request_id=rid)
    assert replay.status == 200, replay.body
    assert replay.names == ["stream_started", "delta", "done"]
    assert replay.started == {"request_id": rid, "message_id": first.started["message_id"], "is_new_turn": False}
    assert replay.first("delta") == {"type": "text", "content": DEFAULT_TEXT}
    d = replay.done
    assert d["usage"] == DEFAULT_USAGE
    assert d["effective_model"] == d["selected_model"] == "gpt-4.1"
    assert d["quota_decision"] == "allow"
    assert "quota_warnings" not in d and "downgrade_reason" not in d

    assert len(mock_llm.chat_requests(chat["id"])) == calls_before
    assert quota_rows(db, "A") == quota_before
    assert db.execute("SELECT COUNT(*) FROM toolkit_outbox_body").fetchone()[0] == outbox_before
    assert len(a.messages(chat["id"])) == 2


def test_running_turn_conflicts_and_replay_before_parallel_guard(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    done_turn = a.send(chat["id"], "completed first")

    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release))
    rid = str(uuid.uuid4())
    live = a.open_stream(chat["id"], "slow one", request_id=rid)
    try:
        live.read_until("delta")  # first delta relayed while the provider still streams
        assert a.wait_turn(chat["id"], rid, ("running",))["state"] == "running"

        same = a.stream(chat["id"], "slow one", request_id=rid)
        p = assert_problem_from(same, 409)
        assert p["context"]["reason"] == "request_id_conflict"

        other = a.stream(chat["id"], "parallel", request_id=str(uuid.uuid4()))
        p = assert_problem_from(other, 409)
        assert p["context"]["reason"] == "turn_already_running"
        p = assert_problem_from(a.stream(chat["id"], "no id"), 409)
        assert p["context"]["reason"] == "turn_already_running"

        # replay is checked before the parallel-turn guard
        replay = a.stream(chat["id"], "completed first", request_id=done_turn.request_id)
        assert replay.status == 200 and replay.started["is_new_turn"] is False
    finally:
        release.set()
    events = live.read_all()
    live.close()
    assert events[-1][0] == "done"
    a.wait_turn(chat["id"], rid, ("done",))
    # a new turn is accepted once the previous one is terminal
    assert a.send(chat["id"], "after").names[-1] == "done"


def assert_problem_from(res, status):
    assert res.status == status, res.body
    assert res.headers["content-type"].startswith("application/problem+json")
    p = res.problem
    assert p["status"] == status
    return p


def test_failed_turn_request_id_is_conflict(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], json_response(500, {"error": {"message": "boom", "type": "server_error"}}))
    rid = str(uuid.uuid4())
    res = a.stream(chat["id"], "will fail", request_id=rid)
    assert res.status == 200 and res.names[-1] == "error"
    assert res.error["code"] == "provider_error"
    p = assert_problem_from(a.stream(chat["id"], "will fail", request_id=rid), 409)
    assert p["context"]["reason"] == "request_id_conflict"


def test_provider_error_event_is_terminal_and_sanitized(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    secret = (
        "Upstream failure for resp_abcdef1234567890 file-abcdefghijklmnop vs_abcdefghijklmnop "
        "see https://internal.example/x key sk-abcdefghijkl1234"
    )
    mock_llm.script_chat(
        chat["id"],
        sse_events(
            [
                ("response.created", {"type": "response.created", "response": {"id": "resp_abcdef1234567890"}}),
                ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "partial"}),
                (
                    "response.failed",
                    {
                        "type": "response.failed",
                        "response": {
                            "id": "resp_abcdef1234567890",
                            "status": "failed",
                            "error": {"code": "server_error", "message": secret},
                        },
                    },
                ),
            ]
        ),
    )
    res = a.stream(chat["id"], "fail please")
    assert res.status == 200
    assert res.names[0] == "stream_started"
    assert res.names[-1] == "error" and res.names.count("error") == 1 and "done" not in res.names
    err = res.error
    assert set(err) == {"code", "message"}
    assert err["code"] == "provider_error"
    msg = err["message"]
    for leaked in ("resp_abcdef1234567890", "file-abcdefghijklmnop", "vs_abcdefghijklmnop", "https://internal", "sk-abcdefghijkl1234"):
        assert leaked not in msg, msg
    assert "[provider_id]" in msg and "[url]" in msg and "[credential]" in msg

    turn = a.wait_turn(chat["id"], res.request_id, ("error",))
    assert turn["error_code"] == "provider_error"
    assert "assistant_message_id" not in turn
    row = db.execute("SELECT state, reserved_credits_micro FROM chat_turns WHERE request_id = ?", (ub(res.request_id),)).fetchone()
    assert row["state"] == "failed"


@pytest.mark.parametrize(
    "status,body,code",
    [
        (429, {"error": {"message": "slow down", "type": "rate_limit"}}, "rate_limited"),
        (500, {"error": {"message": "internal", "type": "server_error"}}, "provider_error"),
    ],
)
def test_provider_http_errors_map_to_stream_codes(api, mock_llm, status, body, code):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], json_response(status, body, headers={"retry-after": "7"}))
    res = a.stream(chat["id"], "x")
    assert res.status == 200, res.body
    assert res.names[0] == "stream_started" and res.names[-1] == "error"
    assert res.error["code"] == code
    assert a.wait_turn(chat["id"], res.request_id, ("error",))["error_code"] == code


def test_incomplete_response_finishes_with_done(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], text_stream(chunks=["cut"], terminal="response.incomplete"))
    res = a.send(chat["id"], "long answer")
    assert res.text == "cut"
    assert "citations" not in res.names
    turn = a.turn(chat["id"], res.request_id).json()
    assert turn["state"] == "done" and "error_code" not in turn


def test_preflight_validation_before_provider_call(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    for content in ("", "   \n\t"):
        p = assert_problem_from(a.stream(chat["id"], content), 400)
        assert field_reasons(p) == ["EMPTY_CONTENT"]
    # schema mismatch / malformed json / non-uuid attachment id
    assert a.post(f"/chats/{chat['id']}/messages:stream", json={}).status_code == 422
    assert a.post(f"/chats/{chat['id']}/messages:stream", json={"content": "x", "attachment_ids": ["nope"]}).status_code == 422
    r = a.post(
        f"/chats/{chat['id']}/messages:stream", content=b"{bad", headers={"content-type": "application/json"}
    )
    assert r.status_code == 400
    # unknown attachment id
    p = assert_problem_from(a.stream(chat["id"], "x", attachment_ids=[str(uuid.uuid4())]), 400)
    assert "invalid_attachment" in field_reasons(p)
    # message larger than the model's max_input_tokens (tiny model: 3072)
    tiny = a.create_chat(model="gpt-tiny-ctx")
    p = assert_problem_from(a.stream(tiny["id"], "word " * 4000), 400)
    assert "INPUT_TOO_LONG" in str(p["context"]), p
    assert mock_llm.chat_requests(chat["id"]) == []
    assert mock_llm.chat_requests(tiny["id"]) == []
    assert a.messages(chat["id"]) == []
    assert a.get(f"/chats/{chat['id']}").json()["message_count"] == 0


def test_client_disconnect_cancels_turn(api, mock_llm, db):
    a = api("A")
    chat = a.create_chat()
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release, first_chunk="Partial answer"))
    live = a.open_stream(chat["id"], "cancel me")
    try:
        events = live.read_until("delta")
        rid = events[0][1]["request_id"]
    finally:
        live.close()  # client disconnects before the terminal event
    try:
        turn = a.wait_turn(chat["id"], rid, ("cancelled",), timeout=20)
        # the provider request is aborted as well (the mock sees the connection close)
        provider_closed = wait_until(lambda: any(r.chat_id == chat["id"] for r in mock_llm.disconnects), timeout=10)
    finally:
        release.set()
    assert provider_closed
    # partial content was persisted
    assert turn["assistant_message_id"]
    msgs = a.messages(chat["id"])
    asst = [m for m in msgs if m["role"] == "assistant"]
    assert asst and asst[0]["content"] == "Partial answer"
    assert asst[0]["id"] == turn["assistant_message_id"]
    row = db.execute("SELECT state, reserved_credits_micro FROM chat_turns WHERE request_id = ?", (ub(rid),)).fetchone()
    assert row["state"] == "cancelled"
    # quota settled: nothing left reserved for this user
    assert wait_until(lambda: all(r["reserved_credits_micro"] == 0 for r in quota_rows(db, "A").values()))
    # chat unblocked
    assert a.send(chat["id"], "next").names[-1] == "done"


def test_ping_before_first_delta(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], text_stream(pause_after_created=6.5))
    res = a.send(chat["id"], "think first")
    names = res.names
    assert "ping" in names
    first_delta = names.index("delta")
    assert all(n == "ping" for n in names[1:first_delta])
    assert "ping" not in names[first_delta:]
    assert res.all("ping")[0] in ({}, None)


def test_turn_status_404_cases(api):
    a, b = api("A"), api("B")
    chat = a.create_chat()
    res = a.send(chat["id"], "x")
    p = assert_problem(a.turn(chat["id"], str(uuid.uuid4())), 404)
    assert p["context"]["resource_type"] == "gts.cf.core.mini_chat.turn.v1~"
    assert_problem(b.turn(chat["id"], res.request_id), 404)
    assert b.retry(chat["id"], res.request_id).status == 404


def test_provider_wire_robustness_unicode_split_writes_and_crlf(api, mock_llm):
    """Provider SSE split into 3-byte writes (mid code point) with CRLF line ends."""
    a = api("A")
    chat = a.create_chat()

    def split_writes(h, req):
        h.start_sse()
        events = [
            ("response.created", {"type": "response.created", "response": {"id": "resp_split00000000001"}}),
            ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "Привет 👋 "}),
            ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "мир ✓"}),
            ("response.completed", {"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 3}}}),
        ]
        for name, data in events:
            raw = f"event: {name}\r\ndata: {json.dumps(data, ensure_ascii=False)}\r\n\r\n".encode()
            for i in range(0, len(raw), 3):
                h.wfile.write(raw[i : i + 3])
                h.wfile.flush()
                time.sleep(0.001)
        h.close_connection = True

    mock_llm.script_chat(chat["id"], split_writes)
    res = a.send(chat["id"], "unicode please")
    assert res.text == "Привет 👋 мир ✓"
    assert a.messages(chat["id"])[-1]["content"] == "Привет 👋 мир ✓"
    assert res.done["usage"] == {"input_tokens": 5, "output_tokens": 3}


def test_provider_events_without_event_lines_use_type_field(api, mock_llm):
    a = api("A")
    chat = a.create_chat()

    def data_only(h, req):
        h.start_sse()
        for d in (
            {"type": "response.created", "response": {"id": "resp_dataonly000000001"}},
            {"type": "response.output_text.delta", "delta": "typed"},
            {"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}},
        ):
            h.wfile.write(f"data: {json.dumps(d)}\n\n".encode())
        h.wfile.flush()
        h.close_connection = True

    mock_llm.script_chat(chat["id"], data_only)
    assert a.send(chat["id"], "x").text == "typed"


def test_provider_stream_cut_without_terminal_is_provider_error(api, mock_llm):
    a = api("A")
    chat = a.create_chat()

    def cut(h, req):
        h.start_sse()
        h.write_event("response.created", {"type": "response.created", "response": {"id": "resp_cut0000000000001"}})
        h.write_event("response.output_text.delta", {"type": "response.output_text.delta", "delta": "half"})
        h.close_connection = True

    mock_llm.script_chat(chat["id"], cut)
    res = a.stream(chat["id"], "cut me")
    assert res.names[-1] == "error" and res.error["code"] == "provider_error"
    turn = a.wait_turn(chat["id"], res.request_id, ("error",))
    assert turn["error_code"] == "provider_error" and "assistant_message_id" not in turn
    # the user message stays, no assistant message for a failed turn
    assert [m["role"] for m in a.messages(chat["id"])] == ["user"]


def test_provider_auth_error_message_is_sanitized(api, mock_llm):
    a = api("A")
    chat = a.create_chat()
    mock_llm.script_chat(chat["id"], json_response(401, {"error": {"message": "Incorrect API key provided: sk-abcdefghijklmnop"}}))
    res = a.stream(chat["id"], "x")
    assert res.error["code"] == "provider_error"
    assert "sk-abcdefghijklmnop" not in res.body and "[credential]" in res.error["message"]


def test_parallel_sends_only_one_turn_runs(api, mock_llm):
    from concurrent.futures import ThreadPoolExecutor

    a = api("A")
    chat = a.create_chat()
    release = threading.Event()
    mock_llm.script_chat(chat["id"], held_stream(release), times=5)
    try:
        with ThreadPoolExecutor(4) as ex:
            futures = [ex.submit(a.stream, chat["id"], f"p{i}") for i in range(4)]
            # release only once the winner's stream is held at the provider and
            # every other request has already been answered
            settled = wait_until(
                lambda: mock_llm.chat_requests(chat["id"]) and sum(f.done() for f in futures) >= 3, timeout=20
            )
            release.set()
            results = [f.result() for f in futures]
            assert settled, [(r.status, r.body[:120]) for r in results]
    finally:
        release.set()
    ok = [r for r in results if r.status == 200]
    conflicts = [r for r in results if r.status == 409]
    assert len(ok) == 1, [(r.status, r.body[:120]) for r in results]
    assert len(conflicts) == 3
    assert {r.problem["context"]["reason"] for r in conflicts} == {"turn_already_running"}
    assert len([m for m in a.messages(chat["id"]) if m["role"] == "user"]) == 1
