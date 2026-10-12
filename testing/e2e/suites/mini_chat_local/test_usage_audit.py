"""Usage and audit publication through the shared outbox (black-box).

One usage message and one audit message are enqueued per finalized turn, in the finalization
transaction, on the queues ``outbox.queue_name`` (``mini-chat.usage_snapshot``) and
``outbox.audit_queue_name`` (``mini-chat.audit``); the usage handler delivers them to the model
policy plugin (``publish_usage``) and the audit handler to the audit plugin (``emit``).

Evidence: the sequence counters of ``toolkit_outbox_partitions`` (messages ever enqueued per queue;
they survive the outbox vacuum) and the log lines of the bundled static plugins, which log what they
receive (``usage event received by the static model policy plugin`` with the fields of the
``UsageEvent``; ``mini-chat audit event`` with ``event_type`` and ``request_id``). The payload
contents are pinned exactly in the Rust tests (``usage_outbox.rs``, ``audit_outbox.rs``).

Source: DESIGN 5.6 (queue, ``UsageEvent`` payload, ``dedupe_key``), 5.7 (settlement transaction
invariant, exactly one event per turn, replay is side-effect free), 5.8 / 5.9, 3.2 "Audit plugin
and audit outbox", 3.9, B.9.3, ADR-0009.
"""

from __future__ import annotations

import re
import time
import uuid

import pytest

from . import catalog
from .mock_openai_provider import ProviderResponse, error_response, failed_stream_events, sse_text
from .support import (
    TENANT_A_ID,
    TENANT_IDS,
    USER_A_ID,
    USER_IDS,
    audit_log,
    dedupe_key,
    open_stream,
    reject_message,
    send_message,
    usage_log,
    uuid_text,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
USAGE_QUEUE = "mini-chat.usage_snapshot"
AUDIT_QUEUE = "mini-chat.audit"
RESPONSES = ("POST", "/v1/responses")
MSG = "hello"


@pytest.fixture()
def qapi(quota):
    client = quota.api()
    yield client
    client.close()


def new_chat(stack, model=PREMIUM, user="a"):
    return stack.seeder().insert_chat(tenant=TENANT_IDS[user], user=USER_IDS[user], model=model)


def queue_counts(stack):
    db = stack.db()
    return db.outbox_sequenced(USAGE_QUEUE), db.outbox_sequenced(AUDIT_QUEUE)


def wait_enqueued(stack, before, usage=1, audit=1):
    """Wait until ``usage`` / ``audit`` more messages were enqueued than at ``before``."""
    def reached():
        now = queue_counts(stack)
        return now == (before[0] + usage, before[1] + audit) or None
    wait_for(reached, f"{usage} usage / {audit} audit message(s) on the outbox queues")


def usage_event_of(stack, request_id):
    """The usage event logged by the plugin for ``request_id`` (waits for the delivery)."""
    def find():
        found = [e for e in usage_log(stack.server) if e.get("request_id") == request_id]
        return found or None
    return wait_for(find, f"the usage event of {request_id} to be delivered")


def audit_events_of(stack, request_id):
    def find():
        found = [e for e in audit_log(stack.server) if e.get("request_id") == request_id]
        return found or None
    return wait_for(find, f"the audit event of {request_id} to be delivered")


def turn_id_of(stack, chat, request_id):
    return uuid_text(stack.db().turn(chat, request_id)["id"])


# --------------------------------------------------------------------------- #
# the pipeline is wired
# --------------------------------------------------------------------------- #


def test_the_usage_and_audit_queues_are_registered_at_startup(quota):
    queues = quota.db().outbox_queues()
    for queue in (USAGE_QUEUE, AUDIT_QUEUE, "mini-chat.chat_cleanup"):
        assert queues.get(queue) == 4, f"{queue} registered with 4 partitions: {queues}"


# --------------------------------------------------------------------------- #
# exactly one message of each kind per finalized turn
# --------------------------------------------------------------------------- #


def test_a_completed_turn_enqueues_one_usage_and_one_audit_message(quota, qapi):
    chat = new_chat(quota)
    before = queue_counts(quota)
    sent = send_message(qapi, chat, MSG)
    assert sent.names_no_ping()[-1] == "done"
    wait_enqueued(quota, before)
    # nothing more arrives later
    usage_event_of(quota, sent.request_id)
    assert queue_counts(quota) == (before[0] + 1, before[1] + 1)
    assert quota.db().dead_letter_count() == 0


def test_every_finalized_turn_enqueues_exactly_one_of_each(quota, qapi):
    chat = new_chat(quota)
    before = queue_counts(quota)
    sent = [send_message(qapi, chat, f"turn {i}") for i in range(3)]
    quota.mock.script(*RESPONSES, error_response(500, "boom"))
    sent.append(send_message(qapi, chat, "failing"))

    wait_enqueued(quota, before, usage=4, audit=4)
    keys = []
    for s in sent:
        events = usage_event_of(quota, s.request_id)
        assert len(events) == 1, f"one delivery of turn {s.request_id}"
        keys.append(events[0]["dedupe_key"])
    assert len(set(keys)) == 4, "distinct dedupe keys"
    assert queue_counts(quota) == (before[0] + 4, before[1] + 4)


def test_nothing_is_enqueued_for_replays_and_rejections(quota, qapi):
    chat = new_chat(quota)
    rid = str(uuid.uuid4())
    before = queue_counts(quota)
    first = send_message(qapi, chat, MSG, request_id=rid)
    assert first.names_no_ping()[-1] == "done"
    wait_enqueued(quota, before)

    send_message(qapi, chat, MSG, request_id=rid)  # replay
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=catalog.QUOTA_LIMITS["total"]["daily"])
    assert reject_message(qapi, chat, {"content": "again"}).status_code == 429
    assert reject_message(qapi, chat, {"content": " "}).status_code == 400

    time.sleep(1.5)
    assert queue_counts(quota) == (before[0] + 1, before[1] + 1)


# --------------------------------------------------------------------------- #
# delivery to the plugins and the content of the usage event
# --------------------------------------------------------------------------- #


def test_the_usage_event_is_delivered_once_with_the_turn_identities(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], input_tokens=800, output_tokens=200))
    sent = send_message(qapi, chat, MSG)

    events = usage_event_of(quota, sent.request_id)
    assert len(events) == 1, "delivered exactly once"
    event = events[0]
    turn = turn_id_of(quota, chat, sent.request_id)
    assert event["dedupe_key"] == dedupe_key(TENANT_A_ID, turn, sent.request_id)
    assert re.fullmatch(r"[0-9a-f]{32}/[0-9a-f]{32}/[0-9a-f]{32}", event["dedupe_key"])
    assert event["tenant_id"] == TENANT_A_ID
    assert event["user_id"] == USER_A_ID
    assert event["chat_id"] == chat
    assert event["turn_id"] == turn
    assert event["effective_model"] == PREMIUM and event["selected_model"] == PREMIUM
    assert event["terminal_state"] == "completed"
    assert event["billing_outcome"] == "completed"
    assert event["settlement_method"] == "actual"
    assert int(event["actual_credits_micro"]) == catalog.model_credits(PREMIUM, 800, 200)
    assert int(event["policy_version_applied"]) == 1
    assert (int(event["web_search_calls"]), int(event["code_interpreter_calls"]),
            int(event["file_search_calls"])) == (0, 0, 0)


def outbox_usage_of(stack, request_id, event_type=None):
    """The ``usage`` object of the outbox payload of ``request_id`` (the usage event, or the
    audit event of ``event_type``)."""
    def find():
        for payload in stack.seeder().outbox_payloads():
            if payload.get("request_id") != request_id or "usage" not in payload:
                continue
            if payload.get("event_type") == event_type or (
                    event_type is None and "event_type" not in payload):
                return payload["usage"]
        return None
    return wait_for(find, f"the outbox payload of {request_id} to carry a usage object")


def assistant_row(stack, chat):
    rows = [r for r in stack.db().ordered_messages(chat) if r["role"] == "assistant"]
    assert len(rows) == 1
    return rows[0]


def test_the_provider_cache_and_reasoning_breakdown_reaches_the_row_and_both_events(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(
        ["a", "b"], input_tokens=500, output_tokens=40, cached_tokens=300, reasoning_tokens=7))
    sent = send_message(qapi, chat, MSG)

    row = assistant_row(quota, chat)
    assert (row["input_tokens"], row["output_tokens"]) == (500, 40)
    assert (row["cache_read_input_tokens"], row["cache_write_input_tokens"],
            row["reasoning_tokens"]) == (300, 0, 7)

    expected = {"input_tokens": 500, "output_tokens": 40, "cache_read_input_tokens": 300,
                "cache_write_input_tokens": 0, "reasoning_tokens": 7}
    assert outbox_usage_of(quota, sent.request_id) == expected
    assert outbox_usage_of(quota, sent.request_id, "turn_completed") == expected
    event = usage_event_of(quota, sent.request_id)[0]
    assert int(event["actual_credits_micro"]) == catalog.model_credits(PREMIUM, 500, 40), \
        "credits come from the totals; the breakdown is a subset"


def test_a_failed_turn_with_usage_carries_the_breakdown(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, ProviderResponse(sse_events=failed_stream_events(
        "server_error", "overloaded", partial_chunks=["p"], usage=(700, 40),
        cached_tokens=256, reasoning_tokens=9)))
    sent = send_message(qapi, chat, MSG)
    assert outbox_usage_of(quota, sent.request_id) == {
        "input_tokens": 700, "output_tokens": 40, "cache_read_input_tokens": 256,
        "cache_write_input_tokens": 0, "reasoning_tokens": 9}


def test_a_downgraded_turn_reports_both_models(quota, qapi):
    chat = new_chat(quota)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="tier:premium",
                                period="daily", spent=catalog.QUOTA_LIMITS["tier:premium"]["daily"])
    quota.mock.script(*RESPONSES, sse_text(["a"], input_tokens=1000, output_tokens=100))
    sent = send_message(qapi, chat, MSG)
    assert sent.done["effective_model"] == STANDARD
    event = usage_event_of(quota, sent.request_id)[0]
    assert event["selected_model"] == PREMIUM
    assert event["effective_model"] == STANDARD
    assert int(event["actual_credits_micro"]) == catalog.model_credits(STANDARD, 1000, 100)


def test_a_failed_turn_without_usage_is_published_failed_estimated(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, error_response(500, "boom"))
    sent = send_message(qapi, chat, MSG)
    event = usage_event_of(quota, sent.request_id)[0]
    assert event["terminal_state"] == "failed"
    assert event["billing_outcome"] == "failed"
    assert event["settlement_method"] == "estimated", \
        "the provider request started: never released"
    assert int(event["actual_credits_micro"]) == catalog.estimated_charge(PREMIUM, MSG)


def test_a_failed_turn_with_usage_is_published_failed_actual(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, ProviderResponse(sse_events=failed_stream_events(
        "server_error", "overloaded", partial_chunks=["p"], usage=(700, 40))))
    sent = send_message(qapi, chat, MSG)
    event = usage_event_of(quota, sent.request_id)[0]
    assert event["settlement_method"] == "actual"
    assert int(event["actual_credits_micro"]) == catalog.model_credits(PREMIUM, 700, 40)


def test_a_cancelled_turn_is_published_aborted_estimated(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], pause_after_deltas=1))
    live = open_stream(qapi, chat, {"content": MSG})
    rid = live.expect("stream_started").data["request_id"]
    live.next_non_ping()
    quota.mock.wait_paused()
    live.close()
    quota.mock.wait_client_disconnected()

    event = usage_event_of(quota, rid)[0]
    assert event["terminal_state"] == "cancelled"
    assert event["billing_outcome"] == "aborted"
    assert event["settlement_method"] == "estimated"
    assert int(event["actual_credits_micro"]) == catalog.estimated_charge(PREMIUM, MSG)


def test_the_published_credits_match_what_quota_usage_was_debited(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["a"], input_tokens=900, output_tokens=300))
    sent = send_message(qapi, chat, MSG)
    event = usage_event_of(quota, sent.request_id)[0]
    spent, reserved = quota.db().quota_amounts(USER_A_ID, "total", "daily")
    assert int(event["actual_credits_micro"]) == spent
    assert reserved == 0


def test_no_prompt_or_answer_text_reaches_the_plugin_logs(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["SECRET-ANSWER-TEXT"]))
    sent = send_message(qapi, chat, "SECRET-PROMPT-TEXT")
    usage_event_of(quota, sent.request_id)
    audit_events_of(quota, sent.request_id)
    log = quota.server.log_path.read_text(errors="replace")
    for line in log.splitlines():
        if "static model policy plugin" in line or "mini-chat audit event" in line:
            assert "SECRET-PROMPT-TEXT" not in line and "SECRET-ANSWER-TEXT" not in line


# --------------------------------------------------------------------------- #
# audit events
# --------------------------------------------------------------------------- #


def test_a_completed_turn_is_audited_as_turn_completed(quota, qapi):
    chat = new_chat(quota)
    sent = send_message(qapi, chat, MSG)
    events = audit_events_of(quota, sent.request_id)
    assert len(events) == 1
    assert events[0]["event_type"] == "turn_completed"


def test_failed_and_cancelled_turns_are_audited_as_turn_failed(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, error_response(500, "boom"))
    failed = send_message(qapi, chat, MSG)
    assert audit_events_of(quota, failed.request_id)[0]["event_type"] == "turn_failed"

    quota.mock.script(*RESPONSES, sse_text(["a"], pause_after_deltas=0))
    live = open_stream(qapi, chat, {"content": "second"})
    rid = live.expect("stream_started").data["request_id"]
    quota.mock.wait_paused()
    live.close()
    quota.mock.wait_client_disconnected()
    events = audit_events_of(quota, rid)
    assert events[0]["event_type"] == "turn_failed", "there is no turn_cancelled type"


def test_nothing_is_audited_for_a_replay(quota, qapi):
    chat = new_chat(quota)
    rid = str(uuid.uuid4())
    send_message(qapi, chat, MSG, request_id=rid)
    audit_events_of(quota, rid)
    send_message(qapi, chat, MSG, request_id=rid)
    time.sleep(1.5)
    assert len(audit_events_of(quota, rid)) == 1
