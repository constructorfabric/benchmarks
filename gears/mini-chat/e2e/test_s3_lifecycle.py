"""S3 streaming: turn lifecycle, client disconnect and settlement of aborted / failed turns.

Contract sources (gears/mini-chat/docs/DESIGN.md):

* "SSE stream close rules" (client disconnect before terminal -> ``cancelled``
  via the CAS finalizer, nothing more is sent) and §3.6 "Streaming
  Cancellation" (hard cancel of the provider connection, partial response
  persisted, estimated settlement);
* §4 "Turn Lifecycle, Crash Recovery and Orphan Handling" (states and
  transitions) and the Turn Status API (internal -> API state mapping,
  ``error_code`` / ``assistant_message_id`` omitted when null, ``updated_at``);
* §3.7 ``chat_turns`` (preflight columns, ``last_progress_at`` set when the
  turn is created) and ``quota_usage`` (reserve at preflight, settlement:
  ``reserved -= turn reserve``, ``spent += charged``, ``calls += 1``, token
  telemetry only on actual settlements);
* §5.4.1 / §5.5.4 (reserve: ``estimated_text_tokens``, ``reserve_tokens``,
  ``reserved_credits_micro``), §5.3 ``credits_micro`` (per-component ceil);
* §5.7 "Terminal outcome taxonomy", "FinalizeTurn Invariant" (cancelled with
  non-empty text persists the partial assistant message and sets
  ``assistant_message_id``), usage accounting rules per outcome;
* §5.8 "Deterministic Charged Token Formula" and "Outbox Emission Requirement
  (Aborted Streams)" (``billing_outcome = aborted``, ``settlement_method =
  estimated``, ``usage = null``, credits ``credits_micro(reserve_tokens -
  max_output_tokens_applied, minimal_generation_floor_applied, in, out)``);
* §5.9 "Post-Stream Terminal Error Reconciliation Rule" (provider-reported
  usage on ``response.failed`` -> ``actual`` settlement);
* §5.6 ``UsageEvent`` payload and ``dedupe_key``.

Settlement-sensitive tests run as ``B1`` (tenant B) so their ``quota_usage``
rows are not shared with the A-tenant tests of other modules.
"""

from __future__ import annotations

import re
import uuid
from typing import Any, Iterator

import httpx
import pytest

from mc_helpers import (
    SseResult,
    db_uuid,
    new_uuid,
    parse_sse_lines,
    wait_until,
)
from mc_seed import is_rfc3339, outbox_payloads_for_chat, parse_ts_ns

CHATS = "/mini-chat/v1/chats"
MODEL = "gpt-4.1-mini"  # standard tier: settles bucket ``total`` only
BILLING_USER = "B1"
TURN_STATUS_FIELDS = {"request_id", "state", "updated_at", "error_code", "assistant_message_id"}
DEFAULT_STREAMING_MAX_OUTPUT_TOKENS = 32_768
DEFAULT_MINIMAL_GENERATION_FLOOR = 50
#: Long enough to observe a running turn; bounded so the mock never lingers.
HANG = {"kind": "hang", "hang_max_secs": 90}
#: Without content events the server first writes again at the next ``ping``
#: (``sse_ping_interval_seconds``, default 15 s), so a disconnect during a
#: silent provider phase may only be noticed then.
SILENT_DISCONNECT_TIMEOUT = 45.0


# --------------------------------------------------------------------------
# Arithmetic (§5.3, §5.4.1, §5.5.4)
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return (n + d - 1) // d


def credits_micro(input_tokens: int, output_tokens: int, in_mult: int, out_mult: int) -> int:
    return _ceil_div(input_tokens * in_mult, 1_000_000) + _ceil_div(output_tokens * out_mult, 1_000_000)


def _estimated_text_tokens(content: str, budgets: dict[str, Any]) -> int:
    base = _ceil_div(len(content.encode("utf-8")), int(budgets["bytes_per_token_conservative"]))
    base += int(budgets["fixed_overhead_tokens"])
    return _ceil_div(base * (100 + int(budgets["safety_margin_pct"])), 100)


def _catalog_entry(server, model: str) -> dict[str, Any]:
    return next(e for e in server.catalog if e["id"] == model)


def _max_output_tokens_applied(server, model: str) -> int:
    cap = int(
        server.gear_config.get("streaming", {}).get(
            "max_output_tokens", DEFAULT_STREAMING_MAX_OUTPUT_TOKENS
        )
    )
    return min(int(_catalog_entry(server, model)["max_output_tokens"]), cap)


def _minimal_generation_floor(server) -> int:
    return int(
        server.gear_config.get("estimation_budgets", {}).get(
            "minimal_generation_floor", DEFAULT_MINIMAL_GENERATION_FLOOR
        )
    )


def _multipliers(server, model: str) -> tuple[int, int]:
    entry = _catalog_entry(server, model)
    return (
        int(entry["input_tokens_credit_multiplier_micro"]),
        int(entry["output_tokens_credit_multiplier_micro"]),
    )


def _estimated_charge(server, row: dict[str, Any]) -> int:
    """§5.8 estimated credits from the persisted ``chat_turns`` columns."""
    in_mult, out_mult = _multipliers(server, row["effective_model"])
    estimated_input = int(row["reserve_tokens"]) - int(row["max_output_tokens_applied"])
    charged_output = int(row["minimal_generation_floor_applied"])
    assert estimated_input + charged_output <= int(row["reserve_tokens"]), row
    return credits_micro(estimated_input, charged_output, in_mult, out_mult)


# --------------------------------------------------------------------------
# HTTP / SSE helpers
# --------------------------------------------------------------------------


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def _new_chat(server, user: str, model: str = MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S3 lifecycle", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


class LiveStream:
    """An open ``messages:stream`` response read event by event.

    Leaving the ``with`` block closes the connection: a client disconnect when
    the terminal event was not read yet.
    """

    def __init__(self, server, chat_id: str, body: dict[str, Any], user: str) -> None:
        self._cm = server.client.stream(
            "POST",
            server.url(_stream_path(chat_id)),
            headers=server.headers(user),
            json=body,
            timeout=httpx.Timeout(60.0, connect=5.0),
        )
        self.events: list[tuple[str, Any]] = []
        self._iter: Iterator[tuple[str, Any]] | None = None

    def __enter__(self) -> "LiveStream":
        resp = self._cm.__enter__()
        content_type = resp.headers.get("content-type", "")
        if resp.status_code != 200 or not content_type.startswith("text/event-stream"):
            resp.read()
            text = resp.text
            self._cm.__exit__(None, None, None)
            raise AssertionError(f"expected an SSE stream, got HTTP {resp.status_code}: {text[:1000]}")
        self._iter = parse_sse_lines(resp.iter_lines())
        return self

    def next_event(self, skip_ping: bool = True) -> tuple[str, Any]:
        assert self._iter is not None
        while True:
            event = next(self._iter)
            self.events.append(event)
            if skip_ping and event[0] == "ping":
                continue
            return event

    def __exit__(self, *exc: Any) -> None:
        self._cm.__exit__(*exc)


def _stream_requests(mock) -> list[dict[str, Any]]:
    """Streaming LLM calls recorded by the mock (thread summaries are non-streaming)."""
    return [r for r in mock.requests_for("responses") if r.get("stream")]


def _wait_stream_requests(mock, count: int) -> list[dict[str, Any]]:
    return wait_until(
        lambda: (found := _stream_requests(mock)) and len(found) >= count and found,
        timeout=15.0,
        interval=0.05,
        description=f"{count} streaming provider request(s)",
    )


def _open_and_read_started(live: LiveStream, request_id: str) -> dict[str, Any]:
    name, started = live.next_event()
    assert name == "stream_started", live.events
    assert started["request_id"] == request_id, started
    assert started["is_new_turn"] is True, started
    assert uuid.UUID(started["message_id"]), started
    return started


def _stream(server, chat_id: str, user: str, content: str, request_id: str) -> SseResult:
    return server.stream(
        "POST", _stream_path(chat_id), user, json={"content": content, "request_id": request_id}
    )


def _turn_status(server, chat_id: str, request_id: str, user: str) -> dict[str, Any]:
    resp = server.get(_turn_path(chat_id, request_id), user)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert set(body) <= TURN_STATUS_FIELDS, f"unexpected Turn Status fields: {body}"
    assert {"request_id", "state", "updated_at"} <= set(body), body
    assert body["request_id"] == request_id, body
    assert is_rfc3339(body["updated_at"]), body
    return body


def _wait_terminal_status(
    server, chat_id: str, request_id: str, user: str, timeout: float
) -> dict[str, Any]:
    def probe() -> dict[str, Any] | None:
        body = _turn_status(server, chat_id, request_id, user)
        return body if body["state"] != "running" else None

    return wait_until(probe, timeout=timeout, interval=0.2, description=f"turn {request_id} to finish")


# --------------------------------------------------------------------------
# DB helpers
# --------------------------------------------------------------------------


def _turn_row(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def _messages_of_turn(server, chat_id: str, request_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT id, role, content, request_id, model, deleted_at FROM messages "
        "WHERE chat_id = ? AND request_id = ? ORDER BY created_at, id",
        (db_uuid(chat_id), db_uuid(request_id)),
    )


QuotaKey = tuple[str, str, str]


def _quota_rows(server, user: str) -> dict[QuotaKey, dict[str, int]]:
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, "
        "calls, input_tokens, output_tokens FROM quota_usage WHERE tenant_id = ? AND user_id = ?",
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


_ZERO = {"spent": 0, "reserved": 0, "calls": 0, "input_tokens": 0, "output_tokens": 0}


def _total_keys(rows: dict[QuotaKey, dict[str, int]]) -> list[QuotaKey]:
    keys = [k for k in rows if k[2] == "total"]
    assert {k[0] for k in keys} >= {"daily", "monthly"}, f"missing total bucket periods: {sorted(rows)}"
    return keys


def _wait_user_idle(server, user: str) -> dict[QuotaKey, dict[str, int]]:
    """Wait until ``user`` has no running turn and no in-flight reserve; return the quota rows."""
    ident = server.identity(user)

    def probe() -> dict[QuotaKey, dict[str, int]] | None:
        running = server.query(
            "SELECT COUNT(*) AS n FROM chat_turns WHERE requester_user_id = ? AND state = 'running' "
            "AND deleted_at IS NULL",
            (db_uuid(ident.user_id),),
        )[0]["n"]
        rows = _quota_rows(server, user)
        if running == 0 and all(v["reserved"] == 0 for v in rows.values()):
            return rows or {("none", "none", "none"): dict(_ZERO)}
        return None

    rows = wait_until(probe, timeout=SILENT_DISCONNECT_TIMEOUT, description=f"{user} to be idle")
    return {k: v for k, v in rows.items() if k[0] != "none"}


def _assert_reserved_while_running(
    before: dict[QuotaKey, dict[str, int]], during: dict[QuotaKey, dict[str, int]], turn_reserve: int
) -> None:
    for key in _total_keys(during):
        expected = before.get(key, _ZERO)["reserved"] + turn_reserve
        assert during[key]["reserved"] == expected, (
            f"{key}: reserved_credits_micro {during[key]['reserved']} != {expected} while the turn runs"
        )
    for key, value in during.items():
        if key[2] == "tier:premium":
            assert value == before.get(key, _ZERO), f"standard-tier turn touched {key}"


def _assert_settled(
    before: dict[QuotaKey, dict[str, int]],
    after: dict[QuotaKey, dict[str, int]],
    charged: int,
    tokens: tuple[int, int] = (0, 0),
) -> None:
    for key in _total_keys(after):
        old, new = before.get(key, _ZERO), after[key]
        assert new["reserved"] == 0, f"{key}: reserve left behind after settlement: {new}"
        assert new["spent"] == old["spent"] + charged, (
            f"{key}: spent_credits_micro {new['spent']} != {old['spent']} + {charged}"
        )
        assert new["calls"] == old["calls"] + 1, f"{key}: calls {old['calls']} -> {new['calls']}"
        assert new["input_tokens"] == old["input_tokens"] + tokens[0], (key, old, new)
        assert new["output_tokens"] == old["output_tokens"] + tokens[1], (key, old, new)
    for key, value in after.items():
        if key[2] == "tier:premium":
            assert value == before.get(key, _ZERO), f"standard-tier turn touched {key}"


# --------------------------------------------------------------------------
# Usage outbox event
# --------------------------------------------------------------------------

_LOG_FIELD = re.compile(
    r"\b(billing_outcome|settlement_method|actual_credits_micro|effective_model|dedupe_key|request_id)"
    r"\s*[=:]\s*\"?([^\s\",}]+)"
)


def _usage_event(server, chat_id: str, turn_id: str, request_id: str) -> dict[str, Any]:
    """The turn's usage event: the outbox payload when still stored, else the
    ``usage event published`` record of the bundled static policy plugin.

    The shared outbox vacuums delivered bodies, so the payload may already be
    gone; the plugin record carries the billing fields but not ``usage``.
    """

    def probe() -> dict[str, Any] | None:
        for obj in outbox_payloads_for_chat(server, chat_id):
            if str(obj.get("turn_id", "")).lower() == turn_id.lower() and "billing_outcome" in obj:
                return {"source": "outbox", **obj}
        for line in server.log_text().splitlines():
            if "usage event published" in line and request_id in line:
                fields = dict(_LOG_FIELD.findall(line))
                if "billing_outcome" in fields:
                    if "actual_credits_micro" in fields:
                        fields["actual_credits_micro"] = int(fields["actual_credits_micro"])
                    return {"source": "log", **fields}
        return None

    return wait_until(probe, timeout=30.0, interval=0.05, description=f"usage event of turn {turn_id}")


def _assert_usage_event(
    server,
    chat_id: str,
    row: dict[str, Any],
    request_id: str,
    *,
    terminal_state: str,
    billing_outcome: str,
    settlement_method: str,
    credits: int,
    usage: dict[str, int] | None,
) -> None:
    turn_id = row["id"]
    event = _usage_event(server, chat_id, turn_id, request_id)
    assert event["billing_outcome"] == billing_outcome, event
    assert event["settlement_method"] == settlement_method, event
    assert int(event["actual_credits_micro"]) == credits, event
    tenant = server.query("SELECT tenant_id FROM chats WHERE id = ?", (db_uuid(chat_id),))[0]["tenant_id"]
    expected_key = f"{uuid.UUID(tenant).hex}/{uuid.UUID(turn_id).hex}/{uuid.UUID(request_id).hex}"
    assert event["dedupe_key"] == expected_key, event
    if event["source"] == "outbox":
        assert event["terminal_state"] == terminal_state, event
        assert event["effective_model"] == row["effective_model"], event
        assert event["request_id"] == request_id, event
        assert event["requester_type"] == "user", event
        assert int(event["policy_version_applied"]) == int(row["policy_version_applied"]), event
        if usage is None:
            assert event["usage"] is None, f"usage must be null: {event}"
        else:
            assert event["usage"] is not None, event
            for key, value in usage.items():
                assert event["usage"][key] == value, event


# --------------------------------------------------------------------------
# Client disconnect
# --------------------------------------------------------------------------


def test_disconnect_mid_stream_cancels_turn_persists_partial_and_settles_estimated(mc, mock):
    user = BILLING_USER
    chat_id = _new_chat(mc, user)
    before = _wait_user_idle(mc, user)
    chunks = [f"chunk-{i:02d} " for i in range(40)]
    full_text = "".join(chunks)
    mock.enqueue({"chunks": chunks, "delay_ms": 250})
    content = "Tell me a long story about the sea, please."
    request_id = new_uuid()

    with LiveStream(mc, chat_id, {"content": content, "request_id": request_id}, user) as live:
        started = _open_and_read_started(live, request_id)
        name, delta = live.next_event()
        assert name == "delta", live.events
        assert delta["type"] == "text", delta
        received = delta["content"]
        assert received, delta

        running = _turn_row(mc, chat_id, request_id)
        assert running["state"] == "running", running
        assert running["completed_at"] is None, running
        # §5.4.1 reserve of a fresh chat on a model without applicable tools.
        budgets = _catalog_entry(mc, MODEL)["estimation_budgets"]
        max_out = _max_output_tokens_applied(mc, MODEL)
        estimated_input = _estimated_text_tokens(content, budgets)
        in_mult, out_mult = _multipliers(mc, MODEL)
        assert running["effective_model"] == MODEL, running
        assert int(running["max_output_tokens_applied"]) == max_out, running
        assert int(running["reserve_tokens"]) == estimated_input + max_out, running
        assert int(running["reserved_credits_micro"]) == credits_micro(
            estimated_input, max_out, in_mult, out_mult
        ), running
        assert int(running["minimal_generation_floor_applied"]) == min(
            _minimal_generation_floor(mc), max_out
        ), running
        assert running["policy_version_applied"] is not None, running
        _assert_reserved_while_running(
            before, _quota_rows(mc, user), int(running["reserved_credits_micro"])
        )
    # Leaving the block disconnected the client after one delta.

    mock.wait_for_disconnect(1, timeout=10.0)
    assert not _stream_requests(mock)[-1]["finished"], "provider stream ran to completion"
    status = _wait_terminal_status(mc, chat_id, request_id, user, timeout=15.0)

    assert status["state"] == "cancelled", status
    assert "error_code" not in status, status
    assert status["assistant_message_id"] == started["message_id"], status

    row = _turn_row(mc, chat_id, request_id)
    assert row["state"] == "cancelled", row
    assert row["error_code"] is None, row
    assert row["completed_at"] is not None, row
    assert row["assistant_message_id"] == started["message_id"], row

    messages = _messages_of_turn(mc, chat_id, request_id)
    assert [m["role"] for m in messages] == ["user", "assistant"], messages
    partial = messages[1]
    assert partial["id"] == started["message_id"], partial
    assert partial["deleted_at"] is None, partial
    assert partial["content"].startswith(received), (received, partial["content"])
    assert full_text.startswith(partial["content"]), "partial content is not a prefix of the provider text"
    assert len(partial["content"]) < len(full_text), "the persisted message is the full answer"

    charged = _estimated_charge(mc, row)
    _assert_settled(before, _quota_rows(mc, user), charged)
    _assert_usage_event(
        mc,
        chat_id,
        row,
        request_id,
        terminal_state="cancelled",
        billing_outcome="aborted",
        settlement_method="estimated",
        credits=charged,
        usage=None,
    )


def test_disconnect_before_any_delta_cancels_turn_without_assistant_message(mc, mock):
    user = BILLING_USER
    chat_id = _new_chat(mc, user)
    before = _wait_user_idle(mc, user)
    mock.enqueue(HANG)
    request_id = new_uuid()

    with LiveStream(mc, chat_id, {"content": "think silently", "request_id": request_id}, user) as live:
        _open_and_read_started(live, request_id)
        _wait_stream_requests(mock, 1)

    status = _wait_terminal_status(mc, chat_id, request_id, user, timeout=SILENT_DISCONNECT_TIMEOUT)
    assert status["state"] == "cancelled", status
    assert "assistant_message_id" not in status, status
    assert "error_code" not in status, status
    mock.wait_for_disconnect(1, timeout=10.0)

    row = _turn_row(mc, chat_id, request_id)
    assert row["state"] == "cancelled", row
    assert row["assistant_message_id"] is None, row
    assert row["error_code"] is None, row
    assert [m["role"] for m in _messages_of_turn(mc, chat_id, request_id)] == ["user"]

    charged = _estimated_charge(mc, row)
    _assert_settled(before, _quota_rows(mc, user), charged)
    _assert_usage_event(
        mc,
        chat_id,
        row,
        request_id,
        terminal_state="cancelled",
        billing_outcome="aborted",
        settlement_method="estimated",
        credits=charged,
        usage=None,
    )


def test_chat_accepts_a_new_turn_after_a_cancelled_turn(mc, mock):
    chat_id = _new_chat(mc, "A1")
    cancelled_request = new_uuid()
    mock.enqueue(HANG)

    with LiveStream(mc, chat_id, {"content": "first", "request_id": cancelled_request}, "A1") as live:
        _open_and_read_started(live, cancelled_request)
        _wait_stream_requests(mock, 1)
    status = _wait_terminal_status(mc, chat_id, cancelled_request, "A1", timeout=SILENT_DISCONNECT_TIMEOUT)
    assert status["state"] == "cancelled", status

    mock.enqueue({"text": "fresh answer"})
    next_request = new_uuid()
    result = _stream(mc, chat_id, "A1", "second", next_request)

    assert result.status_code == 200 and result.is_sse, (result.status_code, result.text[:500])
    assert result.terminal is not None and result.terminal[0] == "done", result.events
    assert result.first("stream_started")["is_new_turn"] is True
    assert result.text_content() == "fresh answer"
    assert _turn_status(mc, chat_id, next_request, "A1")["state"] == "done"
    assert _turn_status(mc, chat_id, cancelled_request, "A1")["state"] == "cancelled"


# --------------------------------------------------------------------------
# Turn states end to end
# --------------------------------------------------------------------------


def test_turn_states_end_to_end_via_turn_status_api(mc, mock):
    user = "A1"
    chat_id = _new_chat(mc, user)

    done_request = new_uuid()
    mock.enqueue({"text": "All good."})
    done = _stream(mc, chat_id, user, "complete me", done_request)
    assert done.terminal is not None and done.terminal[0] == "done", done.events
    done_message_id = done.first("stream_started")["message_id"]

    error_request = new_uuid()
    mock.enqueue({"kind": "failed"})
    failed = _stream(mc, chat_id, user, "fail me", error_request)
    assert failed.terminal is not None and failed.terminal[0] == "error", failed.events
    assert failed.terminal[1]["code"] == "provider_error", failed.terminal

    cancel_request = new_uuid()
    mock.enqueue(HANG)
    with LiveStream(mc, chat_id, {"content": "cancel me", "request_id": cancel_request}, user) as live:
        _open_and_read_started(live, cancel_request)
        _wait_stream_requests(mock, 3)
    _wait_terminal_status(mc, chat_id, cancel_request, user, timeout=SILENT_DISCONNECT_TIMEOUT)

    done_status = _turn_status(mc, chat_id, done_request, user)
    assert done_status["state"] == "done", done_status
    assert done_status["assistant_message_id"] == done_message_id, done_status
    assert "error_code" not in done_status, done_status

    error_status = _turn_status(mc, chat_id, error_request, user)
    assert error_status["state"] == "error", error_status
    assert error_status["error_code"] == "provider_error", error_status
    assert "assistant_message_id" not in error_status, error_status

    cancel_status = _turn_status(mc, chat_id, cancel_request, user)
    assert cancel_status["state"] == "cancelled", cancel_status
    assert "error_code" not in cancel_status, cancel_status
    assert "assistant_message_id" not in cancel_status, cancel_status

    internal = {
        done_request: "completed",
        error_request: "failed",
        cancel_request: "cancelled",
    }
    for request_id, state in internal.items():
        assert _turn_row(mc, chat_id, request_id)["state"] == state, request_id
    updated = [
        parse_ts_ns(s["updated_at"]) for s in (done_status, error_status, cancel_status)
    ]
    assert updated == sorted(updated), f"updated_at must follow the turn order: {updated}"


# --------------------------------------------------------------------------
# Running turn liveness
# --------------------------------------------------------------------------


def test_running_turn_has_last_progress_at(mc, mock):
    user = "A2"
    chat_id = _new_chat(mc, user)
    mock.enqueue(HANG)
    request_id = new_uuid()

    with LiveStream(mc, chat_id, {"content": "stay running", "request_id": request_id}, user) as live:
        _open_and_read_started(live, request_id)
        _wait_stream_requests(mock, 1)

        row = _turn_row(mc, chat_id, request_id)
        assert row["state"] == "running", row
        assert row["completed_at"] is None, row
        assert row["last_progress_at"] is not None, "a running turn must have last_progress_at"
        assert is_rfc3339(row["last_progress_at"]) and is_rfc3339(row["started_at"]), row
        assert parse_ts_ns(row["last_progress_at"]) >= parse_ts_ns(row["started_at"]), row
        assert row["requester_type"] == "user", row
        assert row["requester_user_id"] == mc.identity(user).user_id, row

        status = _turn_status(mc, chat_id, request_id, user)
        assert status["state"] == "running", status
        assert "error_code" not in status and "assistant_message_id" not in status, status

    final = _wait_terminal_status(mc, chat_id, request_id, user, timeout=SILENT_DISCONNECT_TIMEOUT)
    assert final["state"] == "cancelled", final


# --------------------------------------------------------------------------
# Failed turn with provider-reported usage
# --------------------------------------------------------------------------


def test_failed_turn_with_provider_usage_settles_actual(mc, mock):
    user = BILLING_USER
    chat_id = _new_chat(mc, user)
    before = _wait_user_idle(mc, user)
    usage = {"input_tokens": 200, "output_tokens": 40}
    mock.enqueue({"kind": "failed", "usage": usage})
    request_id = new_uuid()

    result = _stream(mc, chat_id, user, "fail after some work", request_id)

    assert result.status_code == 200 and result.is_sse, (result.status_code, result.text[:500])
    assert result.terminal is not None and result.terminal[0] == "error", result.events
    assert result.terminal[1]["code"] == "provider_error", result.terminal
    row = _turn_row(mc, chat_id, request_id)
    assert row["state"] == "failed" and row["error_code"] == "provider_error", row
    assert row["assistant_message_id"] is None, row

    in_mult, out_mult = _multipliers(mc, row["effective_model"])
    charged = credits_micro(usage["input_tokens"], usage["output_tokens"], in_mult, out_mult)
    # The terminal event is sent after the finalization commit (§5.7 emission guard).
    _assert_settled(before, _quota_rows(mc, user), charged, (usage["input_tokens"], usage["output_tokens"]))
    _assert_usage_event(
        mc,
        chat_id,
        row,
        request_id,
        terminal_state="failed",
        billing_outcome="failed",
        settlement_method="actual",
        credits=charged,
        usage=usage,
    )
