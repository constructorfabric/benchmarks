"""Web search under quota (black-box): the tool and its guard in the provider request, the
per-message call limit enforced mid-turn, the daily quota checked at preflight, call accounting in
``quota_usage`` and the code interpreter per-message limit.

Runs on the ``quota`` stack: ``quota.web_search_daily_quota`` 3, ``quota.web_search_max_calls_per_message``
2, ``quota.code_interpreter_max_calls_per_message`` 2. ``mock-premium`` supports web search, the
standard model does not.

Source: DESIGN 4 "Web Search Configuration" / "Web Search Quota Enforcement (P1 Determinism
Rules)" / "Code Interpreter Configuration" / "Code Interpreter Quota Enforcement", 2.2 kill
switches, 3.7 ``quota_usage`` (tool counters on bucket ``total`` only), 5.5.6, 5.8 / 5.9
(``web_search_calls_exceeded`` settles estimated), the REST error table (429 subject
``web_search``).
"""

from __future__ import annotations

import pytest

from . import catalog
from .mock_openai_provider import (
    code_interpreter_events,
    sse_text,
    web_annotation,
    web_search_events,
)
from .support import (
    TENANT_A_ID,
    TENANT_IDS,
    USER_A_ID,
    USER_IDS,
    assert_precondition,
    assert_problem,
    get_turn,
    open_stream,
    reject_message,
    send_message,
    wait_for,
)

PREMIUM = catalog.PREMIUM_ID
STANDARD = catalog.STANDARD_ID
LIMITS = catalog.QUOTA_LIMITS
RESPONSES = ("POST", "/v1/responses")
WEB_GUARD = ("Use web_search only if the answer cannot be obtained from the provided context "
             "or your training data. Never use it for general knowledge questions. At most one "
             "web_search call per request.")
WEB = {"web_search": {"enabled": True}}
TEXT = "latest news"


@pytest.fixture()
def qapi(quota):
    client = quota.api()
    yield client
    client.close()


def new_chat(stack, model=PREMIUM, user="a"):
    return stack.seeder().insert_chat(tenant=TENANT_IDS[user], user=USER_IDS[user], model=model)


def chat_requests(stack):
    return stack.mock.requests_to(*RESPONSES)


def searches(n, chunks=("a", "b"), **kwargs):
    events = [e for _ in range(n) for e in web_search_events()]
    return sse_text(list(chunks), inserts=[(1, events)], input_tokens=900, output_tokens=100,
                    **kwargs)


def interpreter_runs(n, **kwargs):
    events = [e for i in range(n) for e in code_interpreter_events(f"run {i}")]
    return sse_text(["a", "b"], inserts=[(1, events)], input_tokens=900, output_tokens=100,
                    **kwargs)


def assert_exhausted(resp, scope):
    problem = assert_problem(resp, 429, "resource_exhausted")
    assert problem["context"]["violations"][0]["subject"] == scope, problem
    assert problem["context"]["violations"][0]["description"] == "quota_exceeded"
    return problem


# --------------------------------------------------------------------------- #
# tool inclusion, guard, relay
# --------------------------------------------------------------------------- #


def test_the_tool_and_its_guard_are_sent_when_enabled_and_supported(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, searches(1))
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done"

    request = chat_requests(quota)[-1]
    tool = request.tool("web_search")
    assert tool is not None, request.tool_types()
    assert tool["search_context_size"] == catalog.entry(PREMIUM)["web_search_context_size"]
    assert WEB_GUARD in request.system_text()
    tools = [e.data for e in sent.of("tool")]
    assert any(t["name"] == "web_search" and t["phase"] == "start" for t in tools), tools
    assert any(t["name"] == "web_search" and t["phase"] == "done" for t in tools), tools


def test_without_the_flag_there_is_no_tool_no_guard_and_no_daily_check(quota, qapi):
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=1000)
    chat = new_chat(quota)
    sent = send_message(qapi, chat, TEXT)
    assert sent.names_no_ping()[-1] == "done"
    request = chat_requests(quota)[-1]
    assert request.tool("web_search") is None
    assert WEB_GUARD not in request.system_text()


def test_a_model_without_web_search_proceeds_without_the_tool(quota, qapi):
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=1000)
    chat = new_chat(quota, STANDARD)
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done"
    request = chat_requests(quota)[-1]
    assert request.tool("web_search") is None
    assert WEB_GUARD not in request.system_text()
    turn = quota.db().turn(chat, sent.request_id)
    assert turn["web_search_enabled"] == 1, "the request flag is stored even when no tool is sent"
    assert turn["reserved_credits_micro"] == catalog.reserve_for(STANDARD, TEXT)[
        "reserved_credits_micro"], "no surcharge for a tool that is not sent"


def test_a_downgrade_to_a_model_without_web_search_skips_the_daily_check(quota, qapi):
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=1000)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="tier:premium",
                                period="daily", spent=LIMITS["tier:premium"]["daily"])
    chat = new_chat(quota)
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done", sent.events
    assert sent.done["effective_model"] == STANDARD
    assert chat_requests(quota)[-1].tool("web_search") is None


def test_the_web_search_surcharge_is_reserved_for_the_tool_that_is_sent(quota, qapi):
    chat = new_chat(quota)
    sent = send_message(qapi, chat, TEXT, **WEB)
    turn = quota.db().turn(chat, sent.request_id)
    r = catalog.reserve_for(PREMIUM, TEXT, surcharge_tokens=500)
    assert turn["reserve_tokens"] == r["reserve_tokens"]
    assert turn["reserved_credits_micro"] == r["reserved_credits_micro"]


def test_web_citations_reach_the_client_before_done(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(
        ["See the source."], inserts=[(1, web_search_events())],
        annotations=[web_annotation("https://example.test/a", "Example A", 4, 14)]))
    sent = send_message(qapi, chat, TEXT, **WEB)
    names = sent.names_no_ping()
    assert names[-2:] == ["citations", "done"], names
    item = sent.of("citations")[0].data["items"][0]
    assert item["source"] == "web" and item["url"] == "https://example.test/a"


# --------------------------------------------------------------------------- #
# per-message limit: web_search_calls_exceeded
# --------------------------------------------------------------------------- #


def test_the_third_search_fails_the_turn_with_web_search_calls_exceeded(quota, qapi):
    chat = new_chat(quota)
    # the mock writes the whole script at once, so the hard cancel is only observable while the
    # stream is still open: hold it open after its last event
    quota.mock.script(*RESPONSES, searches(3, hold_open=True))

    sent = send_message(qapi, chat, TEXT, **WEB)

    assert sent.names_no_ping()[-1] == "error"
    assert not sent.of("done")
    assert sent.error["code"] == "web_search_calls_exceeded"
    quota.mock.wait_client_disconnected()  # the provider stream was cancelled
    turn = quota.db().turn(chat, sent.request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "web_search_calls_exceeded")
    assert turn["web_search_completed_count"] == 2, "completed calls only"
    assert [m for m in quota.db().ordered_messages(chat) if m["role"] == "assistant"] == []
    status = get_turn(qapi, chat, sent.request_id).json()
    assert (status["state"], status["error_code"]) == ("error", "web_search_calls_exceeded")


def test_the_limit_breach_settles_estimated_and_counts_the_completed_calls(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, searches(3))
    send_message(qapi, chat, TEXT, **WEB)

    # the stream was cancelled before usage arrived: estimated input (message + the web search
    # surcharge, never refunded) plus the floor
    spent = catalog.estimated_charge(PREMIUM, TEXT, surcharge_tokens=500)
    for period in ("daily", "monthly"):
        total = quota.db().quota_row(USER_A_ID, "total", period)
        assert total["spent_credits_micro"] == spent, period
        assert total["reserved_credits_micro"] == 0
        assert total["calls"] == 1
        assert (total["input_tokens"], total["output_tokens"]) == (0, 0)
        assert total["web_search_calls"] == 2


def test_exactly_the_limit_is_allowed_and_the_counter_is_per_turn(quota, qapi):
    chat = new_chat(quota)
    for text in (TEXT, "again"):
        quota.mock.script(*RESPONSES, searches(2))
        sent = send_message(qapi, chat, text, **WEB)
        assert sent.names_no_ping()[-1] == "done", sent.events


# --------------------------------------------------------------------------- #
# daily quota
# --------------------------------------------------------------------------- #


def test_an_exhausted_daily_web_search_quota_is_a_429_before_any_provider_call(quota, qapi):
    chat = new_chat(quota)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=catalog.WEB_SEARCH_DAILY_QUOTA)
    before = quota.db().counts(chat)

    resp = reject_message(qapi, chat, {"content": TEXT, **WEB})

    assert_exhausted(resp, "web_search")
    assert not chat_requests(quota)
    assert quota.db().counts(chat) == before

    # a request that does not use web search is never rejected by this quota
    sent = send_message(qapi, chat, "no search")
    assert sent.names_no_ping()[-1] == "done"


def test_the_daily_quota_closes_once_the_accounted_calls_reach_it(quota, qapi):
    chat = new_chat(quota)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=catalog.WEB_SEARCH_DAILY_QUOTA - 1)
    quota.mock.script(*RESPONSES, searches(1))
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done"
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["web_search_calls"] == \
        catalog.WEB_SEARCH_DAILY_QUOTA
    assert_exhausted(reject_message(qapi, chat, {"content": "again", **WEB}), "web_search")


def test_only_the_daily_total_row_is_read_by_the_daily_check(quota, qapi):
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="monthly", web_search_calls=500)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", web_search_calls=500, stale=True)
    chat = new_chat(quota)
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done"


def test_token_exhaustion_is_reported_before_the_web_search_scope(quota, qapi):
    chat = new_chat(quota)
    quota.seeder().insert_quota(tenant=TENANT_A_ID, user=USER_A_ID, bucket="total",
                                period="daily", spent=LIMITS["total"]["daily"],
                                web_search_calls=catalog.WEB_SEARCH_DAILY_QUOTA)
    assert_exhausted(reject_message(qapi, chat, {"content": TEXT, **WEB}), "tokens")


def test_the_web_search_kill_switch_still_rejects_with_feature_disabled(restricted):
    api = restricted.api()
    try:
        chat = restricted.seeder().insert_chat(tenant=TENANT_A_ID, user=USER_A_ID, model=PREMIUM)
        resp = reject_message(api, chat, {"content": TEXT, **WEB})
        assert_precondition(assert_problem(resp, 400, "failed_precondition"), "web_search",
                            "FEATURE_DISABLED")
        assert not restricted.mock.requests_to(*RESPONSES)
    finally:
        api.close()


# --------------------------------------------------------------------------- #
# accounting
# --------------------------------------------------------------------------- #


def test_completed_searches_are_counted_in_the_total_bucket_only(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, searches(2))
    sent = send_message(qapi, chat, TEXT, **WEB)
    assert sent.names_no_ping()[-1] == "done"

    assert quota.db().turn(chat, sent.request_id)["web_search_completed_count"] == 2
    for period in ("daily", "monthly"):
        total = quota.db().quota_row(USER_A_ID, "total", period)
        assert total["web_search_calls"] == 2, period
        assert total["code_interpreter_calls"] == 0 and total["file_search_calls"] == 0
        premium = quota.db().quota_row(USER_A_ID, "tier:premium", period)
        assert premium["web_search_calls"] == 0, "tool counters live in bucket total only"
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["spent_credits_micro"] == \
        catalog.model_credits(PREMIUM, 900, 100)


def test_searches_of_a_cancelled_turn_are_counted_too(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, sse_text(["a", "b"], inserts=[(1, web_search_events())],
                                           pause_after_deltas=2))
    live = open_stream(qapi, chat, {"content": TEXT, **WEB})
    rid = live.expect("stream_started").data["request_id"]
    quota.mock.wait_paused()
    live.close()
    wait_for(lambda: (lambda t: t is not None and t["state"] == "cancelled")(
        quota.db().turn(chat, rid)), "the turn to be cancelled")
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["web_search_calls"] == 1


def test_a_turn_without_searches_adds_no_search_calls(quota, qapi):
    chat = new_chat(quota)
    send_message(qapi, chat, TEXT, **WEB)  # the tool is available, the provider did not use it
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["web_search_calls"] == 0


# --------------------------------------------------------------------------- #
# code interpreter: per-message limit (the daily quota needs attachments of slice S6)
# --------------------------------------------------------------------------- #


def test_the_code_interpreter_limit_fails_the_turn_like_the_web_search_one(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, interpreter_runs(3, hold_open=True))  # open: see the web test

    sent = send_message(qapi, chat, "run the analysis")

    assert sent.error["code"] == "code_interpreter_calls_exceeded"
    quota.mock.wait_client_disconnected()
    turn = quota.db().turn(chat, sent.request_id)
    assert (turn["state"], turn["error_code"]) == ("failed", "code_interpreter_calls_exceeded")
    assert turn["code_interpreter_completed_count"] == 2
    for period in ("daily", "monthly"):
        total = quota.db().quota_row(USER_A_ID, "total", period)
        assert total["code_interpreter_calls"] == 2
        assert total["spent_credits_micro"] == catalog.estimated_charge(PREMIUM, "run the analysis")


def test_code_interpreter_runs_up_to_the_limit_complete_and_are_accounted(quota, qapi):
    chat = new_chat(quota)
    quota.mock.script(*RESPONSES, interpreter_runs(2))
    sent = send_message(qapi, chat, "run the analysis")
    assert sent.names_no_ping()[-1] == "done"
    assert quota.db().quota_row(USER_A_ID, "total", "daily")["code_interpreter_calls"] == 2
    assert quota.db().quota_row(USER_A_ID, "tier:premium", "daily")["code_interpreter_calls"] == 0
