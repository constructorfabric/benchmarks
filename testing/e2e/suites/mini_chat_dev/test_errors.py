"""Pre-stream errors and streaming error codes (DESIGN section 3.3 "Error Codes",
"Streaming error codes", provider identifier non-exposure)."""

import uuid
from contextlib import closing

import pytest

from . import mock_provider as mp
from .helpers import PREFIX, TOKEN_B, api, assert_problem, create_chat, db, stream, uuid_bytes

pytestmark = pytest.mark.usefixtures("server")

CHAT_TYPE = "gts.cf.core.mini_chat.chat.v1~"


def _url(chat_id: str) -> str:
    return f"{PREFIX}/chats/{chat_id}/messages:stream"


def _turn(chat_id: str):
    with closing(db()) as conn:
        return conn.execute("SELECT state, error_code FROM chat_turns WHERE chat_id = ?",
                            (uuid_bytes(chat_id),)).fetchone()


def test_provider_error_message_is_sanitized(reset_mock):
    reset_mock.enqueue("responses", {"failed": {
        "code": "server_error",
        "message": "request resp_abc123XYZ failed at https://internal.example.com/v1/x "
                   "with key sk-abcdefghijklmnopqrstuv for file-ABCDEFGHIJKLMNOP"}})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    code, data = res.terminal
    assert code == "error" and data["code"] == "provider_error"
    for leak in ("resp_abc123XYZ", "https://", "internal.example.com", "sk-abcdef", "file-ABCDEFGHIJKLMNOP"):
        assert leak not in res.raw, res.raw
    assert "[provider_id]" in data["message"] and "[url]" in data["message"]
    turn = _turn(chat["id"])
    assert (turn["state"], turn["error_code"]) == ("failed", "provider_error")


def test_provider_429_is_rate_limited_event(reset_mock):
    reset_mock.enqueue("responses", {"status": 429, "headers": {"Retry-After": "7"},
                                     "body": {"error": {"message": "slow down", "type": "rate_limit"}}})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    assert res.status == 200
    assert res.names() == ["stream_started", "error"]
    err = res.of("error")[0]
    assert err["code"] == "rate_limited"
    assert "7" in err["message"]
    assert _turn(chat["id"])["error_code"] == "rate_limited"


def test_provider_http_500_is_provider_error(reset_mock):
    reset_mock.enqueue("responses", {"status": 500})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    assert res.terminal[1]["code"] == "provider_error"


def test_stream_without_terminal_is_provider_error(reset_mock):
    reset_mock.enqueue("responses", {"events": [mp.ev_created(), mp.ev_delta("half")], "disconnect": True})
    s = api()
    chat = create_chat(s)
    res = stream(s, chat["id"], {"content": "hi"})
    assert res.names() == ["stream_started", "delta", "error"]
    assert res.terminal[1]["code"] == "provider_error"
    assert _turn(chat["id"])["state"] == "failed"


def test_empty_content_is_400():
    chat = create_chat(api())
    r = api().post(_url(chat["id"]), json={"content": "   "})
    assert_problem(r, 400, "invalid_argument", reason="EMPTY_CONTENT", field="content")


def test_missing_content_is_422_and_malformed_json_400():
    chat = create_chat(api())
    assert_problem(api().post(_url(chat["id"]), json={"request_id": str(uuid.uuid4())}), 422,
                   "invalid_argument")
    r = api().post(_url(chat["id"]), data="{not json", headers={"Content-Type": "application/json"})
    assert_problem(r, 400, "invalid_argument")


def test_non_uuid_attachment_id_is_422():
    chat = create_chat(api())
    r = api().post(_url(chat["id"]), json={"content": "hi", "attachment_ids": ["nope"]})
    assert_problem(r, 422, "invalid_argument")


def test_duplicate_and_unknown_attachment_ids_are_400(reset_mock):
    chat = create_chat(api())
    dup = str(uuid.uuid4())
    for ids in ([dup, dup], [str(uuid.uuid4())]):
        r = api().post(_url(chat["id"]), json={"content": "hi", "attachment_ids": ids})
        assert_problem(r, 400, "invalid_argument", reason="invalid_attachment", field="attachment")
    assert reset_mock.requests(path="/responses") == []
    assert _turn(chat["id"]) is None


def test_unknown_and_foreign_chat_are_404():
    r = api().post(_url(str(uuid.uuid4())), json={"content": "hi"})
    assert_problem(r, 404, "not_found", resource_type=CHAT_TYPE)
    chat = create_chat(api())
    r = api(TOKEN_B).post(_url(chat["id"]), json={"content": "hi"})
    assert_problem(r, 404, "not_found", resource_type=CHAT_TYPE)


def test_unauthenticated_is_401():
    chat = create_chat(api())
    assert api(None).post(_url(chat["id"]), json={"content": "hi"}).status_code == 401
