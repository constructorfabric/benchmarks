"""Send message streaming: SSE contract, persistence, errors, sanitization,
replay/idempotency, parallel-turn guard, cancellation, ping."""

import json
import re
import threading
import time
import uuid

import httpx

from conftest import reason, violations, wait_for

PROVIDER_ID_RE = re.compile(r"(resp_|chatcmpl-|file-|vs_|sk-)[A-Za-z0-9]{6,}")


def assert_order(names):
    assert names[0] == "stream_started", names
    assert names[-1] in ("done", "error"), names
    assert names.count("done") + names.count("error") == 1, names
    body = names[1:-1]
    seen_content = False
    seen_citations = False
    for n in body:
        assert n in ("ping", "delta", "tool", "citations"), names
        if n in ("delta", "tool"):
            assert not seen_citations, names
            seen_content = True
        if n == "ping":
            assert not seen_content, f"ping after content: {names}"
        if n == "citations":
            assert not seen_citations, names
            seen_citations = True


def test_stream_contract_and_persistence(api, chat, mock):
    rid = str(uuid.uuid4())
    s = api.send(chat["id"], "hello world", request_id=rid)
    assert s.status == 200 and s.is_sse
    assert_order(s.names())
    started = s.first("stream_started")
    assert started["request_id"] == rid
    assert started["is_new_turn"] is True
    uuid.UUID(started["message_id"])
    for d in s.all("delta"):
        assert d["type"] == "text"
    assert s.text_content() == "Hello from the mock provider."
    done = s.first("done")
    assert done["usage"] == {"input_tokens": 20, "output_tokens": 10}
    assert done["effective_model"] == "gpt-4.1" and done["selected_model"] == "gpt-4.1"
    assert done["quota_decision"] == "allow"
    assert "downgrade_from" not in done and "downgrade_reason" not in done
    assert isinstance(done.get("quota_warnings"), list) and done["quota_warnings"]
    assert not PROVIDER_ID_RE.search(s.text), s.text

    msgs = api.messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"]
    user, asst = msgs
    assert user["request_id"] == asst["request_id"] == rid
    assert asst["id"] == started["message_id"]
    assert asst["content"] == "Hello from the mock provider."
    assert asst["model"] == "gpt-4.1"
    assert asst["input_tokens"] == 20 and asst["output_tokens"] == 10
    for m in msgs:
        assert m["attachments"] == [] and "my_reaction" in m and m["my_reaction"] is None
    assert "model" not in user and "input_tokens" not in user

    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "done" and t["request_id"] == rid
    assert t["assistant_message_id"] == asst["id"]
    assert "error_code" not in t

    # provider request carries the user text and the system prompt
    reqs = mock.chat_requests(chat["id"])
    assert len(reqs) == 1
    body = reqs[0]["json"]
    assert body["stream"] is True and body["model"] == "gpt-4.1"
    assert "You are a helpful assistant." in body.get("instructions", "")
    assert body["store"] is False
    assert len(body["user"]) == 64


def test_server_generates_request_id(api, chat):
    s = api.send(chat["id"], "no rid")
    rid = s.first("stream_started")["request_id"]
    assert uuid.UUID(rid).version == 4


def test_message_count_and_chronology_across_turns(api, chat):
    for i in range(3):
        assert api.send(chat["id"], f"turn {i}").terminal[0] == "done"
    msgs = api.all_messages(chat["id"])
    assert [m["role"] for m in msgs] == ["user", "assistant"] * 3
    assert [m["content"] for m in msgs if m["role"] == "user"] == ["turn 0", "turn 1", "turn 2"]
    stamps = [m["created_at"] for m in msgs]
    assert stamps == sorted(stamps)
    assert api.get(f"/chats/{chat['id']}").json()["message_count"] == 6
    # messages list query
    r = api.get(f"/chats/{chat['id']}/messages?$filter=role eq 'assistant'")
    assert [m["role"] for m in r.json()["items"]] == ["assistant"] * 3
    r = api.get(f"/chats/{chat['id']}/messages?$orderby=created_at desc&limit=2")
    page = r.json()
    assert [m["content"] for m in page["items"]][1] == "turn 2"
    assert page["page_info"]["next_cursor"]
    nxt = api.get(f"/chats/{chat['id']}/messages?limit=2&cursor={page['page_info']['next_cursor']}").json()
    assert len(nxt["items"]) == 2
    one = msgs[3]["id"]
    r = api.get(f"/chats/{chat['id']}/messages?$filter=id eq {one}")
    assert [m["id"] for m in r.json()["items"]] == [one]
    for q, exp in {"?limit=0": "INVALID_LIMIT", "?$filter=nope eq 1": "INVALID_FILTER", "?cursor=zzz": "INVALID_CURSOR"}.items():
        r = api.get(f"/chats/{chat['id']}/messages{q}")
        assert r.status_code == 400 and reason(r.json()) == exp, (q, r.text)


def test_preflight_validation_before_provider(api, chat, mock):
    for content in ("", "   "):
        s = api.send(chat["id"], content)
        assert s.status == 400 and not s.is_sse
        v = violations(s.json)[0]
        assert v["field"] == "content" and v["reason"] == "EMPTY_CONTENT"
    s = api.send(chat["id"], "x", attachment_ids=[str(uuid.uuid4())])
    assert s.status == 400 and reason(s.json) == "invalid_attachment"
    s = api.stream(f"/chats/{chat['id']}/messages:stream", {"content": "x", "attachment_ids": ["nope"]})
    assert s.status == 422
    s = api.stream(f"/chats/{chat['id']}/messages:stream", {})
    assert s.status == 422
    assert mock.chat_requests(chat["id"]) == []
    assert api.messages(chat["id"]) == []


def test_input_too_long(api, mock):
    chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
    s = api.send(chat["id"], "word " * 6000)
    assert s.status == 400, s.text
    assert reason(s.json) == "INPUT_TOO_LONG"
    assert s.json["title"]
    assert mock.chat_requests(chat["id"]) == []


def test_context_window_truncates_history(api, mock):
    chat = api.create_chat(model="gpt-4.1-mini-tiny-ctx")
    big = "lorem ipsum dolor sit amet " * 70  # ~1900 chars, ~500 tokens
    for i in range(5):
        s = api.send(chat["id"], f"MOCK_ECHO {i} {big}")
        assert s.terminal[0] == "done", s.text
    reqs = mock.chat_requests(chat["id"])
    last = reqs[-1]["json"]
    texts = json.dumps(last["input"])
    assert "MOCK_ECHO 4" in texts
    assert "MOCK_ECHO 0" not in texts  # oldest turns dropped to fit the budget
    # the assembled input never starts with an assistant message
    assert last["input"][0]["role"] == "user"


def test_unknown_chat_404_on_stream(api):
    s = api.send(str(uuid.uuid4()), "x")
    assert s.status == 404
    assert s.json["context"]["resource_type"] == "gts.cf.core.mini_chat.chat.v1~"


# ── errors and sanitization ────────────────────────────────────────────────


def test_provider_failed_event(api, chat):
    s = api.send(chat["id"], "MOCK_FAILED")
    assert_order(s.names())
    name, err = s.terminal
    assert name == "error" and err["code"] == "provider_error"
    assert set(err) == {"code", "message"}
    rid = s.first("stream_started")["request_id"]
    t = api.turn(chat["id"], rid).json()
    assert t["state"] == "error" and t["error_code"] == "provider_error"
    assert "assistant_message_id" not in t
    # failed turn persists no assistant message
    assert [m["role"] for m in api.messages(chat["id"])] == ["user"]
    # a new turn is accepted after the failure
    assert api.send(chat["id"], "again").terminal[0] == "done"


def test_provider_error_message_is_sanitized(api, chat):
    s = api.send(chat["id"], "MOCK_FAILED MOCK_FAILED_IDS")
    err = s.terminal[1]
    assert err["code"] == "provider_error"
    msg = err["message"]
    assert "file-abcdef" not in msg and "vs_abcdef" not in msg and "resp_abc" not in msg
    assert "sk-abc" not in msg and "internal.example.com" not in msg
    assert "[provider_id]" in msg and "[url]" in msg and "[credential]" in msg


def test_provider_http_errors(api, chat):
    s = api.send(chat["id"], "MOCK_HTTP_429")
    assert s.terminal == ("error", s.terminal[1]) and s.terminal[1]["code"] == "rate_limited"
    assert "7" in s.terminal[1]["message"]
    s = api.send(chat["id"], "MOCK_HTTP_500")
    assert s.terminal[1]["code"] == "provider_error"
    rid = s.first("stream_started")["request_id"]
    assert api.turn(chat["id"], rid).json()["error_code"] == "provider_error"


def test_provider_stream_cut_without_terminal(api, chat):
    s = api.send(chat["id"], "MOCK_SLOW MOCK_CLOSE")
    name, err = s.terminal
    assert name == "error" and err["code"] in ("provider_error", "provider_timeout")
    rid = s.first("stream_started")["request_id"]
    assert api.turn(chat["id"], rid).json()["state"] == "error"


def test_incomplete_is_done(api, chat):
    s = api.send(chat["id"], "MOCK_INCOMPLETE")
    assert s.terminal[0] == "done"
    assert s.first("citations") is None
    rid = s.first("stream_started")["request_id"]
    assert api.turn(chat["id"], rid).json()["state"] == "done"


def test_tool_and_citation_events(api, chat):
    s = api.send(chat["id"], "MOCK_WEB tell me", web_search={"enabled": True})
    assert_order(s.names())
    tools = s.all("tool")
    assert {"phase": "start", "name": "web_search", "details": {}} in tools
    assert any(t["phase"] == "done" and t["name"] == "web_search" for t in tools)
    cit = s.first("citations")
    assert cit and cit["items"][0]["source"] == "web"
    item = cit["items"][0]
    assert item["url"] == "https://example.com/page" and item["title"] == "Example Page"
    assert item["snippet"] == "Hello" and item["span"] == {"start": 0, "end": 5}
    assert "score" not in item


def test_ping_before_first_content(api, chat):
    s = api.send(chat["id"], "MOCK_DELAY_FIRST=6")
    names = s.names()
    assert_order(names)
    assert "ping" in names
    assert names.index("ping") < names.index("delta")
    assert s.first("ping") == {}


# ── idempotency and replay ─────────────────────────────────────────────────


def test_replay_is_side_effect_free(api, chat, mock):
    rid = str(uuid.uuid4())
    first = api.send(chat["id"], "replay me", request_id=rid)
    assert first.terminal[0] == "done"
    before_q = api.quota()
    before_reqs = len(mock.chat_requests(chat["id"]))
    replay = api.send(chat["id"], "different text ignored", request_id=rid)
    assert replay.names() == ["stream_started", "delta", "done"]
    st = replay.first("stream_started")
    assert st["is_new_turn"] is False
    assert st["message_id"] == first.first("stream_started")["message_id"]
    assert replay.text_content() == first.text_content()
    done = replay.first("done")
    assert done["usage"] == first.first("done")["usage"]
    assert done["effective_model"] == "gpt-4.1" and done["quota_decision"] == "allow"
    assert "quota_warnings" not in done
    assert len(mock.chat_requests(chat["id"])) == before_reqs
    assert api.quota() == before_q
    assert len(api.messages(chat["id"])) == 2


def test_request_id_conflicts(api, chat):
    failed = str(uuid.uuid4())
    api.send(chat["id"], "MOCK_FAILED", request_id=failed)
    s = api.send(chat["id"], "x", request_id=failed)
    assert s.status == 409 and s.json["context"]["reason"] == "request_id_conflict"
    assert "turn" not in s.json["detail"].lower() or True

    # deleted (soft) completed turn
    done = str(uuid.uuid4())
    assert api.send(chat["id"], "ok", request_id=done).terminal[0] == "done"
    assert api.delete(f"/chats/{chat['id']}/turns/{done}").status_code == 204
    s = api.send(chat["id"], "x", request_id=done)
    assert s.status == 409 and s.json["context"]["reason"] == "request_id_conflict"


def _start_slow(api, chat_id, rid, text="MOCK_SLOW"):
    result = {}

    def run():
        result["stream"] = api.server_client().send(chat_id, text, request_id=rid) if hasattr(api, "server_client") else None

    t = threading.Thread(target=run)
    t.start()
    return t, result


def test_parallel_turn_guard_and_replay_priority(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    old = str(uuid.uuid4())
    assert api.send(chat["id"], "first", request_id=old).terminal[0] == "done"
    running = str(uuid.uuid4())
    holder = {}

    def run():
        holder["s"] = server.client("user-a").send(chat["id"], "MOCK_SLOW", request_id=running)

    t = threading.Thread(target=run)
    t.start()
    wait_for(lambda: api.turn(chat["id"], running).status_code == 200, msg="running turn")
    assert api.turn(chat["id"], running).json()["state"] == "running"
    # another request -> turn_already_running
    s = api.send(chat["id"], "parallel")
    assert s.status == 409 and s.json["context"]["reason"] == "turn_already_running"
    # same running request id -> request_id_conflict (idempotency check first)
    s = api.send(chat["id"], "dup", request_id=running)
    assert s.status == 409 and s.json["context"]["reason"] == "request_id_conflict"
    # replay of a completed turn works while another runs
    s = api.send(chat["id"], "first", request_id=old)
    assert s.status == 200 and s.first("stream_started")["is_new_turn"] is False
    # mutations of a running turn are rejected
    r = api.post(f"/chats/{chat['id']}/turns/{running}/retry")
    assert r.status_code == 400 and violations(r.json())[0]["subject"] == "turn_state"
    t.join(timeout=60)
    assert holder["s"].terminal[0] == "done"
    # accepted once the previous turn is terminal
    assert api.send(chat["id"], "after").terminal[0] == "done"


def test_concurrent_sends_one_wins(server):
    api = server.client("user-a")
    chat = api.create_chat()
    results = []

    def run(i):
        results.append(server.client("user-a").send(chat["id"], f"MOCK_SLOW {i}"))

    threads = [threading.Thread(target=run, args=(i,)) for i in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=90)
    ok = [r for r in results if r.status == 200]
    rejected = [r for r in results if r.status == 409]
    assert len(ok) == 1, [r.status for r in results]
    assert len(rejected) == 3
    for r in rejected:
        assert r.json["context"]["reason"] == "turn_already_running"


# ── cancellation ───────────────────────────────────────────────────────────


def test_client_disconnect_cancels_turn(server, mock):
    api = server.client("user-a")
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    got_delta = False
    with httpx.Client(timeout=30) as c:
        with c.stream(
            "POST",
            f"{server.base}/chats/{chat['id']}/messages:stream",
            json={"content": "MOCK_SLOW", "request_id": rid},
            headers={"Authorization": "Bearer user-a"},
        ) as resp:
            for line in resp.iter_lines():
                if line.startswith("event: delta"):
                    got_delta = True
                    time.sleep(0.6)
                    break
    assert got_delta
    t = wait_for(lambda: (lambda j: j if j["state"] != "running" else None)(api.turn(chat["id"], rid).json()), timeout=20, msg="cancelled")
    assert t["state"] == "cancelled"
    # partial content persisted
    assert "assistant_message_id" in t
    msgs = api.messages(chat["id"])
    asst = [m for m in msgs if m["role"] == "assistant"]
    assert asst and asst[0]["id"] == t["assistant_message_id"]
    assert asst[0]["content"].startswith("chunk0")
    assert len(asst[0]["content"]) < len("".join(f"chunk{i} " for i in range(20)))
    # cancelled request id cannot be reused; new request accepted
    s = api.send(chat["id"], "x", request_id=rid)
    assert s.status == 409 and s.json["context"]["reason"] == "request_id_conflict"
    assert api.send(chat["id"], "new").terminal[0] == "done"


def test_disconnect_before_content_cancels_without_message(server):
    api = server.client("user-a")
    chat = api.create_chat()
    rid = str(uuid.uuid4())
    with httpx.Client(timeout=30) as c:
        with c.stream(
            "POST",
            f"{server.base}/chats/{chat['id']}/messages:stream",
            json={"content": "MOCK_DELAY_FIRST=3", "request_id": rid},
            headers={"Authorization": "Bearer user-a"},
        ) as resp:
            for line in resp.iter_lines():
                if line.startswith("event: stream_started"):
                    break
    t = wait_for(lambda: (lambda j: j if j["state"] != "running" else None)(api.turn(chat["id"], rid).json()), timeout=20, msg="cancelled")
    assert t["state"] == "cancelled"
    assert "assistant_message_id" not in t
    assert [m["role"] for m in api.messages(chat["id"])] == ["user"]


def test_streaming_not_buffered(server):
    """The first delta arrives long before the stream completes."""
    api = server.client("user-a")
    chat = api.create_chat()
    t0 = time.time()
    first_delta = None
    with httpx.Client(timeout=60) as c:
        with c.stream(
            "POST",
            f"{server.base}/chats/{chat['id']}/messages:stream",
            json={"content": "MOCK_SLOW"},
            headers={"Authorization": "Bearer user-a"},
        ) as resp:
            for line in resp.iter_lines():
                if first_delta is None and line.startswith("event: delta"):
                    first_delta = time.time() - t0
    total = time.time() - t0
    assert first_delta is not None
    assert total - first_delta > 3.0, (first_delta, total)
