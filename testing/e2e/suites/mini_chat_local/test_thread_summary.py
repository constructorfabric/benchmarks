"""Thread summary black-box: trigger, outbox-driven generation, storage, context inclusion, failure
and retry, mutation-driven invalidation, the system usage event.

A completed turn whose context was truncated or reached 80 % of the input budget enqueues one
durable thread-summary message (queue ``mini-chat.thread_summary``); the worker calls the summary
model through the same OAGW provider, stores the summary in ``thread_summaries``, flags the covered
messages ``is_compressed`` and emits a ``system_task`` usage event; the next turn sends the summary
instead of the covered messages and reports ``thread_summary_applied``.

Evidence: HTTP / SSE, the SQLite rows (``thread_summaries``, ``messages.is_compressed``), the
sequence counters of ``toolkit_outbox_partitions`` and the dead letters, the requests the mock
provider received (summary requests carry ``metadata.request_type = "summary"``) and the log lines
of the bundled usage plugin. The Rust tests (``thread_summary_*.rs``) pin the same rules in-process
with exact payloads, prompts and boundaries.

Source: DESIGN 3.6 "Thread Summary Update" (trigger, durable scheduling, request format, response
parsing, CAS commit, failure handling), "Thread Summary - Stable Range and Commit Invariant",
"Summary Interaction on Turn Mutation", 3.2 "System Task Attribution Rules", 4 "Context Plan
Assembly and Truncation" (summary delivery format), 3.3 SSE (``thread_summary_applied``), B.5.5,
B.9.3, B.9.4.
"""

from __future__ import annotations

import re
import time

import pytest

from . import catalog
from .mock_openai_provider import (
    error_response,
    nameless_frames,
    sse_frames,
    sse_summary,
    sse_text,
    text_stream_events,
)
from .mutation_support import delete_turn, mutate_stream
from .summary_support import (
    CHAT_ANSWER,
    CHAT_MODEL_ID,
    CLOSING,
    DEFAULT_PROMPT_START,
    DEFAULT_SUMMARY_MODEL_ID,
    OPENING_FRESH,
    OPENING_MERGE,
    PREAMBLE,
    SUMMARY_MAX_OUTPUT,
    SUMMARY_MODEL_ID,
    SUMMARY_QUEUE,
    SYSTEM_SUBJECT_ID,
    TASK_TYPE,
    Summ,
    SummarySeeder,
    entries,
    ids_of,
    is_summary_item,
    labels,
    last_chat_request,
    provider_model,
    summary_entry,
    summary_item_text,
    summary_overrides,
    summary_requests_of,
    system_usage,
    wait_summary,
)
from .support import (
    TENANT_A_ID,
    list_all,
    send_message,
    stored_ts,
    uuid_text,
    wait_for,
)

NIL = "00000000-0000-0000-0000-000000000000"


# --------------------------------------------------------------------------- #
# stacks and fixtures
# --------------------------------------------------------------------------- #


def settle(mock, quiet: float = 1.5, timeout: float = 45.0) -> None:
    """Wait until no summary request arrived for ``quiet`` seconds (pending retries done)."""
    last, since, deadline = -1, time.monotonic(), time.monotonic() + timeout
    while time.monotonic() < deadline:
        n = len(mock.summary_requests())
        now = time.monotonic()
        if n != last:
            last, since = n, now
        elif now - since >= quiet:
            return
        time.sleep(0.1)


def view(stack):
    stack.mock.reset()
    api = stack.api()
    try:
        yield Summ(stack=stack, api=api, seed=SummarySeeder(stack.server.db_path))
    finally:
        stack.mock.release()
        settle(stack.mock)
        api.close()


@pytest.fixture(scope="session")
def summary_stack(stack_factory):
    """Summary model ``mock-summary``, ``max_attempts = 2``, the default 80 % threshold."""
    return stack_factory("summary", summary_overrides(worker={"max_attempts": 2}))


@pytest.fixture()
def sm(summary_stack):
    yield from view(summary_stack)


@pytest.fixture(scope="session")
def summary_off_stack(stack_factory):
    return stack_factory("summary_off", summary_overrides(worker={"enabled": False}))


@pytest.fixture()
def sm_off(summary_off_stack):
    yield from view(summary_off_stack)


@pytest.fixture(scope="session")
def summary_unavailable_stack(stack_factory):
    """The summary model is the disabled catalog entry ``mock-disabled``."""
    return stack_factory("summary_unavailable",
                         summary_overrides(summary_model=catalog.DISABLED_ID))


@pytest.fixture()
def sm_unavailable(summary_unavailable_stack):
    yield from view(summary_unavailable_stack)


@pytest.fixture(scope="session")
def summary_default_stack(stack_factory):
    """``summary_model_id`` empty: the built-in ``gpt-4.1-mini`` (a catalog entry of this stack)."""
    return stack_factory("summary_default", summary_overrides(
        summary_model="", extra_models=[summary_entry(DEFAULT_SUMMARY_MODEL_ID)]))


@pytest.fixture()
def sm_default(summary_default_stack):
    yield from view(summary_default_stack)


def say(sm, chat, text="ping"):
    sent = send_message(sm.api, chat, text)
    assert sent.terminal.event == "done", sent.events
    return sent


def summarized_chat(sm, text="Flow summary text", turns=3, **reply):
    """``turns`` seeded 1000-byte turns, then the real turn ``ping`` whose completion triggers the
    summary of the seeded turns: returns ``(chat, seeded, ping_request_id)`` once it is committed."""
    chat = sm.new_chat()
    seeded = sm.seed.history(chat, turns)
    sm.mock.script_summary(sse_summary(text, **reply))
    sent = say(sm, chat, "ping")
    row = wait_summary(sm.seed, chat, text)
    assert uuid_text(row["summarized_up_to_message_id"]) == seeded[-1]["assistant_message"]
    return chat, seeded, sent.request_id


# --------------------------------------------------------------------------- #
# registration and trigger
# --------------------------------------------------------------------------- #


def test_the_thread_summary_queue_is_registered_at_startup(sm):
    assert sm.db().outbox_queues().get(SUMMARY_QUEUE) == 4, sm.db().outbox_queues()


def test_a_turn_over_the_threshold_enqueues_one_task_and_commits_a_summary(sm):
    chat = sm.new_chat()
    seeded = sm.seed.history(chat, 2)
    sm.mock.script_summary(sse_summary("The user asked about alpha.", input_tokens=900,
                                       output_tokens=120, reasoning_tokens=40))
    queued, dead = sm.queued(), sm.dead_letters()

    sent = say(sm, chat, "ping")

    row = wait_summary(sm.seed, chat)
    assert row["summary_text"].strip() == "The user asked about alpha."
    assert uuid_text(row["tenant_id"]) == TENANT_A_ID
    assert uuid_text(row["summarized_up_to_message_id"]) == seeded[1]["assistant_message"]
    assert row["summarized_up_to_created_at"] == stored_ts(21)
    assert row["token_estimate"] == 80, "output tokens minus reasoning tokens"
    # exactly the summarized range is compressed; the causing turn is not
    assert sm.seed.compressed_ids(chat) == ids_of(seeded)
    assert sent.request_id not in sm.seed.compressed_ids(chat)
    time.sleep(1.0)
    assert sm.queued() == queued + 1, "one durable work item per trigger"
    requests = summary_requests_of(sm.mock, chat)
    assert len(requests) == 1
    # the summary call is a plain request / response (DESIGN 3.6): not streamed
    # (the Accept header does not survive the gateway; the adapter unit tests pin it)
    assert requests[0].json["stream"] is False, requests[0].json
    assert requests[0].json["metadata"]["request_type"] == "summary"
    assert sm.dead_letters() == dead
    assert len(sm.seed.thread_summaries(chat)) == 1


def test_a_turn_below_the_threshold_enqueues_nothing(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 1)
    queued = sm.queued()
    say(sm, chat, "ping")
    time.sleep(1.2)
    assert sm.queued() == queued
    assert summary_requests_of(sm.mock, chat) == []
    assert sm.seed.thread_summaries(chat) == []


def test_a_truncated_context_triggers_as_well(sm):
    chat = sm.new_chat()
    seeded = sm.seed.history(chat, 3)  # 6 x 385 + 230 tokens: the oldest turn is dropped
    sent = say(sm, chat, "ping")
    assert last_chat_request(sm.mock).conversation_texts()[0].startswith("user:Q2")
    row = wait_summary(sm.seed, chat)
    assert uuid_text(row["summarized_up_to_message_id"]) == seeded[2]["assistant_message"]
    assert sm.seed.compressed_ids(chat) == ids_of(seeded), "also what was dropped from the context"
    assert sent.request_id


def test_the_triggering_turn_is_an_ordinary_turn(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.set_summary_default(error_response(500, "summary provider down"))
    sent = say(sm, chat, "ping")
    assert sent.names_no_ping()[0] == "stream_started"
    assert sent.names_no_ping()[-1] == "done"
    assert "thread_summary_applied" not in sent.started
    items = last_chat_request(sm.mock).conversation_texts()
    assert len(items) == 5 and not any(is_summary_item(i) for i in items), items


def test_the_turn_does_not_wait_for_the_summary(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.script_summary(sse_summary("slow summary", pause_after_deltas=1))
    sent = say(sm, chat, "ping")
    assert sent.terminal.event == "done"
    sm.mock.wait_paused(1)
    assert sm.seed.thread_summaries(chat) == [], "the summary is not there yet"
    sm.mock.release()
    wait_summary(sm.seed, chat, "slow summary")


# --------------------------------------------------------------------------- #
# the summary request
# --------------------------------------------------------------------------- #


def test_the_summary_request_is_a_system_request_to_the_summary_model(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    say(sm, chat, "ping")
    wait_summary(sm.seed, chat)

    (request,) = summary_requests_of(sm.mock, chat)
    body = request.json
    assert request.path == "/v1/responses"
    assert body["model"] == provider_model(SUMMARY_MODEL_ID)
    assert body["max_output_tokens"] == SUMMARY_MAX_OUTPUT
    assert request.tools() == []
    assert request.metadata["request_type"] == "summary"
    assert request.metadata["feature"] == "none"
    assert request.metadata["chat_id"] == chat
    assert request.metadata["tenant_id"] == TENANT_A_ID
    assert request.metadata["user_id"] == SYSTEM_SUBJECT_ID
    assert body["user"] == TENANT_A_ID.replace("-", "") + SYSTEM_SUBJECT_ID.replace("-", "")
    assert request.system_text().startswith(DEFAULT_PROMPT_START)
    assert len(sm.mock.chat_requests()) == 1, "only the ping turn used the chat model"


def test_the_prompt_lists_the_messages_in_order_and_asks_for_the_two_blocks(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    say(sm, chat, "ping")
    wait_summary(sm.seed, chat)

    prompt = summary_requests_of(sm.mock, chat)[0].prompt()
    assert prompt.startswith(OPENING_FRESH)
    assert prompt.rstrip().endswith(CLOSING)
    assert labels(prompt) == ["Q1", "A1", "Q2", "A2"]
    assert "<existing_summary>" not in prompt
    for needle in ("<analysis>", "<summary>", "Conversation Purpose", "Current Topic"):
        assert needle in prompt


def test_an_event_stream_answer_to_the_summary_request_is_still_accepted(sm):
    # the response Content-Type decides: a provider that streams anyway is not a failure
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    events = text_stream_events(["<analysis>x</analysis>\n<summary>Streamed anyway.</summary>"])
    sm.mock.script_summary(sse_frames(*nameless_frames(events)))
    say(sm, chat, "ping")
    row = wait_summary(sm.seed, chat)
    assert row["summary_text"].strip() == "Streamed anyway."
    assert summary_requests_of(sm.mock, chat)[0].json["stream"] is False


def test_the_answer_is_parsed_into_the_stored_summary(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.script_summary(sse_text([
        "<analysis>thinking it over</analysis>\n<summary>Para one.\n\n\n\n", "Para two.</summary>"]))
    say(sm, chat, "ping")
    row = wait_summary(sm.seed, chat)
    assert row["summary_text"].strip() == "Para one.\n\nPara two."


# --------------------------------------------------------------------------- #
# system task: usage event, no turn, no quota
# --------------------------------------------------------------------------- #


def test_the_summary_emits_one_system_task_usage_event(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.script_summary(sse_summary("text", input_tokens=500, output_tokens=80))
    say(sm, chat, "ping")
    wait_summary(sm.seed, chat)

    events = wait_for(lambda: system_usage(sm.server, chat) or None, "the system usage event")
    time.sleep(1.0)
    events = system_usage(sm.server, chat)
    assert len(events) == 1, events
    event = events[0]
    assert event["tenant_id"] == TENANT_A_ID
    assert event["user_id"] == NIL, "no user is billed"
    assert event["turn_id"] == NIL, "there is no turn"
    assert event["effective_model"] == SUMMARY_MODEL_ID
    assert event["selected_model"] == SUMMARY_MODEL_ID
    assert event["terminal_state"] == "completed"
    assert event["billing_outcome"] == "system_task"
    assert event["settlement_method"] == "none"
    assert event["actual_credits_micro"] == "0"
    system_request_id = event["request_id"]
    assert re.fullmatch(r"[0-9a-f-]{36}", system_request_id)
    assert event["dedupe_key"] == (
        f"{TENANT_A_ID.replace('-', '')}/{TASK_TYPE}/{system_request_id.replace('-', '')}")


def test_a_summary_creates_no_turn_and_debits_no_quota(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    say(sm, chat, "ping")
    quota_after_turn = [(r["bucket"], r["period_type"], r["spent_credits_micro"],
                         r["reserved_credits_micro"], r["calls"]) for r in sm.seed.quota_rows()]
    wait_summary(sm.seed, chat)
    wait_for(lambda: system_usage(sm.server, chat) or None, "the system usage event")

    assert len(sm.seed.turns_of(chat)) == 3, "two seeded turns and ping: none for the summary"
    assert [(r["bucket"], r["period_type"], r["spent_credits_micro"], r["reserved_credits_micro"],
             r["calls"]) for r in sm.seed.quota_rows()] == quota_after_turn
    assert len(sm.seed.ordered_messages(chat)) == 6, "no message is created"


# --------------------------------------------------------------------------- #
# the next turns
# --------------------------------------------------------------------------- #


def test_the_next_turn_carries_the_summary_and_only_the_recent_messages(sm):
    chat, seeded, ping = summarized_chat(sm, "Alpha was discussed.", turns=2, output_tokens=120)
    row = sm.seed.thread_summaries(chat)[0]

    sent = say(sm, chat, "second")

    assert sent.started["thread_summary_applied"] == {"token_estimate": row["token_estimate"]}
    items = last_chat_request(sm.mock).conversation_texts()
    assert is_summary_item(items[0]), items
    assert summary_item_text(items[0]) == "Alpha was discussed."
    assert items[1:] == ["user:ping", f"assistant:{CHAT_ANSWER}", "user:second"]
    assert not any("Q1" in i or "A2" in i for i in items), "covered messages are not sent"
    # a user message, not part of the system prompt
    request = last_chat_request(sm.mock)
    assert request.json["input"][0]["role"] == "user"
    assert "Alpha was discussed." not in request.system_text()

    # the small context triggers nothing more
    time.sleep(1.2)
    assert len(summary_requests_of(sm.mock, chat)) == 1
    assert len(sm.seed.thread_summaries(chat)) == 1


def test_a_chat_without_a_summary_reports_no_thread_summary_applied(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 1)
    sent = say(sm, chat, "hello")
    assert "thread_summary_applied" not in sent.started
    items = last_chat_request(sm.mock).conversation_texts()
    assert not any(is_summary_item(i) for i in items)


def test_the_next_summary_merges_the_previous_one_with_the_new_messages(sm):
    chat, seeded, _ping = summarized_chat(sm, "First summary", turns=2)
    say(sm, chat, "second")
    time.sleep(1.0)
    assert len(summary_requests_of(sm.mock, chat)) == 1

    # a large message truncates the context: the summary is stale, the urgent trigger fires
    sm.mock.script_summary(sse_summary("Merged summary"))
    say(sm, chat, "b" * 5000)

    row = wait_summary(sm.seed, chat, "Merged summary")
    live = sm.seed.live_ids(chat)  # q1 a1 q2 a2 ping answer second answer big answer
    assert len(live) == 10
    assert uuid_text(row["summarized_up_to_message_id"]) == live[7], "the message before the big turn"
    prompt = summary_requests_of(sm.mock, chat)[1].prompt()
    assert prompt.startswith(OPENING_MERGE)
    assert "<existing_summary>" in prompt and "First summary" in prompt
    assert entries(prompt) == ["User: ping", f"Assistant: {CHAT_ANSWER}", "User: second",
                               f"Assistant: {CHAT_ANSWER}"], "only what the first summary lacks"
    assert sm.seed.compressed_ids(chat) == sorted(ids_of(seeded) + live[4:8])
    assert len(sm.seed.thread_summaries(chat)) == 1, "one row per chat"


def test_summarized_messages_stay_visible_in_the_message_list(sm):
    chat, seeded, _ping = summarized_chat(sm, "Visible anyway", turns=2)
    items = list_all(sm.api, f"/v1/chats/{chat}/messages")
    ids = [m["id"] for m in items]
    assert len(ids) == 6
    assert set(ids_of(seeded)) <= set(ids), "compressed messages remain for the UI"
    assert sm.seed.compressed_ids(chat) == ids_of(seeded)


def test_summaries_of_different_chats_do_not_mix(sm):
    chats = [sm.new_chat(), sm.new_chat()]
    turns = [sm.seed.history(c, 2) for c in chats]
    for chat in chats:
        say(sm, chat, "ping")
    for chat, seeded in zip(chats, turns):
        row = wait_summary(sm.seed, chat)
        assert uuid_text(row["summarized_up_to_message_id"]) == seeded[1]["assistant_message"]
        assert sm.seed.compressed_ids(chat) == ids_of(seeded)


# --------------------------------------------------------------------------- #
# failures
# --------------------------------------------------------------------------- #


def test_a_failing_provider_is_retried_then_dead_lettered_and_the_chat_keeps_working(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.set_summary_default(error_response(500, "summary provider down"))
    dead = sm.dead_letters()

    say(sm, chat, "ping")

    # max_attempts = 2: the delivery that would retry on its last attempt is rejected
    sm.mock.wait_for_summary_requests(2)
    wait_for(lambda: sm.dead_letters() == dead + 1 or None, "the dead-lettered message")
    time.sleep(1.2)
    assert len(summary_requests_of(sm.mock, chat)) == 2
    assert sm.seed.thread_summaries(chat) == []
    assert sm.seed.compressed_ids(chat) == []
    assert system_usage(sm.server, chat) == [], "no usage event for a failed call"

    # the chat keeps working with the uncompressed history
    sent = say(sm, chat, "next")
    assert "thread_summary_applied" not in sent.started
    items = last_chat_request(sm.mock).conversation_texts()
    assert not any(is_summary_item(i) for i in items), items
    assert items[-1] == "user:next" and f"assistant:{CHAT_ANSWER}" in items

    # the new turn is over the budget again: a wider work item is scheduled and fails the same way
    wait_for(lambda: sm.dead_letters() == dead + 2 or None, "the second dead-lettered message")
    assert len(summary_requests_of(sm.mock, chat)) == 4


def test_a_provider_that_recovers_is_retried_into_a_single_summary(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.script_summary(error_response(503, "unavailable"), sse_summary("Second try"))
    dead = sm.dead_letters()

    say(sm, chat, "ping")

    wait_summary(sm.seed, chat, "Second try")
    assert len(summary_requests_of(sm.mock, chat)) == 2
    wait_for(lambda: system_usage(sm.server, chat) or None, "the system usage event")
    time.sleep(1.0)
    assert len(system_usage(sm.server, chat)) == 1
    assert sm.dead_letters() == dead


def test_an_answer_without_a_summary_is_retried_like_a_failure(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.set_summary_default(sse_text(["<analysis>nothing more to say</analysis>"]))
    dead = sm.dead_letters()

    say(sm, chat, "ping")

    wait_for(lambda: sm.dead_letters() == dead + 1 or None, "the dead-lettered message")
    assert len(summary_requests_of(sm.mock, chat)) == 2
    assert sm.seed.thread_summaries(chat) == []


def test_a_chat_deleted_while_the_provider_works_gets_no_summary(sm):
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    sm.mock.script_summary(sse_summary("too late", pause_after_deltas=1))
    say(sm, chat, "ping")
    sm.mock.wait_paused(1)
    assert sm.api.delete(f"/v1/chats/{chat}").status_code == 204
    dead = sm.dead_letters()

    sm.mock.release()
    time.sleep(1.5)

    assert sm.seed.thread_summaries(chat) == []
    assert sm.dead_letters() == dead, "the message is skipped, not rejected"
    assert system_usage(sm.server, chat) == []


# --------------------------------------------------------------------------- #
# mutation-driven invalidation
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("op", ["retry", "edit", "delete"])
def test_a_mutation_of_the_covered_turn_drops_the_summary_and_the_next_summary_has_no_base(sm, op):
    chat, seeded, ping = summarized_chat(sm, "Covers turns one to three")
    old = sm.seed.thread_summaries(chat)[0]
    assert len(sm.seed.compressed_ids(chat)) == 6

    # the summary stops before `ping`: deleting `ping` keeps the summary and its flags
    assert delete_turn(sm.api, chat, ping).status_code == 204
    assert [uuid_text(r["id"]) for r in sm.seed.thread_summaries(chat)] == [uuid_text(old["id"])]
    assert len(sm.seed.compressed_ids(chat)) == 6

    # turn 3 is the latest again and the summary covers it: the mutation drops the summary
    sm.mock.script_summary(sse_summary("Rebuilt summary"))
    third = seeded[2]["request_id"]
    if op == "delete":
        assert delete_turn(sm.api, chat, third).status_code == 204
        say(sm, chat, "next")
    else:
        sent = mutate_stream(sm.api, op, chat, third)
        assert sent.terminal.event == "done", sent.events

    # until the next summary the context is the uncompressed history, bounded by the budget
    context = last_chat_request(sm.mock).conversation_texts()
    assert not any(is_summary_item(i) for i in context), context
    assert any(i.startswith("assistant:A2") for i in context), \
        f"formerly summarized turns are part of the context again: {context}"
    assert not any(i.startswith("user:ping") or i.startswith("assistant:Hello") for i in context)

    # the next trigger builds a new summary with no base over the live messages
    row = wait_summary(sm.seed, chat, "Rebuilt summary")
    assert uuid_text(row["summarized_up_to_message_id"]) == seeded[1]["assistant_message"]
    assert uuid_text(row["id"]) != uuid_text(old["id"])
    prompt = summary_requests_of(sm.mock, chat)[1].prompt()
    assert prompt.startswith(OPENING_FRESH) and "<existing_summary>" not in prompt
    assert labels(prompt) == ["Q1", "A1", "Q2", "A2"], \
        "deleted messages and the replacement turn are not summarized"
    assert sm.seed.compressed_ids(chat) == ids_of(seeded[:2])
    assert len(sm.seed.thread_summaries(chat)) == 1


def test_a_retry_of_the_causing_turn_keeps_the_summary_in_the_context(sm):
    chat, seeded, ping = summarized_chat(sm, "Covers turns one to three")
    old = sm.seed.thread_summaries(chat)[0]

    sent = mutate_stream(sm.api, "retry", chat, ping)

    assert sent.terminal.event == "done"
    items = last_chat_request(sm.mock).conversation_texts()
    assert len(items) == 2 and is_summary_item(items[0]), items
    assert summary_item_text(items[0]) == "Covers turns one to three"
    assert items[1] == "user:ping"
    assert "thread_summary_applied" in sent.started
    assert [uuid_text(r["id"]) for r in sm.seed.thread_summaries(chat)] == [uuid_text(old["id"])]
    assert len(sm.seed.compressed_ids(chat)) == 6
    time.sleep(1.0)
    assert len(summary_requests_of(sm.mock, chat)) == 1


# --------------------------------------------------------------------------- #
# configuration: disabled worker, unavailable model, built-in default model
# --------------------------------------------------------------------------- #


def test_a_disabled_worker_never_enqueues(sm_off):
    chat = sm_off.new_chat()
    sm_off.seed.history(chat, 3)  # truncated and far above the threshold
    queued = sm_off.queued()
    say(sm_off, chat, "ping")
    time.sleep(1.5)
    assert sm_off.queued() == queued
    assert sm_off.mock.summary_requests() == []
    assert sm_off.seed.thread_summaries(chat) == []


def test_a_disabled_summary_model_dead_letters_at_once(sm_unavailable):
    sm = sm_unavailable
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    dead = sm.dead_letters()

    sent = say(sm, chat, "ping")  # the trigger does not check the model: the turn works

    assert sent.terminal.event == "done"
    wait_for(lambda: sm.dead_letters() == dead + 1 or None, "the dead-lettered message")
    time.sleep(1.0)
    assert sm.mock.summary_requests() == [], "rejected before any provider call, no retry"
    assert sm.seed.thread_summaries(chat) == []


def test_an_empty_summary_model_id_means_gpt_4_1_mini(sm_default):
    sm = sm_default
    chat = sm.new_chat()
    sm.seed.history(chat, 2)
    say(sm, chat, "ping")
    wait_summary(sm.seed, chat)
    (request,) = summary_requests_of(sm.mock, chat)
    assert request.json["model"] == provider_model(DEFAULT_SUMMARY_MODEL_ID)
    events = wait_for(lambda: system_usage(sm.server, chat) or None, "the system usage event")
    assert events[0]["effective_model"] == DEFAULT_SUMMARY_MODEL_ID
