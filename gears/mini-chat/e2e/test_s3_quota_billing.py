"""S3 quota enforcement, settlement, usage/audit outbox and quota status.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.2 "quota service" (two-phase reserve / commit, tier availability rule),
  "Quota Period Reset Semantics" (UTC calendar periods), "Quota Warning
  Thresholds" and "Quota Status Endpoint" (``QuotaStatusResponse``,
  ``QuotaTierStatus``, ``QuotaPeriodStatus``; ``mini_chat.get_quota_status``);
* §2 "Model Locked Per Chat" / emergency flags (``disable_premium_tier``,
  ``force_standard_tier``), "Two-Tier Rate Limiting" quota counting flow and
  "Downgrade Decision Flow" (cascade, candidate selection, downgrade reasons);
* §3.3 ``event: done`` (``DoneData``, ``QuotaWarning``) and the REST error
  mapping (429 ``resource_exhausted``, ``violations[0].subject = tokens``,
  ``description = quota_exceeded``; no SSE stream on preflight failure);
* §3.7 ``quota_usage`` (bucket rows ``total`` / ``tier:premium``, commit
  semantics, token telemetry only in ``total``) and ``chat_turns`` reserve
  columns;
* §5.3 credit arithmetic (per-component ``ceil_div``), §5.4.1 / §5.5 preflight
  reserve (``estimated_text_tokens``, ``prior_context_tokens``,
  ``max_output_tokens_applied = min(catalog, streaming.max_output_tokens)``),
  §5.4.4 settlement, §5.4.5 overshoot tolerance (``quota.overshoot_tolerance_factor``
  1.10, capped commit = ``reserved_credits_micro``);
* §5.6 usage outbox (queue ``mini-chat.usage_snapshot``, SDK ``UsageEvent``,
  ``dedupe_key = tenant.hex/turn.hex/request.hex``), §5.8 billing outcome
  table, §3.2 audit outbox (queue ``mini-chat.audit``, SDK
  ``TurnAuditEvent`` ``turn_completed``) delivered to the static plugins;
* §3.8 PEP ``USER_QUOTA`` ``read``, scoped to tenant + user.

Credits of the harness catalog (``conftest.default_model_catalog``):
``gpt-4.1`` premium 3x input / 15x output, ``gpt-4.1-mini`` standard 1x / 3x
(micro-credits per token = multiplier / 10^6). Every expected value is
derived from the catalog of the server under test.
"""

from __future__ import annotations

import calendar
import json
import threading
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import httpx
import pytest

from mc_helpers import (
    assert_problem,
    db_uuid,
    new_uuid,
    stream_sse,
    wait_until,
)
from mc_seed import (
    is_rfc3339,
    outbox_payloads_for_chat,
    outbox_snapshot,
    parse_ts_ns,
    seed_chat,
)

CHATS = "/mini-chat/v1/chats"
QUOTA_STATUS = "/mini-chat/v1/quota/status"
PREMIUM_MODEL = "gpt-4.1"
STANDARD_MODEL = "gpt-4.1-mini"
DISABLED_MODEL = "gpt-disabled"
#: ``streaming.max_output_tokens`` default (B.4).
STREAMING_MAX_OUTPUT_TOKENS = 32_768
#: Gear ``estimation_budgets.minimal_generation_floor`` default.
MINIMAL_GENERATION_FLOOR = 50
#: Version of the static model-policy plugin snapshot.
POLICY_VERSION = 1
#: ``quota.overshoot_tolerance_factor`` default.
OVERSHOOT_TOLERANCE = 1.10
#: ``quota.warning_threshold_pct`` default.
WARNING_THRESHOLD_PCT = 80
USAGE_QUEUE = "mini-chat.usage_snapshot"
AUDIT_QUEUE = "mini-chat.audit"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"

USAGE_EVENT_FIELDS = {
    "tenant_id",
    "user_id",
    "chat_id",
    "turn_id",
    "request_id",
    "effective_model",
    "selected_model",
    "terminal_state",
    "billing_outcome",
    "usage",
    "actual_credits_micro",
    "settlement_method",
    "policy_version_applied",
    "web_search_calls",
    "code_interpreter_calls",
    "file_search_calls",
    "timestamp",
    "requester_type",
    "dedupe_key",
}
USAGE_TOKENS_FIELDS = {
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_write_input_tokens",
    "reasoning_tokens",
}
TURN_AUDIT_FIELDS = {
    "event_type",
    "timestamp",
    "tenant_id",
    "requester_type",
    "user_id",
    "chat_id",
    "turn_id",
    "request_id",
    "selected_model",
    "effective_model",
    "usage",
    "latency_ms",
    "tool_calls",
    "policy_decisions",
    "error_code",
    "trace_id",
    "prompt",
    "response",
    "attachments",
    "license",
    "quota_scope",
}
QUOTA_PERIOD_FIELDS = {
    "period",
    "limit_credits_micro",
    "used_credits_micro",
    "remaining_credits_micro",
    "remaining_percentage",
    "next_reset",
    "warning",
    "exhausted",
}
QUOTA_WARNING_REQUIRED = {"tier", "period", "remaining_percentage", "warning", "exhausted"}
DONE_REQUIRED = {"usage", "effective_model", "selected_model", "quota_decision"}


# --------------------------------------------------------------------------
# Credit arithmetic and reserve formula (DESIGN §5.3, §5.4.1, §5.5.4)
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def credits_micro(input_tokens: int, output_tokens: int, in_mult: int, out_mult: int) -> int:
    """Canonical per-component rounding: ``ceil(in*im/1e6) + ceil(out*om/1e6)``."""
    return _ceil_div(input_tokens * in_mult, 1_000_000) + _ceil_div(output_tokens * out_mult, 1_000_000)


def model_credits(entry: dict[str, Any], input_tokens: int, output_tokens: int) -> int:
    return credits_micro(
        input_tokens,
        output_tokens,
        entry["input_tokens_credit_multiplier_micro"],
        entry["output_tokens_credit_multiplier_micro"],
    )


def estimated_text_tokens(entry: dict[str, Any], content: str) -> int:
    budgets = entry["estimation_budgets"]
    utf8_bytes = len(content.encode("utf-8"))
    base = _ceil_div(utf8_bytes, budgets["bytes_per_token_conservative"]) + budgets["fixed_overhead_tokens"]
    return _ceil_div(base * (100 + budgets["safety_margin_pct"]), 100)


def expected_reserve(
    entry: dict[str, Any],
    content: str,
    *,
    prior_context_tokens: int = 0,
    web_search: bool = False,
) -> dict[str, int]:
    """Preflight reserve of ``entry`` for a text-only message (no attachments)."""
    est_input = estimated_text_tokens(entry, content) + prior_context_tokens
    if web_search and entry["general_config"]["tool_support"]["web_search"]:
        est_input += entry["estimation_budgets"]["web_search_surcharge_tokens"]
    max_out = min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)
    return {
        "estimated_input_tokens": est_input,
        "max_output_tokens_applied": max_out,
        "reserve_tokens": est_input + max_out,
        "reserved_credits_micro": model_credits(entry, est_input, max_out),
        "minimal_generation_floor_applied": min(MINIMAL_GENERATION_FLOOR, max_out),
    }


# --------------------------------------------------------------------------
# Server / request helpers
# --------------------------------------------------------------------------


def _policy(**config: Any) -> dict[str, Any]:
    """Config patch of the static model-policy plugin."""
    return {"gears": {POLICY_PLUGIN: {"config": config}}}


def _entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the server catalog")


def _limits(server) -> dict[str, dict[str, int]]:
    cfg = server.config["gears"][POLICY_PLUGIN]["config"]
    return {
        "total": {
            "daily": cfg["default_standard_limits"]["limit_daily_credits_micro"],
            "monthly": cfg["default_standard_limits"]["limit_monthly_credits_micro"],
        },
        "premium": {
            "daily": cfg["default_premium_limits"]["limit_daily_credits_micro"],
            "monthly": cfg["default_premium_limits"]["limit_monthly_credits_micro"],
        },
    }


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _new_chat(server, user: str = "A1", model: str = PREMIUM_MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S3 quota", "model": model})
    assert resp.status_code == 201, resp.text
    assert resp.json()["model"] == model
    return resp.json()["id"]


def _body(content: str, request_id: str, web_search: bool | None = None) -> dict[str, Any]:
    body: dict[str, Any] = {"content": content, "request_id": request_id}
    if web_search is not None:
        body["web_search"] = {"enabled": web_search}
    return body


def _send(server, chat_id: str, content: str, *, user: str = "A1", request_id: str | None = None):
    request_id = request_id or new_uuid()
    result = server.stream("POST", _stream_path(chat_id), user, json=_body(content, request_id))
    return result, request_id


def _send_done(server, chat_id: str, content: str, *, user: str = "A1") -> tuple[dict[str, Any], Any, str]:
    """Send a message and assert a well-formed completed stream; return ``(done, result, request_id)``."""
    result, request_id = _send(server, chat_id, content, user=user)
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert result.first("stream_started")["request_id"] == request_id
    assert result.terminal is not None and result.terminal[0] == "done", (names, result.events[-1:])
    done = result.terminal[1]
    assert DONE_REQUIRED <= set(done), done
    return done, result, request_id


def _assert_allow(done: dict[str, Any], model: str) -> None:
    assert done["selected_model"] == model, done
    assert done["effective_model"] == model, done
    assert done["quota_decision"] == "allow", done
    assert done.get("downgrade_from") is None, done
    assert done.get("downgrade_reason") is None, done


def _turn(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def _turn_count(server, chat_id: str) -> int:
    return server.query("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?", (db_uuid(chat_id),))[0]["n"]


def _message(server, message_id: str) -> dict[str, Any]:
    rows = server.query("SELECT * FROM messages WHERE id = ?", (db_uuid(message_id),))
    assert len(rows) == 1, rows
    return rows[0]


def _assert_reserve_columns(turn: dict[str, Any], expected: dict[str, int], model: str) -> None:
    assert turn["reserve_tokens"] == expected["reserve_tokens"], (turn, expected)
    assert turn["max_output_tokens_applied"] == expected["max_output_tokens_applied"], (turn, expected)
    assert turn["reserved_credits_micro"] == expected["reserved_credits_micro"], (turn, expected)
    assert turn["minimal_generation_floor_applied"] == expected["minimal_generation_floor_applied"], turn
    assert turn["policy_version_applied"] == POLICY_VERSION, turn
    assert turn["effective_model"] == model, turn


def _stream_requests(mock) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("responses") if r["stream"]]


def _date_text(value: Any) -> str:
    if isinstance(value, (bytes, bytearray, memoryview)):
        value = bytes(value).decode("ascii")
    return str(value)[:10]


def _quota_rows(server, user: str) -> dict[tuple[str, str, str], dict[str, Any]]:
    """``quota_usage`` rows of ``user`` keyed by ``(period_type, period_start, bucket)``."""
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, "
        "calls, input_tokens, output_tokens, web_search_calls FROM quota_usage "
        "WHERE tenant_id = ? AND user_id = ?",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    return {(r["period_type"], _date_text(r["period_start"]), r["bucket"]): r for r in rows}


def _today() -> str:
    return datetime.now(timezone.utc).date().isoformat()


def _month_start() -> str:
    return datetime.now(timezone.utc).date().replace(day=1).isoformat()


def _period_keys(bucket: str) -> set[tuple[str, str, str]]:
    return {("daily", _today(), bucket), ("monthly", _month_start(), bucket)}


def _next_reset_ns(period: str) -> int:
    today = datetime.now(timezone.utc).date()
    if period == "daily":
        day = today + timedelta(days=1)
    else:
        day = (today.replace(day=28) + timedelta(days=4)).replace(day=1)
    return calendar.timegm((day.year, day.month, day.day, 0, 0, 0, 0, 0, 0)) * 1_000_000_000


def _enforcement_rows(rows: dict[tuple[str, str, str], dict[str, Any]], bucket: str) -> dict:
    return {key: row for key, row in rows.items() if key[2] == bucket}


def _quota_status(server, user: str) -> dict[str, Any]:
    resp = server.get(QUOTA_STATUS, user)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert set(body) == {"tiers", "warning_threshold_pct"}, body
    assert body["warning_threshold_pct"] == WARNING_THRESHOLD_PCT, body
    tiers = [t["tier"] for t in body["tiers"]]
    assert len(tiers) == len(set(tiers)), f"duplicate tiers: {body}"
    assert set(tiers) <= {"premium", "total"}, body
    for tier in body["tiers"]:
        assert set(tier) == {"tier", "periods"}, tier
        periods = [p["period"] for p in tier["periods"]]
        assert len(periods) == len(set(periods)), tier
        for period in tier["periods"]:
            assert set(period) == QUOTA_PERIOD_FIELDS, period
            assert period["period"] in ("daily", "monthly"), period
            assert is_rfc3339(period["next_reset"]), period
    return body


def _status_periods(body: dict[str, Any]) -> dict[tuple[str, str], dict[str, Any]]:
    return {(t["tier"], p["period"]): p for t in body["tiers"] for p in t["periods"]}


def _expected_period(limit: int, used: int) -> dict[str, Any]:
    remaining = max(limit - used, 0)
    pct = remaining * 100 // limit
    return {
        "limit_credits_micro": limit,
        "used_credits_micro": used,
        "remaining_credits_micro": remaining,
        "remaining_percentage": pct,
        "warning": pct <= 100 - WARNING_THRESHOLD_PCT,
        "exhausted": pct == 0,
    }


def _assert_status_period(actual: dict[str, Any], period: str, limit: int, used: int) -> None:
    expected = _expected_period(limit, used)
    for key, value in expected.items():
        assert actual[key] == value, (key, actual, expected)
    assert actual["period"] == period
    assert parse_ts_ns(actual["next_reset"]) == _next_reset_ns(period), actual


def _bucket_used(server, user: str, bucket: str, period: str) -> int:
    start = _today() if period == "daily" else _month_start()
    row = _quota_rows(server, user).get((period, start, bucket))
    if row is None:
        return 0
    return row["spent_credits_micro"] + row["reserved_credits_micro"]


# --------------------------------------------------------------------------
# Outbox and plugin log observation
# --------------------------------------------------------------------------


class OutboxRecorder:
    """Polls the outbox bodies of one chat in a background thread.

    Usage and audit bodies are vacuumed shortly after the handler acknowledges
    them, so the payloads are captured while they are still stored.
    """

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="outbox-recorder", daemon=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.poll()
            time.sleep(self._interval)

    def poll(self) -> None:
        try:
            payloads = outbox_payloads_for_chat(self._server, self._chat_id)
        except Exception:  # noqa: BLE001 - transient SQLite contention, polled again
            return
        with self._lock:
            for payload in payloads:
                self._seen.setdefault(json.dumps(payload, sort_keys=True), payload)

    def __enter__(self) -> "OutboxRecorder":
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def payloads(self) -> list[dict[str, Any]]:
        self.poll()
        with self._lock:
            return list(self._seen.values())

    def usage_events(self, request_id: str) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if "dedupe_key" in p and str(p.get("request_id", "")).lower() == request_id.lower()
        ]

    def audit_events(self, request_id: str, event_type: str) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if p.get("event_type") == event_type
            and str(p.get("request_id", "")).lower() == request_id.lower()
        ]


def _wait_usage_event(recorder: OutboxRecorder, request_id: str) -> dict[str, Any]:
    events = wait_until(
        lambda: recorder.usage_events(request_id),
        timeout=15,
        interval=0.05,
        description=f"usage outbox payload of request {request_id}",
    )
    assert len(events) == 1, events
    return events[0]


def _wait_audit_event(recorder: OutboxRecorder, request_id: str, event_type: str) -> dict[str, Any]:
    events = wait_until(
        lambda: recorder.audit_events(request_id, event_type),
        timeout=15,
        interval=0.05,
        description=f"{event_type} audit outbox payload of request {request_id}",
    )
    assert len(events) == 1, events
    return events[0]


def _log_text(server) -> str:
    texts = [server.log_text()]
    log_dir = Path(server.work_dir) / "logs"
    for path in sorted(log_dir.glob("*.log")):
        if path != server.log_path:
            texts.append(path.read_text(errors="replace"))
    return "\n".join(texts)


def _wait_log_line(server, *needles: str, timeout: float = 20.0) -> str:
    def probe() -> str | None:
        for line in _log_text(server).splitlines():
            if all(needle in line for needle in needles):
                return line
        return None

    return wait_until(probe, timeout=timeout, interval=0.2, description=f"log line with {needles}")


def _wait_queue_activity(server, queue: str, before: int, expected_delta: int) -> None:
    wait_until(
        lambda: outbox_snapshot(server, queue).activity >= before + expected_delta,
        timeout=15,
        description=f"{expected_delta} message(s) enqueued on {queue!r}",
    )
    time.sleep(1.0)
    after = outbox_snapshot(server, queue)
    assert after.registered_partitions > 0, f"queue {queue!r} is not registered"
    assert after.activity - before == expected_delta, (queue, before, after)


def _uuid(value: Any) -> str:
    return str(uuid.UUID(str(value)))


# --------------------------------------------------------------------------
# Reserve and settlement
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_completed_turn_persists_reserve_and_settles_quota_buckets(start_server, mock):
    server = start_server(name="quota-buckets")
    premium = _entry(server, PREMIUM_MODEL)
    standard = _entry(server, STANDARD_MODEL)

    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    content = "How many credits does this turn cost?"
    mock.enqueue({"text": "Exactly what the provider reports.", "usage": {"input_tokens": 321, "output_tokens": 45}})
    done, result, request_id = _send_done(server, chat_id, content)

    _assert_allow(done, PREMIUM_MODEL)
    assert done["usage"] == {"input_tokens": 321, "output_tokens": 45}, done
    turn = _turn(server, chat_id, request_id)
    assert turn["state"] == "completed", turn
    _assert_reserve_columns(turn, expected_reserve(premium, content), PREMIUM_MODEL)
    assert _message(server, result.first("stream_started")["message_id"])["model"] == PREMIUM_MODEL

    actual = model_credits(premium, 321, 45)
    rows = _quota_rows(server, "A1")
    enforcement = {k: r for k, r in rows.items() if k[2] in ("total", "tier:premium")}
    assert set(enforcement) == _period_keys("total") | _period_keys("tier:premium"), sorted(rows)
    for (period, _start, bucket), row in enforcement.items():
        assert row["reserved_credits_micro"] == 0, (period, bucket, row)
        assert row["spent_credits_micro"] == actual, (period, bucket, row)
        assert row["calls"] == 1, (period, bucket, row)
        assert row["web_search_calls"] == 0, (period, bucket, row)
        if bucket == "total":
            assert (row["input_tokens"], row["output_tokens"]) == (321, 45), (period, row)
        else:
            assert (row["input_tokens"], row["output_tokens"]) == (0, 0), (period, row)

    # A standard-tier turn of another user debits only that user's `total` rows.
    mini_chat = _new_chat(server, "A2", STANDARD_MODEL)
    mock.enqueue({"text": "Cheap answer.", "usage": {"input_tokens": 100, "output_tokens": 20}})
    mini_content = "And a standard one?"
    mini_done, _, mini_request = _send_done(server, mini_chat, mini_content, user="A2")
    _assert_allow(mini_done, STANDARD_MODEL)
    _assert_reserve_columns(
        _turn(server, mini_chat, mini_request), expected_reserve(standard, mini_content), STANDARD_MODEL
    )
    a2_rows = _quota_rows(server, "A2")
    assert set(_enforcement_rows(a2_rows, "total")) == _period_keys("total"), sorted(a2_rows)
    for row in _enforcement_rows(a2_rows, "total").values():
        assert row["spent_credits_micro"] == model_credits(standard, 100, 20), row
        assert row["reserved_credits_micro"] == 0 and row["calls"] == 1, row
        assert (row["input_tokens"], row["output_tokens"]) == (100, 20), row
    for row in _enforcement_rows(a2_rows, "tier:premium").values():
        assert row["spent_credits_micro"] == 0 and row["reserved_credits_micro"] == 0, row
        assert row["calls"] == 0, row
    # A1's rows are untouched by A2's turn.
    assert _quota_rows(server, "A1") == rows


def test_reserve_includes_prior_context_tokens_of_latest_assistant_message(mc, mock):
    premium = _entry(mc, PREMIUM_MODEL)
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)

    first = "First question about reserves"
    mock.enqueue({"text": "First answer.", "usage": {"input_tokens": 400, "output_tokens": 60}})
    _, _, first_request = _send_done(mc, chat_id, first)
    _assert_reserve_columns(_turn(mc, chat_id, first_request), expected_reserve(premium, first), PREMIUM_MODEL)

    # The latest assistant message with non-zero usage (400 + 60) stands in for the history.
    second = "Second question"
    mock.enqueue({"text": "Second answer.", "usage": {"input_tokens": 0, "output_tokens": 0}})
    _, _, second_request = _send_done(mc, chat_id, second)
    _assert_reserve_columns(
        _turn(mc, chat_id, second_request),
        expected_reserve(premium, second, prior_context_tokens=460),
        PREMIUM_MODEL,
    )

    # The second assistant message has zero usage, so it is skipped.
    third = "Third question"
    _, _, third_request = _send_done(mc, chat_id, third)
    _assert_reserve_columns(
        _turn(mc, chat_id, third_request),
        expected_reserve(premium, third, prior_context_tokens=460),
        PREMIUM_MODEL,
    )


# --------------------------------------------------------------------------
# Usage and audit outbox
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_completed_turn_enqueues_one_usage_and_one_audit_message(start_server, mock):
    server = start_server(name="quota-outbox")
    premium = _entry(server, PREMIUM_MODEL)
    ident = server.identity("A1")
    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    usage = {"input_tokens": 250, "output_tokens": 40, "cached_tokens": 64, "reasoning_tokens": 12}
    mock.enqueue({"text": "Billed answer.", "usage": usage})
    usage_before = outbox_snapshot(server, USAGE_QUEUE).activity
    audit_before = outbox_snapshot(server, AUDIT_QUEUE).activity
    started_ns = time.time_ns()

    with OutboxRecorder(server, chat_id) as recorder:
        done, _, request_id = _send_done(server, chat_id, "Bill me exactly once")
        event = _wait_usage_event(recorder, request_id)
        audit = _wait_audit_event(recorder, request_id, "turn_completed")
        _wait_queue_activity(server, USAGE_QUEUE, usage_before, 1)
        _wait_queue_activity(server, AUDIT_QUEUE, audit_before, 1)
        assert len(recorder.usage_events(request_id)) == 1
        assert len(recorder.audit_events(request_id, "turn_completed")) == 1
    _assert_allow(done, PREMIUM_MODEL)
    turn = _turn(server, chat_id, request_id)
    turn_id = turn["id"]
    expected_usage = {
        "input_tokens": 250,
        "output_tokens": 40,
        "cache_read_input_tokens": 64,
        "cache_write_input_tokens": 0,
        "reasoning_tokens": 12,
    }
    actual = model_credits(premium, 250, 40)

    # Usage event (SDK `UsageEvent`); system_task_type is omitted for user turns.
    assert set(event) == USAGE_EVENT_FIELDS, event
    assert _uuid(event["tenant_id"]) == ident.tenant_id
    assert _uuid(event["user_id"]) == ident.user_id
    assert _uuid(event["chat_id"]) == chat_id
    assert _uuid(event["turn_id"]) == turn_id
    assert _uuid(event["request_id"]) == request_id
    assert event["effective_model"] == PREMIUM_MODEL
    assert event["selected_model"] == PREMIUM_MODEL
    assert event["terminal_state"] == "completed"
    assert event["billing_outcome"] == "completed"
    assert event["settlement_method"] == "actual"
    assert event["usage"] == expected_usage, event
    assert set(event["usage"]) == USAGE_TOKENS_FIELDS
    assert event["actual_credits_micro"] == actual, event
    assert event["policy_version_applied"] == POLICY_VERSION
    assert (event["web_search_calls"], event["code_interpreter_calls"], event["file_search_calls"]) == (0, 0, 0)
    assert event["requester_type"] == "user"
    assert event["dedupe_key"] == (
        f"{uuid.UUID(ident.tenant_id).hex}/{uuid.UUID(turn_id).hex}/{uuid.UUID(request_id).hex}"
    ), event
    assert is_rfc3339(event["timestamp"]), event
    assert started_ns - 5_000_000_000 <= parse_ts_ns(event["timestamp"]) <= time.time_ns() + 5_000_000_000

    # Turn audit event (SDK `TurnAuditEvent`); no content reaches the plugin in P1.
    assert set(audit) == TURN_AUDIT_FIELDS, audit
    assert audit["event_type"] == "turn_completed"
    assert _uuid(audit["tenant_id"]) == ident.tenant_id
    assert _uuid(audit["user_id"]) == ident.user_id
    assert _uuid(audit["chat_id"]) == chat_id
    assert _uuid(audit["turn_id"]) == turn_id
    assert _uuid(audit["request_id"]) == request_id
    assert audit["requester_type"] == "user"
    assert audit["selected_model"] == PREMIUM_MODEL
    assert audit["effective_model"] == PREMIUM_MODEL
    assert audit["usage"] == expected_usage, audit
    assert isinstance(audit["latency_ms"], int) and audit["latency_ms"] >= 0, audit
    assert audit["tool_calls"] == {"web_search_calls": 0, "file_search_calls": 0}, audit
    assert audit["policy_decisions"] == {
        "quota": {"decision": "allow", "downgrade_from": None, "downgrade_reason": None}
    }, audit
    assert audit["error_code"] is None
    assert (audit["prompt"], audit["response"], audit["attachments"]) == ("", "", [])
    assert audit["license"] is None and audit["quota_scope"] is None
    assert is_rfc3339(audit["timestamp"]), audit

    # Both messages are delivered to the static plugins, which log them.
    usage_line = _wait_log_line(server, "usage event published", event["dedupe_key"])
    assert str(actual) in usage_line, usage_line
    _wait_log_line(server, "audit event", "turn_completed", request_id)


# --------------------------------------------------------------------------
# Downgrade cascade and kill switches
# --------------------------------------------------------------------------


def _assert_downgraded_to_standard(server, mock, chat_id: str, done, result, request_id, content, reason):
    standard = _entry(server, STANDARD_MODEL)
    assert done["selected_model"] == PREMIUM_MODEL, done
    assert done["effective_model"] == STANDARD_MODEL, done
    assert done["quota_decision"] == "downgrade", done
    assert done["downgrade_from"] == PREMIUM_MODEL, done
    assert done["downgrade_reason"] == reason, done
    assert _stream_requests(mock)[-1]["json"]["model"] == standard["provider_model_id"]
    assert _message(server, result.first("stream_started")["message_id"])["model"] == STANDARD_MODEL
    _assert_reserve_columns(_turn(server, chat_id, request_id), expected_reserve(standard, content), STANDARD_MODEL)
    # Only the overall cap is debited for a standard-tier turn.
    rows = _quota_rows(server, "A1")
    for row in _enforcement_rows(rows, "tier:premium").values():
        assert row["spent_credits_micro"] == 0 and row["reserved_credits_micro"] == 0, row
    total = _enforcement_rows(rows, "total")
    assert set(total) == _period_keys("total"), sorted(rows)
    for row in total.values():
        assert row["spent_credits_micro"] == model_credits(standard, 120, 30), row
        assert row["reserved_credits_micro"] == 0, row


@pytest.mark.slow
def test_premium_quota_exhausted_downgrades_to_standard(start_server, mock, model_catalog):
    content = "Downgrade me please"
    premium = next(e for e in model_catalog if e["id"] == PREMIUM_MODEL)
    premium_reserve = expected_reserve(premium, content)["reserved_credits_micro"]
    server = start_server(
        _policy(default_premium_limits={"limit_daily_credits_micro": premium_reserve - 1}),
        name="quota-downgrade",
    )
    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    mock.enqueue({"text": "Served by the standard model.", "usage": {"input_tokens": 120, "output_tokens": 30}})

    done, result, request_id = _send_done(server, chat_id, content)

    _assert_downgraded_to_standard(
        server, mock, chat_id, done, result, request_id, content, "premium_quota_exhausted"
    )


@pytest.mark.slow
@pytest.mark.parametrize("switch", ["force_standard_tier", "disable_premium_tier"])
def test_premium_kill_switch_downgrades_to_standard(start_server, mock, switch):
    server = start_server(_policy(kill_switches={switch: True}), name=f"quota-{switch}")
    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    content = f"Kill switch {switch}"
    mock.enqueue({"text": "Standard tier only.", "usage": {"input_tokens": 120, "output_tokens": 30}})

    done, result, request_id = _send_done(server, chat_id, content)

    _assert_downgraded_to_standard(server, mock, chat_id, done, result, request_id, content, switch)


def test_disabled_selected_model_is_downgraded_with_model_disabled(mc, mock):
    """The chat's model was disabled in the catalog after the chat was created.

    The cascade starts at the disabled model's tier (premium) and picks the
    enabled default model of that tier (DESIGN "Downgrade Decision Flow" 3b).
    """
    ident = mc.identity("A1")
    chat_id = seed_chat(mc, ident.tenant_id, ident.user_id, model=DISABLED_MODEL, title="Disabled model")
    premium = _entry(mc, PREMIUM_MODEL)
    content = "Is my model still there?"

    done, result, request_id = _send_done(mc, chat_id, content)

    assert done["selected_model"] == DISABLED_MODEL, done
    assert done["effective_model"] == PREMIUM_MODEL, done
    assert done["quota_decision"] == "downgrade", done
    assert done["downgrade_from"] == DISABLED_MODEL, done
    assert done["downgrade_reason"] == "model_disabled", done
    assert _stream_requests(mock)[-1]["json"]["model"] == premium["provider_model_id"]
    assert _message(mc, result.first("stream_started")["message_id"])["model"] == PREMIUM_MODEL
    _assert_reserve_columns(_turn(mc, chat_id, request_id), expected_reserve(premium, content), PREMIUM_MODEL)


@pytest.mark.slow
def test_all_tiers_exhausted_rejects_with_429_before_streaming(start_server, mock):
    tiny = {"limit_daily_credits_micro": 1_000, "limit_monthly_credits_micro": 1_000}
    server = start_server(
        _policy(default_standard_limits=tiny, default_premium_limits=tiny), name="quota-exhausted"
    )

    for model in (PREMIUM_MODEL, STANDARD_MODEL):
        chat_id = _new_chat(server, "A1", model)
        resp = server.post(_stream_path(chat_id), "A1", json=_body("Anything left?", new_uuid()))

        assert not resp.headers.get("content-type", "").startswith("text/event-stream"), resp.headers
        problem = assert_problem(resp, 429, category="resource_exhausted", violation_subject="tokens")
        violation = problem["context"]["violations"][0]
        assert violation["subject"] == "tokens", problem
        assert violation["description"] == "quota_exceeded", problem
        assert _turn_count(server, chat_id) == 0
    assert _stream_requests(mock) == []
    for row in _quota_rows(server, "A1").values():
        assert row["reserved_credits_micro"] == 0 and row["spent_credits_micro"] == 0, row


# --------------------------------------------------------------------------
# Overshoot tolerance (DESIGN §5.4.5)
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_overshoot_beyond_tolerance_is_capped_and_within_tolerance_is_actual(start_server, mock):
    server = start_server(name="quota-overshoot")
    standard = _entry(server, STANDARD_MODEL)
    chat_id = _new_chat(server, "A1", STANDARD_MODEL)

    # Turn 1: usage far above the reserve (> 1.10x) -> billed at the reserve.
    first = "Overshoot please"
    reserve1 = expected_reserve(standard, first)
    in1, out1 = 200, 40_000
    assert (in1 + out1) / reserve1["reserve_tokens"] > OVERSHOOT_TOLERANCE
    assert model_credits(standard, in1, out1) > reserve1["reserved_credits_micro"]
    mock.enqueue({"text": "Very long answer.", "usage": {"input_tokens": in1, "output_tokens": out1}})

    with OutboxRecorder(server, chat_id) as recorder:
        done1, result1, request1 = _send_done(server, chat_id, first)
        event1 = _wait_usage_event(recorder, request1)

        assert done1["usage"] == {"input_tokens": in1, "output_tokens": out1}
        turn1 = _turn(server, chat_id, request1)
        assert turn1["state"] == "completed", turn1
        _assert_reserve_columns(turn1, reserve1, STANDARD_MODEL)
        message1 = _message(server, result1.first("stream_started")["message_id"])
        assert (message1["input_tokens"], message1["output_tokens"]) == (in1, out1)
        assert event1["terminal_state"] == "completed" and event1["billing_outcome"] == "completed"
        assert event1["settlement_method"] == "actual", event1
        assert event1["actual_credits_micro"] == reserve1["reserved_credits_micro"], event1
        assert event1["usage"]["input_tokens"] == in1 and event1["usage"]["output_tokens"] == out1
        for row in _enforcement_rows(_quota_rows(server, "A1"), "total").values():
            assert row["spent_credits_micro"] == reserve1["reserved_credits_micro"], row
            assert row["reserved_credits_micro"] == 0, row
            assert (row["input_tokens"], row["output_tokens"]) == (in1, out1), row

        # Turn 2: overshoot within the tolerance -> billed at the actual usage.
        second = "Within tolerance"
        reserve2 = expected_reserve(standard, second, prior_context_tokens=in1 + out1)
        out2 = 33_000
        in2 = int(reserve2["reserve_tokens"] * 1.05) - out2
        factor = (in2 + out2) / reserve2["reserve_tokens"]
        assert 1.0 < factor <= OVERSHOOT_TOLERANCE, factor
        actual2 = model_credits(standard, in2, out2)
        assert actual2 > reserve2["reserved_credits_micro"]
        mock.enqueue({"text": "Slightly long answer.", "usage": {"input_tokens": in2, "output_tokens": out2}})

        _, _, request2 = _send_done(server, chat_id, second)
        event2 = _wait_usage_event(recorder, request2)

    _assert_reserve_columns(_turn(server, chat_id, request2), reserve2, STANDARD_MODEL)
    assert event2["settlement_method"] == "actual", event2
    assert event2["actual_credits_micro"] == actual2, event2
    for row in _enforcement_rows(_quota_rows(server, "A1"), "total").values():
        assert row["spent_credits_micro"] == reserve1["reserved_credits_micro"] + actual2, row
        assert row["reserved_credits_micro"] == 0 and row["calls"] == 2, row
        assert (row["input_tokens"], row["output_tokens"]) == (in1 + in2, out1 + out2), row


# --------------------------------------------------------------------------
# Quota status endpoint and done.quota_warnings
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_quota_status_reports_usage_per_tier_and_period(start_server, mock):
    server = start_server(name="quota-status")
    limits = _limits(server)
    premium = _entry(server, PREMIUM_MODEL)

    # No usage yet: every period is full.
    for user in ("A1", "A2"):
        periods = _status_periods(_quota_status(server, user))
        assert set(periods) == {(t, p) for t in ("premium", "total") for p in ("daily", "monthly")}, periods
        for (tier, period), status in periods.items():
            _assert_status_period(status, period, limits[tier][period], 0)

    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    mock.enqueue({"text": "Counted.", "usage": {"input_tokens": 500, "output_tokens": 100}})
    _send_done(server, chat_id, "Count this turn")
    spent = model_credits(premium, 500, 100)

    periods = _status_periods(_quota_status(server, "A1"))
    for (tier, period), status in periods.items():
        bucket = "tier:premium" if tier == "premium" else "total"
        used = _bucket_used(server, "A1", bucket, period)
        assert used == spent, (tier, period, used)
        _assert_status_period(status, period, limits[tier][period], used)

    # Scoped to the caller: A2 (same tenant) still sees zero usage.
    for (tier, period), status in _status_periods(_quota_status(server, "A2")).items():
        _assert_status_period(status, period, limits[tier][period], 0)

    # used_credits_micro counts the in-flight reserve (spent + reserved).
    b1_chat = _new_chat(server, "B1", PREMIUM_MODEL)
    mock.enqueue({"text": "Slow answer.", "initial_delay_ms": 4_000, "usage": {"input_tokens": 10, "output_tokens": 10}})
    holder: dict[str, Any] = {}
    b1_request = new_uuid()

    def run() -> None:
        with httpx.Client(timeout=60) as client:
            holder["result"] = stream_sse(
                client,
                "POST",
                server.url(_stream_path(b1_chat)),
                headers=server.headers("B1"),
                json=_body("Slow one", b1_request),
            )

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    try:
        running = wait_until(
            lambda: next(
                (t for t in server.query(
                    "SELECT * FROM chat_turns WHERE chat_id = ?", (db_uuid(b1_chat),)
                ) if t["state"] == "running"),
                None,
            ),
            timeout=15,
            interval=0.05,
            description="running B1 turn",
        )
        reserved = running["reserved_credits_micro"]
        assert reserved == expected_reserve(premium, "Slow one")["reserved_credits_micro"], running
        in_flight = wait_until(
            lambda: (
                lambda periods: periods
                if periods[("premium", "daily")]["used_credits_micro"] == reserved
                else None
            )(_status_periods(_quota_status(server, "B1"))),
            timeout=10,
            interval=0.1,
            description="quota status counting the in-flight reserve",
        )
        for (tier, period), status in in_flight.items():
            _assert_status_period(status, period, limits[tier][period], reserved)
    finally:
        thread.join(timeout=60)
    result = holder.get("result")
    assert result is not None and result.terminal is not None and result.terminal[0] == "done", result
    for (tier, period), status in _status_periods(_quota_status(server, "B1")).items():
        _assert_status_period(status, period, limits[tier][period], model_credits(premium, 10, 10))


def test_quota_status_without_token_is_unauthenticated(mc):
    resp = mc.get(QUOTA_STATUS, None)

    assert_problem(resp, 401, category="unauthenticated")


@pytest.mark.slow
def test_quota_periods_with_non_positive_limit_are_omitted(start_server, mock):
    server = start_server(
        _policy(default_premium_limits={"limit_monthly_credits_micro": 0}), name="quota-zero-limit"
    )

    periods = _status_periods(_quota_status(server, "A1"))

    assert set(periods) == {("premium", "daily"), ("total", "daily"), ("total", "monthly")}, periods

    chat_id = _new_chat(server, "A1", STANDARD_MODEL)
    done, _, _ = _send_done(server, chat_id, "Standard turn")
    warnings = done.get("quota_warnings") or []
    assert ("premium", "monthly") not in {(w["tier"], w["period"]) for w in warnings}, warnings


@pytest.mark.slow
def test_quota_warnings_in_done_and_status_after_crossing_threshold(start_server, mock, model_catalog):
    content = "Warn me"
    premium_daily = 500_000
    premium = next(e for e in model_catalog if e["id"] == PREMIUM_MODEL)
    reserve = expected_reserve(premium, content)
    assert reserve["reserved_credits_micro"] <= premium_daily, reserve
    server = start_server(
        _policy(default_premium_limits={"limit_daily_credits_micro": premium_daily}), name="quota-warning"
    )
    limits = _limits(server)

    # A1: 453_000 of 500_000 used -> 9 % remaining: warning, not exhausted.
    # A2: 499_200 of 500_000 used -> 0 % remaining (floored): warning and exhausted.
    scenarios = {
        "A1": ({"input_tokens": 1_000, "output_tokens": 30_000}, 9, False),
        "A2": ({"input_tokens": 1_400, "output_tokens": 33_000}, 0, True),
    }
    for user, (usage, expected_pct, expected_exhausted) in scenarios.items():
        tokens = usage["input_tokens"] + usage["output_tokens"]
        assert tokens / reserve["reserve_tokens"] <= OVERSHOOT_TOLERANCE
        spent = model_credits(premium, usage["input_tokens"], usage["output_tokens"])
        chat_id = _new_chat(server, user, PREMIUM_MODEL)
        mock.enqueue({"text": "Expensive answer.", "usage": usage})

        done, _, _ = _send_done(server, chat_id, content, user=user)

        _assert_allow(done, PREMIUM_MODEL)
        warnings = done.get("quota_warnings")
        assert isinstance(warnings, list), done
        by_key = {(w["tier"], w["period"]): w for w in warnings}
        assert len(by_key) == len(warnings), warnings
        assert set(by_key) == {(t, p) for t in ("premium", "total") for p in ("daily", "monthly")}, warnings
        status = _status_periods(_quota_status(server, user))
        for (tier, period), warning in by_key.items():
            keys = set(warning)
            assert QUOTA_WARNING_REQUIRED <= keys <= QUOTA_WARNING_REQUIRED | {"next_reset"}, warning
            expected = _expected_period(limits[tier][period], spent)
            assert warning["remaining_percentage"] == expected["remaining_percentage"], (warning, expected)
            assert warning["warning"] == expected["warning"], (warning, expected)
            assert warning["exhausted"] == expected["exhausted"], (warning, expected)
            if warning["warning"] or warning["exhausted"]:
                assert parse_ts_ns(warning["next_reset"]) == _next_reset_ns(period), warning
            else:
                assert warning.get("next_reset") is None, warning
            # The status endpoint reports the same data.
            for key in ("remaining_percentage", "warning", "exhausted"):
                assert status[(tier, period)][key] == warning[key], (tier, period, status, warning)
            _assert_status_period(status[(tier, period)], period, limits[tier][period], spent)

        premium_daily_warning = by_key[("premium", "daily")]
        assert premium_daily_warning["remaining_percentage"] == expected_pct, premium_daily_warning
        assert premium_daily_warning["warning"] is True, premium_daily_warning
        assert premium_daily_warning["exhausted"] is expected_exhausted, premium_daily_warning
        assert by_key[("total", "daily")]["warning"] is False, by_key
