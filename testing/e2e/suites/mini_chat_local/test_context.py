"""Context assembly black-box: what history the provider receives for a turn.

Source: DESIGN 4 "Context Plan Assembly and Truncation" (order, truncation priority,
algorithm: mandatory items, then recent messages newest first, whole turns), "ContextPlan
Determinism and Snapshot Boundary" (``(created_at, id)`` ordering, recent messages query,
``K = context.recent_messages_limit``, default 10), 2.2 "Context Window Budget" and 5.5.4 (token
estimation, reproduced by ``catalog.estimate_tokens``). The tiny-context models live on the
"restricted" server of ``conftest.py`` (``catalog.restricted_catalog``).
"""

from __future__ import annotations

import uuid

from . import catalog
from .support import (
    TENANT_A_ID,
    USER_A_ID,
    send_message,
    uid,
)

PREMIUM = catalog.PREMIUM_ID


def new_chat(seed, model=PREMIUM, **kwargs):
    return seed.insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=model, **kwargs)


def last_request(mock):
    requests = mock.requests_to("POST", "/v1/responses")
    assert requests, "the provider was never called"
    return requests[-1]


def seed_history(seed, chat, pairs):
    """``pairs`` completed turns: ``question i`` / ``answer i``, ascending timestamps."""
    for i in range(pairs):
        rid = str(uuid.uuid4())
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", request_id=rid,
                            content=f"question {i + 1}", created=i * 2 + 1)
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", request_id=rid,
                            content=f"answer {i + 1}", created=i * 2 + 2, model=PREMIUM)


def padded(label):
    return label + "x" * (catalog.FIT_MESSAGE_BYTES - len(label))


def seed_three_turns(seed, chat):
    """Q1 A1 Q2 A2 Q3 A3, 400 bytes each."""
    for i in (1, 2, 3):
        rid = str(uuid.uuid4())
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", request_id=rid,
                            content=padded(f"Q{i}"), created=i * 2)
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", request_id=rid,
                            content=padded(f"A{i}"), created=i * 2 + 1)


def shape(mock):
    """``role:first two characters`` of every conversation item of the last request."""
    return [f"{role}:{text[:2]}" for role, text in last_request(mock).conversation()]


# --------------------------------------------------------------------------- #
# the recent messages window
# --------------------------------------------------------------------------- #


def test_the_last_ten_messages_are_sent_by_default(api, seed, mock_provider):
    chat = new_chat(seed)
    seed_history(seed, chat, 7)  # 14 messages
    send_message(api, chat, "current")
    conversation = last_request(mock_provider).conversation_texts()
    assert len(conversation) == 11, "ten recent messages and the current one"
    assert conversation[0] == "user:question 3"
    assert conversation[9] == "assistant:answer 7"
    assert conversation[10] == "user:current"


def test_messages_are_ordered_by_created_at_then_id(api, seed, mock_provider):
    chat = new_chat(seed)
    # same timestamp, inserted out of id order
    for n, role, text in ((3, "user", "u-c"), (1, "user", "u-a"), (2, "assistant", "a-b")):
        seed.insert_message(tenant=TENANT_A_ID, chat=chat, role=role, content=text,
                            message_id=uid(n), request_id=str(uuid.uuid4()), created=5)
    send_message(api, chat, "current")
    assert last_request(mock_provider).conversation_texts() == [
        "user:u-a", "assistant:a-b", "user:u-c", "user:current"]


def test_deleted_nameless_and_foreign_messages_are_not_part_of_the_context(api, seed,
                                                                         mock_provider):
    chat, other = new_chat(seed), new_chat(seed)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", content="kept question",
                        created=1)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="assistant", content="kept answer",
                        created=2)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", content="deleted question",
                        created=3, deleted=True)
    seed.insert_message(tenant=TENANT_A_ID, chat=chat, role="user", content="no request id",
                        created=4, request_id=None)
    seed.insert_message(tenant=TENANT_A_ID, chat=other, role="user", content="another chat",
                        created=5)
    send_message(api, chat, "current")
    assert last_request(mock_provider).conversation_texts() == [
        "user:kept question", "assistant:kept answer", "user:current"]


def test_the_same_history_yields_the_same_request_context(api, seed, mock_provider):
    seen = []
    for _ in range(2):
        chat = new_chat(seed)
        seed_history(seed, chat, 5)
        send_message(api, chat, "current")
        seen.append(last_request(mock_provider).conversation_texts())
    assert seen[0] == seen[1]


def test_the_system_prompt_is_the_catalog_prompt_of_the_chat_model(api, seed, mock_provider):
    chat = new_chat(seed, model=catalog.STANDARD_ID)
    send_message(api, chat, "hello")
    assert catalog.SECRET_SYSTEM_PROMPT in last_request(mock_provider).system_text()


# --------------------------------------------------------------------------- #
# truncation under a small context window
# --------------------------------------------------------------------------- #


def run_fit(restricted, model, current=catalog.FIT_CURRENT_MESSAGE):
    api = restricted.api()
    try:
        seed = restricted.seeder()
        chat = new_chat(seed, model=model)
        seed_three_turns(seed, chat)
        sent = send_message(api, chat, current)
        assert sent.terminal.event == "done", sent.events
        return seed, chat, api
    finally:
        api.close()


def test_the_oldest_whole_turns_are_dropped_when_the_budget_is_exceeded(restricted):
    # room for three history messages: A2 Q3 A3 fit, the leading A2 is dropped as well
    seed, chat, _ = run_fit(restricted, catalog.FIT_THREE_ID)
    assert shape(restricted.mock) == ["user:Q3", "assistant:A3", "user:no"]
    assert catalog.FIT_SYSTEM_PROMPT in last_request(restricted.mock).system_text()
    # nothing is deleted from the conversation itself
    assert len(seed.ordered_messages(chat)) == 6 + 2


def test_a_larger_budget_keeps_more_turns(restricted):
    run_fit(restricted, catalog.FIT_FOUR_ID)
    assert shape(restricted.mock) == [
        "user:Q2", "assistant:A2", "user:Q3", "assistant:A3", "user:no"]
    run_fit(restricted, catalog.FIT_SIX_ID)
    assert len(shape(restricted.mock)) == 7, "everything fits"


def test_only_the_mandatory_items_are_sent_when_nothing_else_fits(restricted):
    run_fit(restricted, catalog.FIT_ZERO_ID)
    assert shape(restricted.mock) == ["user:no"]
    assert catalog.FIT_SYSTEM_PROMPT in last_request(restricted.mock).system_text()


def test_truncation_is_deterministic(restricted):
    first = run_fit(restricted, catalog.FIT_THREE_ID)
    shape_one = last_request(restricted.mock).conversation_texts()
    second = run_fit(restricted, catalog.FIT_THREE_ID)
    assert last_request(restricted.mock).conversation_texts() == shape_one
    assert first[1] != second[1]
