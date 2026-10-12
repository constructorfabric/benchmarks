"""S4 turn mutations: pipeline order, setup failure, concurrency, billing, requester check.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.6 "Retry / edit variant": (1) read-only mutation preview, (2) mutation
  preflight (quota cascade, kill switches, image guards) whose rejection
  "returns a JSON error and changes nothing", (3) mutation commit, (4) context
  assembly, provider resolution and the quota reserve (last step); a failure
  in (4) returns a JSON error and marks the new turn ``failed`` with
  ``context_length_exceeded`` / ``quota_exceeded`` / ``turn_setup_failed``,
  no reserve exists then;
* §3.9 rules 2 (requester -> 403 ``permission_denied`` ``AUTHZ_DENIED``),
  7 (atomicity; insert-race loser -> 409 ``GENERATION_IN_PROGRESS``, a loser
  that sees the new latest turn -> 409 ``NOT_LATEST_TURN``), 8
  (preflight-before-mutation order), the delete contract ("no streaming and no
  preflight") and "Audit Events for Turn Mutations" (a retry/edit turn that
  fails setup emits no turn audit event);
* §3.3 REST error table (429 ``resource_exhausted`` ``tokens`` /
  ``quota_exceeded``, ``FEATURE_DISABLED``, ``VISION_NOT_SUPPORTED``,
  ``TOO_MANY_IMAGES``, ``CONTEXT_BUDGET_EXCEEDED``, ``AUTHZ_DENIED``) and
  "Streaming error codes" (``context_length_exceeded`` is never sent over
  SSE, the client gets a JSON error);
* §3.7 ``chat_turns`` "Preflight columns on retry/edit" (NULL until the
  reserve), ``web_search_enabled`` ("Retry/edit reuse it"), ``quota_usage``;
* "Unstarted retry/edit turn" (§5.7): no settlement and no outbox event;
* §5.4.1 reserve (``estimated_text_tokens + prior_context_tokens``,
  ``max_output_tokens_applied``), §5.3 credits, §5.6 usage event
  (``dedupe_key``), §5.8 billing outcome ``completed`` / ``actual``;
* "Context Plan Truncation Algorithm" (mandatory items above the budget ->
  ``CONTEXT_BUDGET_EXCEEDED``).

Helpers come from ``test_s4_turn_mutations``.
"""

from __future__ import annotations

import copy
import threading
import time
import uuid
from typing import Any

import httpx
import pytest

from conftest import default_model_catalog
from mc_helpers import assert_problem, db_uuid, parse_sse_lines, uuid_bytes, wait_until
from mc_seed import outbox_snapshot
from test_s4_turn_mutations import (
    A1,
    AUDIT_QUEUE,
    EDIT_TEXT,
    OPS,
    Q1,
    Q2,
    STREAMING_OPS,
    USAGE_QUEUE,
    OutboxRecorder,
    _s4_servers,  # noqa: F401 - module-scoped fixture used by the servers below
    assert_rejected,
    assert_sse_done,
    assert_unchanged,
    conversation,
    db_exec,
    link,
    live_turns,
    message_rows,
    module_server,
    mutate,
    new_chat,
    retry_path,
    same_uuid,
    seed,
    seed_image,
    send_done,
    snapshot,
    stream_requests,
    turn_path,
    turn_row,
    turn_rows,
    turn_status,
    two_turn_chat,
)

POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
STANDARD_MODEL = "gpt-4.1-mini"
#: ``streaming.max_output_tokens`` default.
STREAMING_MAX_OUTPUT_TOKENS = 32_768
#: Gear ``estimation_budgets.minimal_generation_floor`` default.
MINIMAL_GENERATION_FLOOR = 50
#: Version of the static model-policy plugin snapshot.
POLICY_VERSION = 1
PREFLIGHT_COLUMNS = (
    "reserve_tokens",
    "max_output_tokens_applied",
    "reserved_credits_micro",
    "policy_version_applied",
    "effective_model",
    "minimal_generation_floor_applied",
)

# Budget model: ``fixed_overhead_tokens`` and ``safety_margin_pct`` are 0, so
# an item estimate is ceil(utf8_bytes / 4); no tools, no surcharges.
# input_limit = min(1000, 1000 - 900) = 100 = token_budget; system prompt
# 200 bytes = 50 tokens. A 40-byte message fits (60), a 400-byte one does not
# (150), and it is below max_input_tokens (100 <= 1000), so not INPUT_TOO_LONG.
SMALL_CONTEXT_MODEL = "gpt-e2e-s4-context-1000"
FITS = "y" * 40
DOES_NOT_FIT = "y" * 400


def _small_context_catalog() -> list[dict[str, Any]]:
    catalog = default_model_catalog()
    entry = copy.deepcopy(next(e for e in catalog if e["id"] == "gpt-text-only"))
    entry.update(
        id=SMALL_CONTEXT_MODEL,
        provider_model_id=f"{SMALL_CONTEXT_MODEL}-provider",
        display_name=SMALL_CONTEXT_MODEL,
        context_window=1_000,
        max_output_tokens=900,
        max_input_tokens=1_000,
        system_prompt="S" * 200,
        preference={"is_default": False, "sort_order": 20},
    )
    entry["estimation_budgets"] = {
        **entry["estimation_budgets"],
        "bytes_per_token_conservative": 4,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 0,
    }
    return catalog + [entry]


@pytest.fixture(scope="module")
def preflight_server(mock, _s4_servers):  # noqa: F811 - fixture argument
    """Every tier exhausted (1000 micro-credits) and ``disable_web_search`` on."""
    tiny = {"limit_daily_credits_micro": 1_000, "limit_monthly_credits_micro": 1_000}
    overrides = {
        "gears": {
            POLICY_PLUGIN: {
                "config": {
                    "default_standard_limits": tiny,
                    "default_premium_limits": tiny,
                    "kill_switches": {"disable_web_search": True},
                }
            }
        }
    }
    return module_server(mock, _s4_servers, "s4-preflight", overrides)


@pytest.fixture(scope="module")
def budget_server(mock, _s4_servers):  # noqa: F811 - fixture argument
    overrides = {"gears": {POLICY_PLUGIN: {"config": {"model_catalog": _small_context_catalog()}}}}
    return module_server(mock, _s4_servers, "s4-budget", overrides)


def _quota_rows(server, user: str) -> dict[tuple[str, str, str], dict[str, int]]:
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, calls, "
        "input_tokens, output_tokens FROM quota_usage WHERE tenant_id = ? AND user_id = ?",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    return {
        (str(r["period_type"]), str(r["period_start"]), str(r["bucket"])): {
            "spent": int(r["spent_credits_micro"] or 0),
            "reserved": int(r["reserved_credits_micro"] or 0),
            "calls": int(r["calls"] or 0),
            "input_tokens": int(r["input_tokens"] or 0),
            "output_tokens": int(r["output_tokens"] or 0),
        }
        for r in rows
    }


def _assert_old_turn_is_latest(server, chat_id: str, request_id: str, turns_before: int) -> None:
    latest = live_turns(server, chat_id)
    assert latest and latest[0]["request_id"] == request_id, latest
    assert latest[0]["deleted_at"] is None and latest[0]["replaced_by_request_id"] is None, latest[0]
    assert len(turn_rows(server, chat_id)) == turns_before, "a preflight rejection must create no turn row"


# --------------------------------------------------------------------------
# Preflight before the mutation (steps 1-2 leave the chat unchanged)
# --------------------------------------------------------------------------


@pytest.mark.slow
@pytest.mark.parametrize("op", STREAMING_OPS)
def test_quota_rejection_leaves_the_previous_turn_in_place(preflight_server, mock, op):
    server = preflight_server
    chat_id = new_chat(server)["id"]
    turn = seed(server, chat_id, user_text="Seeded before the quota ran out")
    before = snapshot(server, chat_id)
    quota_before = _quota_rows(server, "A1")
    calls = len(stream_requests(mock))

    result = mutate(server, op, chat_id, turn.request_id)

    problem = assert_rejected(result, 429, "resource_exhausted", violation_subject="tokens")
    violation = problem["context"]["violations"][0]
    assert violation["subject"] == "tokens" and violation["description"] == "quota_exceeded", problem
    assert_unchanged(server, mock, chat_id, before, calls)
    _assert_old_turn_is_latest(server, chat_id, turn.request_id, 1)
    assert _quota_rows(server, "A1") == quota_before, "a rejected preflight must leave no reserve"


@pytest.mark.slow
def test_delete_runs_no_preflight_and_succeeds_with_exhausted_quota(preflight_server, mock):
    server = preflight_server
    chat_id = new_chat(server)["id"]
    turn = seed(server, chat_id)

    result = mutate(server, "delete", chat_id, turn.request_id)

    assert result.status_code == 204, result.text
    assert turn_row(server, chat_id, turn.request_id)["deleted_at"] is not None
    assert live_turns(server, chat_id) == []


@pytest.mark.slow
@pytest.mark.parametrize("op", STREAMING_OPS)
def test_web_search_kill_switch_rejects_a_web_search_turn_before_the_quota(preflight_server, mock, op):
    """Retry and edit reuse ``web_search_enabled``; ``disable_web_search`` is checked before the cascade."""
    server = preflight_server
    chat_id = new_chat(server)["id"]
    turn = seed(server, chat_id, user_text="Search the web for fruit prices")
    db_exec(server, "UPDATE chat_turns SET web_search_enabled = 1 WHERE id = ?", (uuid_bytes(turn.turn_id),))
    before = snapshot(server, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(server, op, chat_id, turn.request_id)

    assert_rejected(
        result, 400, "failed_precondition", violation_subject="web_search", violation_type="FEATURE_DISABLED"
    )
    assert_unchanged(server, mock, chat_id, before, calls)
    _assert_old_turn_is_latest(server, chat_id, turn.request_id, 1)


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_vision_guard_rejection_leaves_the_chat_unchanged(mc, mock, op):
    chat_id = new_chat(mc, model="gpt-text-only")["id"]
    turn = seed(mc, chat_id, user_text="What is in this picture?", model="gpt-text-only")
    image_id, _ = seed_image(mc, chat_id)
    link(mc, chat_id, turn.user_message_id, image_id)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, turn.request_id)

    assert_rejected(result, 400, "invalid_argument", field_name="content_type", field_reason="VISION_NOT_SUPPORTED")
    assert_unchanged(mc, mock, chat_id, before, calls)
    _assert_old_turn_is_latest(mc, chat_id, turn.request_id, 1)


@pytest.mark.parametrize("op", STREAMING_OPS)
def test_image_count_guard_rejection_leaves_the_chat_unchanged(mc, mock, op):
    max_images = int(mc.gear_config.get("rag", {}).get("max_images_per_message", 4))
    chat_id = new_chat(mc)["id"]
    turn = seed(mc, chat_id, user_text="Compare these pictures")
    for i in range(max_images + 1):
        image_id, _ = seed_image(mc, chat_id, filename=f"img-{i}.png")
        link(mc, chat_id, turn.user_message_id, image_id)
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, turn.request_id)

    assert_rejected(result, 400, "out_of_range", field_name="image_count", field_reason="TOO_MANY_IMAGES")
    assert_unchanged(mc, mock, chat_id, before, calls)
    _assert_old_turn_is_latest(mc, chat_id, turn.request_id, 1)


# --------------------------------------------------------------------------
# Setup failure after the mutation commit
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_edit_context_budget_failure_after_commit_marks_the_new_turn_failed(budget_server, mock):
    server = budget_server
    chat_id = new_chat(server, model=SMALL_CONTEXT_MODEL)["id"]
    _, old_rid = send_done(server, chat_id, FITS)
    calls = len(stream_requests(mock))
    usage_before = outbox_snapshot(server, USAGE_QUEUE).activity
    audit_before = outbox_snapshot(server, AUDIT_QUEUE).activity
    quota_before = _quota_rows(server, "A1")

    with OutboxRecorder(server, chat_id) as recorder:
        result = mutate(server, "edit", chat_id, old_rid, body={"content": DOES_NOT_FIT})

        assert_rejected(result, 400, "out_of_range", field_reason="CONTEXT_BUDGET_EXCEEDED")
        assert len(stream_requests(mock)) == calls, "no provider call after a setup failure"

        old = turn_row(server, chat_id, old_rid)
        assert old["deleted_at"] is not None, "the mutation committed before the setup failure"
        new_rid = old["replaced_by_request_id"]
        assert new_rid is not None and new_rid != old_rid, old
        new = turn_row(server, chat_id, new_rid)
        assert new["state"] == "failed", new
        assert new["error_code"] == "context_length_exceeded", new
        assert new["deleted_at"] is None and new["completed_at"] is not None, new
        assert new["assistant_message_id"] is None, new
        for column in PREFLIGHT_COLUMNS:
            assert new[column] is None, f"{column} must stay NULL without a reserve: {new}"
        new_user = message_rows(server, chat_id, new_rid)
        assert [(m["role"], m["content"]) for m in new_user] == [("user", DOES_NOT_FIT)], new_user

        status = turn_status(server, chat_id, new_rid)
        assert status.status_code == 200, status.text
        assert status.json()["state"] == "error", status.json()
        assert status.json()["error_code"] == "context_length_exceeded", status.json()

        edit_event = recorder.wait_one(
            lambda: recorder.audit_events("turn_edit", original_request_id=old_rid), "turn_edit audit payload"
        )
        assert same_uuid(edit_event["new_request_id"], new_rid), edit_event
        time.sleep(1.5)
        assert outbox_snapshot(server, USAGE_QUEUE).activity == usage_before, "no usage event without a reserve"
        assert outbox_snapshot(server, AUDIT_QUEUE).activity == audit_before + 1, "only the turn_edit event"
        assert recorder.usage_events(new_rid) == []
        assert recorder.audit_events("turn_failed", request_id=new_rid) == []
        assert recorder.audit_events("turn_completed", request_id=new_rid) == []
    assert _quota_rows(server, "A1") == quota_before, "no reserve and no settlement"

    mock.enqueue({"text": "Still working."})
    result, _ = send_done(server, chat_id, FITS)
    assert result.text_content() == "Still working."


# --------------------------------------------------------------------------
# Concurrency (rule 7)
# --------------------------------------------------------------------------


class BackgroundRequest:
    """A request sent from a thread after a shared barrier; reads the whole answer."""

    def __init__(self, server, method: str, path: str, barrier: threading.Barrier, json_body: Any = None) -> None:
        self.url = server.url(path)
        self.method = method
        self.headers = server.headers("A1")
        self.barrier = barrier
        self.json_body = json_body
        self.status_code: int | None = None
        self.content_type = ""
        self.events: list[tuple[str, Any]] = []
        self.response: httpx.Response | None = None
        self.error: str | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)

    @property
    def is_sse(self) -> bool:
        return self.content_type.startswith("text/event-stream")

    def _run(self) -> None:
        kwargs: dict[str, Any] = {"headers": self.headers}
        if self.json_body is not None:
            kwargs["json"] = self.json_body
        try:
            timeout = httpx.Timeout(connect=5.0, read=60.0, write=10.0, pool=10.0)
            with httpx.Client(timeout=timeout) as client:
                self.barrier.wait(timeout=30)
                with client.stream(self.method, self.url, **kwargs) as resp:
                    self.status_code = resp.status_code
                    self.content_type = resp.headers.get("content-type", "")
                    if self.is_sse:
                        self.events = list(parse_sse_lines(resp.iter_lines()))
                    else:
                        resp.read()
                    self.response = resp
        except Exception as exc:  # noqa: BLE001 - reported to the test
            self.error = f"{type(exc).__name__}: {exc}"

    def start(self) -> "BackgroundRequest":
        self._thread.start()
        return self

    def join(self) -> "BackgroundRequest":
        self._thread.join(timeout=90)
        assert not self._thread.is_alive(), "background request did not finish"
        assert self.error is None, self.error
        return self


def _race(mc, mock, chat_id: str, old_rid: str, ops: tuple[str, str]) -> list[BackgroundRequest]:
    mock.enqueue({"chunks": [f"race-{i} " for i in range(6)], "delay_ms": 50})
    barrier = threading.Barrier(len(ops))
    senders = []
    for op in ops:
        if op == "retry":
            senders.append(BackgroundRequest(mc, "POST", retry_path(chat_id, old_rid), barrier))
        else:
            senders.append(
                BackgroundRequest(mc, "PATCH", turn_path(chat_id, old_rid), barrier, {"content": EDIT_TEXT})
            )
    for sender in senders:
        sender.start()
    return [sender.join() for sender in senders]


@pytest.mark.parametrize(
    "ops",
    [("retry", "retry")] * 3 + [("retry", "edit")],
    ids=["retry-retry-1", "retry-retry-2", "retry-retry-3", "retry-edit"],
)
def test_concurrent_mutations_of_the_latest_turn_start_exactly_one_turn(mc, mock, ops):
    chat_id = new_chat(mc)["id"]
    _, old_rid = send_done(mc, chat_id, Q1)
    calls = len(stream_requests(mock))

    senders = _race(mc, mock, chat_id, old_rid, ops)

    winners = [s for s in senders if s.status_code == 200 and s.is_sse]
    losers = [s for s in senders if s not in winners]
    assert len(winners) == 1, [(s.status_code, s.response.text if s.response else None) for s in senders]
    loser = losers[0]
    assert loser.response is not None
    problem = assert_problem(loser.response, 409, category="aborted")
    assert problem["context"].get("reason") in ("GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"), problem
    names = [name for name, _ in winners[0].events]
    assert names[0] == "stream_started" and names[-1] == "done", names
    started = winners[0].events[0][1]
    new_rid = started["request_id"]
    assert started["is_new_turn"] is True and new_rid != old_rid, started

    assert len(stream_requests(mock)) == calls + 1, "only the winner calls the provider"
    rows = turn_rows(mc, chat_id)
    assert [r["request_id"] for r in rows] == [old_rid, new_rid], "the loser must leave no turn row"
    old = turn_row(mc, chat_id, old_rid)
    assert old["deleted_at"] is not None and old["replaced_by_request_id"] == new_rid, old
    latest = live_turns(mc, chat_id)
    assert [t["request_id"] for t in latest] == [new_rid], latest
    assert latest[0]["state"] == "completed", latest[0]
    users = [m for m in message_rows(mc, chat_id) if m["role"] == "user"]
    assert [m["request_id"] for m in users] == [old_rid, new_rid], "the loser must leave no user message"
    live = [m for m in message_rows(mc, chat_id) if m["deleted_at"] is None]
    assert [(m["request_id"], m["role"]) for m in live] == [(new_rid, "user"), (new_rid, "assistant")], live


# --------------------------------------------------------------------------
# Billing of the replacement turn
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def _credits(entry: dict[str, Any], input_tokens: int, output_tokens: int) -> int:
    return _ceil_div(input_tokens * entry["input_tokens_credit_multiplier_micro"], 1_000_000) + _ceil_div(
        output_tokens * entry["output_tokens_credit_multiplier_micro"], 1_000_000
    )


def _expected_reserve(entry: dict[str, Any], content: str, prior_context_tokens: int) -> dict[str, Any]:
    budgets = entry["estimation_budgets"]
    base = _ceil_div(len(content.encode("utf-8")), budgets["bytes_per_token_conservative"])
    base += budgets["fixed_overhead_tokens"]
    estimated_input = _ceil_div(base * (100 + budgets["safety_margin_pct"]), 100) + prior_context_tokens
    max_out = min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)
    return {
        "reserve_tokens": estimated_input + max_out,
        "max_output_tokens_applied": max_out,
        "reserved_credits_micro": _credits(entry, estimated_input, max_out),
        "policy_version_applied": POLICY_VERSION,
        "effective_model": entry["id"],
        "minimal_generation_floor_applied": min(MINIMAL_GENERATION_FLOOR, max_out),
    }


def _wait_no_reserve(server, user: str) -> dict[tuple[str, str, str], dict[str, int]]:
    return wait_until(
        lambda: (rows := _quota_rows(server, user)) and all(v["reserved"] == 0 for v in rows.values()) and rows,
        timeout=45.0,
        description=f"{user} to have no outstanding reserve",
    )


def test_retry_reserves_settles_and_bills_the_new_turn(mc, mock):
    user = "B1"  # settlement-sensitive: tenant B rows, as in the S3 billing tests
    ident = mc.identity(user)
    entry = next(e for e in mc.catalog if e["id"] == STANDARD_MODEL)
    chat_id = new_chat(mc, user, STANDARD_MODEL)["id"]
    # Turn 1 has usage (400 + 60); the retried turn 2 has none. prior_context_tokens of
    # the retry is therefore 460 whether or not the target turn is still visible
    # when the reserve is computed.
    mock.enqueue(
        {"text": A1, "usage": {"input_tokens": 400, "output_tokens": 60}},
        {"text": "Zero usage answer.", "usage": {"input_tokens": 0, "output_tokens": 0}},
        {"text": "Billed retry answer.", "usage": {"input_tokens": 300, "output_tokens": 50}},
    )

    with OutboxRecorder(mc, chat_id) as recorder:
        _, rid1 = send_done(mc, chat_id, Q1, user)
        _, rid2 = send_done(mc, chat_id, Q2, user)
        recorder.wait_one(lambda: recorder.usage_events(rid2), "usage event of the original turn")
        before = _wait_no_reserve(mc, user)

        result = mutate(mc, "retry", chat_id, rid2, user=user)

        started = assert_sse_done(result)
        new_rid = started["request_id"]
        assert result.first("done")["usage"] == {"input_tokens": 300, "output_tokens": 50}
        new = turn_row(mc, chat_id, new_rid)
        assert new["state"] == "completed", new
        for column, value in _expected_reserve(entry, Q2, prior_context_tokens=460).items():
            assert new[column] == value, f"{column}: {new[column]!r} != {value!r} ({new})"

        actual = _credits(entry, 300, 50)
        after = _quota_rows(mc, user)
        total_keys = [k for k in after if k[2] == "total"]
        assert {k[0] for k in total_keys} >= {"daily", "monthly"}, sorted(after)
        zero = {"spent": 0, "reserved": 0, "calls": 0, "input_tokens": 0, "output_tokens": 0}
        for key in total_keys:
            old, cur = before.get(key, zero), after[key]
            assert cur["reserved"] == 0, (key, cur)
            assert cur["spent"] == old["spent"] + actual, (key, old, cur)
            assert cur["calls"] == old["calls"] + 1, (key, old, cur)
            assert (cur["input_tokens"], cur["output_tokens"]) == (
                old["input_tokens"] + 300,
                old["output_tokens"] + 50,
            ), (key, old, cur)

        event = recorder.wait_one(lambda: recorder.usage_events(new_rid), "usage event of the new turn")
        assert event["billing_outcome"] == "completed", event
        assert event["settlement_method"] == "actual", event
        assert event["terminal_state"] == "completed", event
        assert event["actual_credits_micro"] == actual, event
        assert event["effective_model"] == STANDARD_MODEL, event
        assert same_uuid(event["turn_id"], new["id"]), event
        assert event["dedupe_key"] == (
            f"{uuid.UUID(ident.tenant_id).hex}/{uuid.UUID(new['id']).hex}/{uuid.UUID(new_rid).hex}"
        ), event
        time.sleep(1.0)
        old_events = recorder.usage_events(rid2)
        assert len(old_events) == 1, f"the mutation must not emit a usage event for the replaced turn: {old_events}"
        assert old_events[0]["actual_credits_micro"] == 0, old_events
        assert len(recorder.usage_events(new_rid)) == 1
        assert len(recorder.usage_events(rid1)) == 1


# --------------------------------------------------------------------------
# Requester check (rule 2)
# --------------------------------------------------------------------------


@pytest.mark.parametrize("op", OPS)
def test_mutation_of_a_turn_of_another_requester_is_permission_denied(mc, mock, op):
    chat_id = new_chat(mc, "A1")["id"]
    turn = seed(mc, chat_id, user="A1", requester="A2", user_text="Asked by someone else")
    before = snapshot(mc, chat_id)
    calls = len(stream_requests(mock))

    result = mutate(mc, op, chat_id, turn.request_id, user="A1")

    assert_rejected(result, 403, "permission_denied", reason="AUTHZ_DENIED")
    assert_unchanged(mc, mock, chat_id, before, calls)
    _assert_old_turn_is_latest(mc, chat_id, turn.request_id, 1)


def test_conversation_helper_matches_the_two_turn_fixture(mc, mock):
    """Self-check of the shared fixture against the existing send path (no S4 endpoint)."""
    chat_id, rid1, rid2 = two_turn_chat(mc, mock)
    body = stream_requests(mock)[-1]["json"]
    assert conversation(body) == [("user", Q1), ("assistant", A1), ("user", Q2)]
    assert [t["request_id"] for t in live_turns(mc, chat_id)] == [rid2, rid1]
