"""S3 web search: tool inclusion, tool events, citations, call limits and quota.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* "Web Search Configuration" (``cpt-cf-mini-chat-design-web-search``): the
  ``web_search`` tool is sent only when ``web_search.enabled = true`` and the
  effective model has ``tool_support.web_search``, with the catalog entry's
  ``web_search_context_size``; the guard text (``context.web_search_guard``)
  is appended to the system prompt only when the tool is sent; the requested
  flag is stored on ``chat_turns.web_search_enabled`` either way;
* "Web Search Quota Enforcement (P1 Determinism Rules)": kill switch
  ``disable_web_search`` (400 ``failed_precondition``, ``web_search`` /
  ``FEATURE_DISABLED``), daily quota ``quota.web_search_daily_quota`` (429
  ``resource_exhausted``, quota scope ``web_search``, checked only for
  requests that enable web search on a supporting model), per-message limit
  ``quota.web_search_max_calls_per_message`` counted on tool ``start`` events
  (SSE ``error`` ``web_search_calls_exceeded``, turn ``failed``, estimated
  settlement, outbox ``failed`` / ``estimated``);
* §3.3 ``event: tool`` (``ToolData``) and ``event: citations``
  (``CitationsData``, ``Citation``, ``TextSpan``), "SSE Event Ordering"
  (``stream_started ping* (delta | tool)* citations? (done | error)``) and
  "Provider Event Translation" (``response.web_search_call.searching`` /
  ``.completed`` -> ``tool`` start / done with ``details: {}``; url
  annotations -> ``source: web`` citations, snippet = answer text in the
  annotation range, ``span`` from ``start_index`` / ``end_index``);
* "Provider Request Metadata" (``metadata.feature``);
* §5.4.1 / §5.5.6 reserve surcharge ``web_search_surcharge_tokens`` (only for
  a model whose ``tool_support.web_search`` is set);
* §3.7 ``quota_usage.web_search_calls`` (bucket ``total`` only), §5.6 usage
  outbox ``web_search_calls``, audit ``tool_calls.web_search_calls``.
"""

from __future__ import annotations

import json
import re
import threading
import time
from datetime import datetime, timezone
from typing import Any

import pytest

from mc_helpers import assert_problem, db_uuid, new_uuid, wait_until
from mc_seed import outbox_payloads_for_chat

CHATS = "/mini-chat/v1/chats"
PREMIUM_MODEL = "gpt-4.1"
STANDARD_MODEL = "gpt-4.1-mini"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
#: ``streaming.max_output_tokens`` default (B.4).
STREAMING_MAX_OUTPUT_TOKENS = 32_768
#: Gear ``estimation_budgets.minimal_generation_floor`` default.
MINIMAL_GENERATION_FLOOR = 50
#: Built-in ``context.web_search_guard``.
WEB_SEARCH_GUARD = (
    "Use web_search only if the answer cannot be obtained from the provided context or your "
    "training data. Never use it for general knowledge questions. At most one web_search call "
    "per request."
)
#: ``quota.web_search_max_calls_per_message`` default.
WEB_SEARCH_MAX_CALLS_PER_MESSAGE = 2
CITATION_FIELDS = {"source", "title", "url", "attachment_id", "span", "snippet", "score"}
CITATIONS_ORDER = re.compile(r"^stream_started( ping)*( (delta|tool))* citations done$")


# --------------------------------------------------------------------------
# Credit arithmetic and reserve formula (DESIGN §5.3, §5.4.1, §5.5)
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def model_credits(entry: dict[str, Any], input_tokens: int, output_tokens: int) -> int:
    return _ceil_div(input_tokens * entry["input_tokens_credit_multiplier_micro"], 1_000_000) + _ceil_div(
        output_tokens * entry["output_tokens_credit_multiplier_micro"], 1_000_000
    )


def expected_reserve(entry: dict[str, Any], content: str, *, web_search: bool) -> dict[str, int]:
    """Preflight reserve of a first, text-only turn (no prior context, no attachments)."""
    budgets = entry["estimation_budgets"]
    base = _ceil_div(len(content.encode("utf-8")), budgets["bytes_per_token_conservative"])
    est_input = _ceil_div((base + budgets["fixed_overhead_tokens"]) * (100 + budgets["safety_margin_pct"]), 100)
    if web_search and entry["general_config"]["tool_support"]["web_search"]:
        est_input += budgets["web_search_surcharge_tokens"]
    max_out = min(entry["max_output_tokens"], STREAMING_MAX_OUTPUT_TOKENS)
    return {
        "reserve_tokens": est_input + max_out,
        "max_output_tokens_applied": max_out,
        "reserved_credits_micro": model_credits(entry, est_input, max_out),
    }


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _policy(**config: Any) -> dict[str, Any]:
    return {"gears": {POLICY_PLUGIN: {"config": config}}}


def _entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the server catalog")


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _new_chat(server, user: str = "A1", model: str = PREMIUM_MODEL) -> str:
    resp = server.post(CHATS, user, json={"title": "S3 web search", "model": model})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _body(content: str, request_id: str, web_search: bool | None) -> dict[str, Any]:
    body: dict[str, Any] = {"content": content, "request_id": request_id}
    if web_search is not None:
        body["web_search"] = {"enabled": web_search}
    return body


def _send(server, chat_id: str, content: str, *, web_search: bool | None = True, user: str = "A1"):
    request_id = new_uuid()
    result = server.stream("POST", _stream_path(chat_id), user, json=_body(content, request_id, web_search))
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert result.terminal is not None, names
    return result, request_id


def _send_done(server, chat_id: str, content: str, *, web_search: bool | None = True, user: str = "A1"):
    result, request_id = _send(server, chat_id, content, web_search=web_search, user=user)
    assert result.terminal[0] == "done", (result.names(), result.events[-1:])
    return result.terminal[1], result, request_id


def _turn(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def _turn_count(server, chat_id: str) -> int:
    return server.query("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?", (db_uuid(chat_id),))[0]["n"]


def _assert_reserve(turn: dict[str, Any], expected: dict[str, int]) -> None:
    for key, value in expected.items():
        assert turn[key] == value, (key, turn, expected)


def _stream_requests(mock) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("responses") if r["stream"]]


def _item_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(p.get("text", "")) for p in content if isinstance(p, dict))
    return ""


def _system_instructions(body: dict[str, Any]) -> str:
    """Top-level ``instructions`` plus the text of every ``system`` / ``developer`` input item."""
    parts = [str(body.get("instructions") or "")]
    for item in body.get("input") or []:
        if isinstance(item, dict) and item.get("role") in ("system", "developer"):
            parts.append(_item_text(item.get("content")))
    return "\n".join(parts)


def _web_search_tools(body: dict[str, Any]) -> list[dict[str, Any]]:
    return [t for t in body.get("tools") or [] if isinstance(t, dict) and t.get("type") == "web_search"]


def _date_text(value: Any) -> str:
    if isinstance(value, (bytes, bytearray, memoryview)):
        value = bytes(value).decode("ascii")
    return str(value)[:10]


def _quota_rows(server, user: str) -> dict[tuple[str, str, str], dict[str, Any]]:
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, web_search_calls FROM quota_usage "
        "WHERE tenant_id = ? AND user_id = ?",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    return {(r["period_type"], _date_text(r["period_start"]), r["bucket"]): r for r in rows}


def _total_web_search_calls(server, user: str) -> dict[str, int]:
    today = datetime.now(timezone.utc).date()
    rows = _quota_rows(server, user)
    result = {}
    for period, start in (("daily", today.isoformat()), ("monthly", today.replace(day=1).isoformat())):
        row = rows.get((period, start, "total"))
        result[period] = 0 if row is None else row["web_search_calls"]
    return result


class OutboxRecorder:
    """Polls the outbox bodies of one chat in a background thread (bodies are vacuumed after ack)."""

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

    def _matching(self, request_id: str, predicate) -> list[dict[str, Any]]:
        self.poll()
        with self._lock:
            return [
                p
                for p in self._seen.values()
                if str(p.get("request_id", "")).lower() == request_id.lower() and predicate(p)
            ]

    def wait_usage_event(self, request_id: str) -> dict[str, Any]:
        events = wait_until(
            lambda: self._matching(request_id, lambda p: "dedupe_key" in p),
            timeout=15,
            interval=0.05,
            description=f"usage outbox payload of request {request_id}",
        )
        assert len(events) == 1, events
        return events[0]

    def wait_audit_event(self, request_id: str, event_type: str) -> dict[str, Any]:
        events = wait_until(
            lambda: self._matching(request_id, lambda p: p.get("event_type") == event_type),
            timeout=15,
            interval=0.05,
            description=f"{event_type} audit outbox payload of request {request_id}",
        )
        assert len(events) == 1, events
        return events[0]


# --------------------------------------------------------------------------
# Tool inclusion, guard, metadata and reserve surcharge
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_web_search_enabled_sends_tool_guard_and_reserves_surcharge(start_server, mock, model_catalog):
    for entry in model_catalog:
        if entry["id"] == PREMIUM_MODEL:
            entry["web_search_context_size"] = "medium"
            entry["estimation_budgets"]["web_search_surcharge_tokens"] = 777
    server = start_server(_policy(model_catalog=model_catalog), name="web-search-tool")
    entry = _entry(server, PREMIUM_MODEL)
    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    content = "What happened in the news today?"

    done, _, request_id = _send_done(server, chat_id, content, web_search=True)

    assert done["effective_model"] == PREMIUM_MODEL, done
    body = _stream_requests(mock)[-1]["json"]
    tools = body.get("tools") or []
    assert {t.get("type") for t in tools} == {"web_search"}, tools
    web_search = _web_search_tools(body)
    assert len(web_search) == 1, tools
    assert web_search[0].get("search_context_size") == "medium", web_search
    instructions = _system_instructions(body)
    assert entry["system_prompt"] in instructions, instructions
    assert WEB_SEARCH_GUARD in instructions, instructions
    assert body["metadata"]["feature"] == "web_search", body["metadata"]

    turn = _turn(server, chat_id, request_id)
    assert turn["web_search_enabled"] == 1, turn
    _assert_reserve(turn, expected_reserve(entry, content, web_search=True))


def test_web_search_on_model_without_support_sends_no_tool(mc, mock):
    entry = _entry(mc, STANDARD_MODEL)
    assert entry["general_config"]["tool_support"]["web_search"] is False
    chat_id = _new_chat(mc, "A1", STANDARD_MODEL)
    content = "Search the web if you can"

    done, _, request_id = _send_done(mc, chat_id, content, web_search=True)

    assert done["effective_model"] == STANDARD_MODEL, done
    body = _stream_requests(mock)[-1]["json"]
    assert _web_search_tools(body) == [], body.get("tools")
    assert WEB_SEARCH_GUARD not in _system_instructions(body)
    assert body["metadata"]["feature"] == "none", body["metadata"]
    turn = _turn(mc, chat_id, request_id)
    assert turn["web_search_enabled"] == 1, turn
    _assert_reserve(turn, expected_reserve(entry, content, web_search=False))


@pytest.mark.parametrize("web_search", [False, None], ids=["disabled", "omitted"])
def test_web_search_not_enabled_sends_no_tool_and_no_surcharge(mc, mock, web_search):
    entry = _entry(mc, PREMIUM_MODEL)
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)
    content = "No web search for this one"

    _, _, request_id = _send_done(mc, chat_id, content, web_search=web_search)

    body = _stream_requests(mock)[-1]["json"]
    assert _web_search_tools(body) == [], body.get("tools")
    assert WEB_SEARCH_GUARD not in _system_instructions(body)
    assert body["metadata"]["feature"] == "none", body["metadata"]
    turn = _turn(mc, chat_id, request_id)
    assert turn["web_search_enabled"] == 0, turn
    _assert_reserve(turn, expected_reserve(entry, content, web_search=False))


# --------------------------------------------------------------------------
# Tool events, counters and citations
# --------------------------------------------------------------------------


def test_web_search_tool_events_are_streamed_and_counted(mc, mock):
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)
    before = _total_web_search_calls(mc, "A1")
    mock.enqueue(
        {
            "text": "Two searches later, here is the answer.",
            "tool_events": [{"type": "web_search", "count": WEB_SEARCH_MAX_CALLS_PER_MESSAGE}],
        }
    )

    with OutboxRecorder(mc, chat_id) as recorder:
        done, result, request_id = _send_done(mc, chat_id, "Search twice please", web_search=True)
        event = recorder.wait_usage_event(request_id)
        audit = recorder.wait_audit_event(request_id, "turn_completed")

    tools = result.of("tool")
    assert tools == [
        {"phase": "start", "name": "web_search", "details": {}},
        {"phase": "done", "name": "web_search", "details": {}},
    ] * WEB_SEARCH_MAX_CALLS_PER_MESSAGE, tools
    names = result.names()
    assert names.index("tool") < names.index("delta"), names
    assert set(names) <= {"stream_started", "ping", "tool", "delta", "done"}, names
    assert names[-1] == "done"
    assert result.text_content() == "Two searches later, here is the answer."
    assert done["effective_model"] == PREMIUM_MODEL

    turn = _turn(mc, chat_id, request_id)
    assert turn["state"] == "completed" and turn["web_search_completed_count"] == 2, turn
    after = _total_web_search_calls(mc, "A1")
    assert after == {period: before[period] + 2 for period in before}, (before, after)
    for key, row in _quota_rows(mc, "A1").items():
        if key[2] == "tier:premium":
            assert row["web_search_calls"] == 0, (key, row)
    assert event["web_search_calls"] == 2, event
    assert event["settlement_method"] == "actual" and event["billing_outcome"] == "completed", event
    assert audit["tool_calls"]["web_search_calls"] == 2, audit


def test_url_citations_are_sent_once_before_done(mc, mock):
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)
    text = "Paris is the capital of France."
    annotations = [
        {
            "type": "url_citation",
            "url": "https://example.com/paris",
            "title": "Paris - Example",
            "start_index": 0,
            "end_index": 5,
        },
        {
            "type": "url_citation",
            "url": "https://example.org/france",
            "title": "France facts",
            "start_index": 24,
            "end_index": 30,
        },
    ]
    mock.enqueue({"text": text, "annotations": annotations, "tool_events": [{"type": "web_search", "count": 1}]})

    _, result, _ = _send_done(mc, chat_id, "What is the capital of France?", web_search=True)

    names = result.names()
    assert CITATIONS_ORDER.match(" ".join(names)), names
    assert names.count("citations") == 1, names
    citations = result.first("citations")
    assert set(citations) == {"items"}, citations
    items = citations["items"]
    assert len(items) == len(annotations), items
    for item, annotation in zip(items, annotations):
        assert set(item) <= CITATION_FIELDS, item
        assert "score" not in item, item
        assert item.get("attachment_id") is None, item
        start, end = annotation["start_index"], annotation["end_index"]
        assert {k: item.get(k) for k in ("source", "url", "title", "snippet", "span")} == {
            "source": "web",
            "url": annotation["url"],
            "title": annotation["title"],
            "snippet": text[start:end],
            "span": {"start": start, "end": end},
        }, item
    assert result.text_content() == text


def test_no_citations_event_without_annotations(mc, mock):
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)
    mock.enqueue({"text": "No sources needed.", "tool_events": [{"type": "web_search", "count": 1}]})

    _, result, _ = _send_done(mc, chat_id, "Answer without sources", web_search=True)

    assert "citations" not in result.names(), result.names()


# --------------------------------------------------------------------------
# Limits: per message, daily quota, kill switch
# --------------------------------------------------------------------------


def test_web_search_calls_over_per_message_limit_fail_the_turn(mc, mock):
    entry = _entry(mc, PREMIUM_MODEL)
    chat_id = _new_chat(mc, "A1", PREMIUM_MODEL)
    mock.enqueue(
        {
            "text": "Too many searches.",
            "tool_events": [{"type": "web_search", "count": WEB_SEARCH_MAX_CALLS_PER_MESSAGE + 1}],
        }
    )

    with OutboxRecorder(mc, chat_id) as recorder:
        result, request_id = _send(mc, chat_id, "Search three times", web_search=True)
        event = recorder.wait_usage_event(request_id)
        audit = recorder.wait_audit_event(request_id, "turn_failed")

    terminal, error = result.terminal
    assert terminal == "error", result.names()
    assert error["code"] == "web_search_calls_exceeded", error
    assert isinstance(error.get("message"), str), error
    assert "done" not in result.names(), result.names()
    tool_names = {t["name"] for t in result.of("tool")}
    assert tool_names == {"web_search"}, result.of("tool")

    turn = _turn(mc, chat_id, request_id)
    assert turn["state"] == "failed", turn
    assert turn["error_code"] == "web_search_calls_exceeded", turn
    assert turn["web_search_enabled"] == 1, turn

    estimated_input = turn["reserve_tokens"] - turn["max_output_tokens_applied"]
    floor = turn["minimal_generation_floor_applied"]
    assert floor == min(MINIMAL_GENERATION_FLOOR, turn["max_output_tokens_applied"]), turn
    assert event["terminal_state"] == "failed", event
    assert event["billing_outcome"] == "failed", event
    assert event["settlement_method"] == "estimated", event
    assert event["usage"] is None, event
    assert event["actual_credits_micro"] == model_credits(entry, estimated_input, floor), event
    assert audit["error_code"] == "web_search_calls_exceeded", audit


@pytest.mark.slow
def test_web_search_daily_quota_rejects_only_web_search_requests(start_server, mock):
    server = start_server(
        {"gears": {"mini-chat": {"config": {"quota": {"web_search_daily_quota": 1}}}}},
        name="web-search-daily",
    )
    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    mock.enqueue({"text": "Searched once.", "tool_events": [{"type": "web_search", "count": 1}]})
    _send_done(server, chat_id, "Use the one search I have", web_search=True)
    assert _total_web_search_calls(server, "A1")["daily"] == 1
    requests_before = len(_stream_requests(mock))

    resp = server.post(_stream_path(chat_id), "A1", json=_body("Search again", new_uuid(), True))

    problem = assert_problem(resp, 429, category="resource_exhausted", violation_subject="web_search")
    violation = problem["context"]["violations"][0]
    assert violation["subject"] == "web_search", problem
    assert violation["description"] == "quota_exceeded", problem
    assert len(_stream_requests(mock)) == requests_before
    assert _turn_count(server, chat_id) == 1

    # The same request without web search is allowed and sends no web_search tool.
    _send_done(server, chat_id, "Answer without searching", web_search=False)
    assert _web_search_tools(_stream_requests(mock)[-1]["json"]) == []

    # A model without web search support is never checked against the daily quota.
    mini_chat = _new_chat(server, "A1", STANDARD_MODEL)
    _send_done(server, mini_chat, "Search if you can", web_search=True)

    # The quota is per user: A2 still has its daily search.
    other_chat = _new_chat(server, "A2", PREMIUM_MODEL)
    _send_done(server, other_chat, "My own search", web_search=True, user="A2")
    assert _web_search_tools(_stream_requests(mock)[-1]["json"]) != []


@pytest.mark.slow
def test_disable_web_search_kill_switch_rejects_web_search_requests(start_server, mock):
    server = start_server(_policy(kill_switches={"disable_web_search": True}), name="web-search-kill-switch")

    for model in (PREMIUM_MODEL, STANDARD_MODEL):
        chat_id = _new_chat(server, "A1", model)
        resp = server.post(_stream_path(chat_id), "A1", json=_body("Search please", new_uuid(), True))

        assert not resp.headers.get("content-type", "").startswith("text/event-stream"), resp.headers
        assert_problem(
            resp,
            400,
            category="failed_precondition",
            violation_subject="web_search",
            violation_type="FEATURE_DISABLED",
        )
        assert _turn_count(server, chat_id) == 0
    assert _stream_requests(mock) == []

    chat_id = _new_chat(server, "A1", PREMIUM_MODEL)
    for web_search in (False, None):
        _send_done(server, chat_id, "No search needed", web_search=web_search)
        assert _web_search_tools(_stream_requests(mock)[-1]["json"]) == []
