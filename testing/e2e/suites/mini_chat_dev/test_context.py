"""Context limits and truncation (DESIGN section 4 "Context Plan Assembly and Truncation")."""

import pytest

from ._seed import insert_message
from .helpers import PREFIX, api, assert_problem, create_chat, stream

pytestmark = pytest.mark.usefixtures("server")


def _ts(second: int) -> str:
    return f"2020-01-01T00:00:{second:02d}.000000001Z"


def test_long_message_on_small_model_is_input_too_long(reset_mock):
    s = api()
    chat = create_chat(s, model="gpt-tiny")  # max_input_tokens 3072
    r = s.post(f"{PREFIX}/chats/{chat['id']}/messages:stream", json={"content": "x" * 12_000})
    assert_problem(r, 400, "out_of_range", reason="INPUT_TOO_LONG")
    assert reset_mock.requests(path="/responses") == []


def test_history_is_truncated_by_whole_turns(reset_mock):
    s = api()
    chat = create_chat(s, model="gpt-tiny")
    # Budget 3072 - 100 = 2972 tokens. u2 (~2585 tokens) does not fit after the newer
    # messages, so u1, a1 and u2 are dropped; a2 then would start the kept range and is
    # dropped too (an answer is never sent without its question).
    insert_message(chat["id"], "user", _ts(1), content="q1")
    insert_message(chat["id"], "assistant", _ts(2), content="a1")
    insert_message(chat["id"], "user", _ts(3), content="u" * 9000)
    insert_message(chat["id"], "assistant", _ts(4), content="a2-answer")
    insert_message(chat["id"], "user", _ts(5), content="q3")
    insert_message(chat["id"], "assistant", _ts(6), content="a3")
    current = "current question " + "z" * 400
    res = stream(s, chat["id"], {"content": current})
    assert res.terminal[0] == "done", res.raw
    body = reset_mock.requests(path="/responses")[-1]["json"]
    texts = [(m["role"], m["content"][0]["text"]) for m in body["input"]]
    assert texts == [("user", "q3"), ("assistant", "a3"), ("user", current)]
