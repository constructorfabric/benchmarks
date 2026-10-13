"""S6 background: thread summary scheduling, execution, commit and use.

Contract sources (gears/mini-chat/docs/DESIGN.md):

* §3.6 "Thread Summary Update": trigger on the assembled context estimate
  (proactive: ``assembled >= compression_threshold_pct% of effective_budget``
  with no summary; urgent: context assembly truncated recent messages, also
  with a summary), ``effective_budget = min(max_input_tokens, context_window -
  max_output_tokens_applied)``; durable scheduling on the
  ``mini-chat.thread_summary`` queue in the causing turn's finalization
  transaction with ``tenant_id``, ``chat_id``, ``system_request_id``,
  ``base_frontier_*``, ``frozen_target_*`` and ``system_task_type =
  thread_summary_update``; the frozen target is the latest non-deleted message
  that does not belong to the causing turn;
* handler: summary model ``thread_summary_worker.summary_model_id`` resolved
  with the enabled filter (missing / disabled -> ``Reject``, dead letter, no
  provider call); system prompt = summary model catalog
  ``thread_summary_prompt``, else ``thread_summary_worker.summary_system_prompt``;
  user prompt (B.5.5): ``Summarize the following conversation:`` or the merge
  opening + ``<existing_summary>`` block + ``New messages to incorporate:``,
  then ``User: …`` / ``Assistant: …`` entries separated by a blank line
  (content cut to ``message_content_limit`` characters and ending with
  ``...``), then the analysis instruction; ``max_output_tokens`` = the summary
  model's catalog ``max_output_tokens`` (not capped by
  ``streaming.max_output_tokens``); non-streaming call;
* "Provider Request Metadata": the summary request uses the system identity
  (``user`` = tenant hex + ``11111111-6a88-4768-9dfc-6bcd5187d9ed`` hex,
  ``metadata.request_type = summary``, ``metadata.user_id`` = the system
  subject, ``feature = none``);
* response parsing (``<analysis>`` removed, ``<summary>`` content stored, runs
  of blank lines collapsed; empty -> ``Retry``), ``token_estimate`` =
  ``output_tokens - reasoning_tokens`` or ``ceil(summary bytes / 4)`` when not
  positive; provider failure -> ``Retry``; ``thread_summary_worker.max_attempts``
  deliveries -> ``Reject`` (dead letter);
* CAS commit: ``thread_summaries`` upsert with the frozen frontier,
  ``is_compressed = true`` on exactly ``(base_frontier, frozen_target]``,
  system usage event (§3.2 "System Task Attribution Rules": ``billing_outcome
  = system_task``, ``settlement_method = none``, ``actual_credits_micro = 0``,
  ``requester_type = system``, no ``user_id`` / ``turn_id``, ``dedupe_key =
  tenant.hex/thread_summary_update/system_request.hex``); no ``chat_turns``
  row and no ``quota_usage`` debit;
* context assembly afterwards: the summary is one ``user``-role message with
  the B.5.5 preamble, ``is_compressed`` messages are not sent,
  ``stream_started.thread_summary_applied = {"token_estimate": n}``.

Summary invalidation on turn mutation is covered by ``test_s4_turn_mutations.py``.

Every test runs on extra servers (a small-context chat model), hence ``slow``.
"""

from __future__ import annotations

import copy
import json
import re
import threading
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Iterator

import pytest

from conftest import create_server, default_model_catalog
from mc_helpers import db_uuid, is_uuid, new_uuid, wait_until
from mc_seed import is_rfc3339, outbox_payloads_for_chat, outbox_snapshot, outbox_tables, parse_ts_ns

pytestmark = pytest.mark.slow

CHATS = "/mini-chat/v1/chats"
POLICY_PLUGIN = "static-mini-chat-model-policy-plugin"
THREAD_SUMMARY_QUEUE = "mini-chat.thread_summary"
SYSTEM_TASK_TYPE = "thread_summary_update"
#: Platform default subject id used as the summary request's user (DESIGN §3.3).
SYSTEM_SUBJECT_ID = "11111111-6a88-4768-9dfc-6bcd5187d9ed"

#: Chat model with a small context window, so a few turns reach the threshold.
CHAT_MODEL = "gpt-small-ctx"
CHAT_CONTEXT_WINDOW = 4000
CHAT_MAX_OUTPUT_TOKENS = 1000
#: Summary model (``thread_summary_worker.summary_model_id``).
SUMMARY_MODEL = "gpt-4.1-mini"
CATALOG_SUMMARY_PROMPT = "E2E catalog summary prompt: summarize the conversation faithfully."
CONFIG_SUMMARY_PROMPT = "E2E configured summary prompt: produce a structured summary."
MESSAGE_CONTENT_LIMIT = 1500
SUMMARY_MAX_ATTEMPTS = 3
COMPRESSION_THRESHOLD_PCT = 80
#: Below the summary model's catalog ``max_output_tokens``: the summary call
#: must not be capped by it.
STREAMING_MAX_OUTPUT_TOKENS = 4096
DEFAULT_FIXED_OVERHEAD = 100

#: B.5.5 thread summary preamble.
PREAMBLE = (
    "This conversation has earlier messages that have been summarized. The summary below covers "
    "the earlier portion of the conversation. Recent messages follow after."
)
#: B.5.5 opening of a first summary.
OPENING_FIRST = "Summarize the following conversation:"
#: B.5.5 opening of a re-summarize (followed by the ``<existing_summary>`` block).
MERGE_OPENING = (
    "The existing summary below covers the earlier conversation. Incorporate it with the new "
    "messages into a single updated summary.",
    "IMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: "
    "current topic and recent decisions > user preferences and corrections > older facts. Compress "
    "or drop the least relevant older details rather than letting the summary grow unboundedly.",
)
NEW_MESSAGES_HEADER = "New messages to incorporate:"
#: B.5.5 analysis instruction, line by line (the prompt ends with the last one).
ANALYSIS_LINES = (
    "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:",
    "1. Chronologically review each exchange, identifying:",
    "- The user's requests and questions",
    "- Key decisions, answers, and information shared",
    "- Any follow-up actions or commitments",
    "- Specific names, dates, numbers, URLs, or references mentioned",
    "2. Verify accuracy and completeness.",
    "Your summary MUST include these sections:",
    "1. Conversation Purpose: The user's primary goals and recurring themes",
    "2. Key Information Exchanged: Important facts, decisions, recommendations, and answers",
    "3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections",
    "4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit",
    "5. Current Topic: What was being discussed most recently, with enough detail to continue naturally",
    "Respond with an <analysis> block followed by a <summary> block.",
)

#: Provider output of a summary call and the text that must be stored for it.
RAW_SUMMARY = (
    "<analysis>The user shared several long texts.\nThe assistant answered each of them.</analysis>\n"
    "<summary>1. Conversation Purpose: a long text exchange.\n\n\n\n"
    "2. Key Information Exchanged: the assistant answered every text.</summary>"
)
STORED_SUMMARY = (
    "1. Conversation Purpose: a long text exchange.\n\n"
    "2. Key Information Exchanged: the assistant answered every text."
)
SUMMARY_USAGE = {"input_tokens": 900, "output_tokens": 85, "reasoning_tokens": 5, "cached_tokens": 300}
#: ``output_tokens - reasoning_tokens`` of :data:`SUMMARY_USAGE`.
SUMMARY_TOKEN_ESTIMATE = 80

RAW_SUMMARY_2 = (
    "<analysis>The new messages continue the long text exchange.</analysis>"
    "<summary>Merged summary: the earlier long texts and the third long question.</summary>"
)
STORED_SUMMARY_2 = "Merged summary: the earlier long texts and the third long question."
#: Only reasoning tokens: ``output - reasoning = 0`` -> ``ceil(bytes / 4)``.
SUMMARY_USAGE_2 = {"input_tokens": 400, "output_tokens": 40, "reasoning_tokens": 40}

#: Message sizes (characters, ASCII) of the three-turn conversation. With the
#: small chat model, turns 1 and 2 stay below the 80% threshold and turn 3
#: reaches it without truncation (checked by :func:`plan_context`).
SIZES = {"U1": 2000, "A1": 2400, "U2": 300, "A2": 2800, "U3": 2000, "A3": 60}
SHORT_U4 = 120
#: A fourth message large enough to force truncation next to the summary.
HUGE_U4 = 9000

_FILLER = "The quick brown fox jumps over the lazy dog while the summary worker waits. "


# --------------------------------------------------------------------------
# Config
# --------------------------------------------------------------------------


def summary_catalog(catalog_prompt: str = CATALOG_SUMMARY_PROMPT) -> list[dict[str, Any]]:
    """Default catalog + the small-context chat model; the summary model's
    ``thread_summary_prompt`` is ``catalog_prompt``."""
    catalog = default_model_catalog()
    by_id = {entry["id"]: entry for entry in catalog}
    by_id[SUMMARY_MODEL]["thread_summary_prompt"] = catalog_prompt
    small = copy.deepcopy(by_id["gpt-text-only"])
    small.update(
        id=CHAT_MODEL,
        provider_model_id="gpt-small-ctx-2025-01-01",
        display_name="GPT Small Context",
        context_window=CHAT_CONTEXT_WINDOW,
        max_input_tokens=CHAT_CONTEXT_WINDOW,
        max_output_tokens=CHAT_MAX_OUTPUT_TOKENS,
    )
    small["preference"] = {"is_default": False, "sort_order": 9}
    catalog.append(small)
    return catalog


def summary_overrides(
    *, catalog_prompt: str = CATALOG_SUMMARY_PROMPT, summary_model_id: str = SUMMARY_MODEL
) -> dict[str, Any]:
    return {
        "gears": {
            "mini-chat": {
                "config": {
                    "streaming": {"max_output_tokens": STREAMING_MAX_OUTPUT_TOKENS},
                    "thread_summary_worker": {
                        "enabled": True,
                        "summary_model_id": summary_model_id,
                        "summary_system_prompt": CONFIG_SUMMARY_PROMPT,
                        "message_content_limit": MESSAGE_CONTENT_LIMIT,
                        "max_attempts": SUMMARY_MAX_ATTEMPTS,
                        "compression_threshold_pct": COMPRESSION_THRESHOLD_PCT,
                    },
                }
            },
            POLICY_PLUGIN: {"config": {"model_catalog": summary_catalog(catalog_prompt)}},
        }
    }


@pytest.fixture(scope="module")
def _s6_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


@pytest.fixture(scope="module")
def summary_server(mock, _s6_servers):
    server = create_server(mock, summary_overrides(), name="s6-summary")
    _s6_servers.append(server)
    server.start()
    return server.wait_ready()


# --------------------------------------------------------------------------
# Arithmetic (DESIGN §3.6 budget, 5.5.4 per-item estimate)
# --------------------------------------------------------------------------


def _ceil_div(n: int, d: int) -> int:
    return -(-n // d)


def catalog_entry(server, model_id: str) -> dict[str, Any]:
    for entry in server.catalog:
        if entry["id"] == model_id:
            return entry
    raise AssertionError(f"model {model_id!r} not in the server catalog")


def item_tokens(text: str, budgets: dict[str, Any]) -> int:
    """``ceil(ceil(utf8_bytes / bytes_per_token) * (100 + safety_margin_pct) / 100)``."""
    size = len(text.encode("utf-8"))
    if size == 0:
        return 0
    per_token = max(1, int(budgets["bytes_per_token_conservative"]))
    return _ceil_div(_ceil_div(size, per_token) * (100 + int(budgets["safety_margin_pct"])), 100)


@dataclass(frozen=True)
class Plan:
    assembled: int
    effective_budget: int
    token_budget: int
    truncated: bool
    summary_kept: bool

    @property
    def reaches_threshold(self) -> bool:
        return self.assembled * 100 >= self.effective_budget * COMPRESSION_THRESHOLD_PCT


def plan_context(
    server, history: list[tuple[str, str]], user_text: str, summary_text: str | None = None
) -> Plan:
    """Context plan of a text-only turn on :data:`CHAT_MODEL` (DESIGN "Context
    Plan Truncation Algorithm"); used to check the test's own preconditions."""
    entry = catalog_entry(server, CHAT_MODEL)
    budgets = entry["estimation_budgets"]
    max_out = min(int(entry["max_output_tokens"]), STREAMING_MAX_OUTPUT_TOKENS)
    window_budget = int(entry["context_window"]) - max_out
    max_input = int(entry.get("max_input_tokens") or 0)
    effective = min(max_input, window_budget) if max_input > 0 else window_budget
    token_budget = effective - int(budgets.get("fixed_overhead_tokens", DEFAULT_FIXED_OVERHEAD))
    mandatory = item_tokens(entry["system_prompt"], budgets) + item_tokens(user_text, budgets)
    assert mandatory <= token_budget, "test design: the mandatory items must fit"
    remaining = token_budget - mandatory
    summary_kept = False
    if summary_text is not None:
        cost = item_tokens(f"{PREAMBLE}\n\n{summary_text}", budgets)
        if cost <= remaining:
            remaining -= cost
            summary_kept = True
    kept: list[tuple[str, str]] = []
    for role, text in reversed(history):
        cost = item_tokens(text, budgets)
        if cost > remaining:
            break
        remaining -= cost
        kept.insert(0, (role, text))
    while kept and kept[0][0] == "assistant":
        remaining += item_tokens(kept.pop(0)[1], budgets)
    return Plan(
        assembled=token_budget - remaining,
        effective_budget=effective,
        token_budget=token_budget,
        truncated=len(kept) < len(history),
        summary_kept=summary_kept,
    )


# --------------------------------------------------------------------------
# Texts and prompt expectations
# --------------------------------------------------------------------------


def text_of(label: str, size: int) -> str:
    """``size`` ASCII characters starting with ``[label]`` and ending with ``.``."""
    head = f"[{label}] "
    body = (head + _FILLER * (size // len(_FILLER) + 2))[: size - 1]
    return body + "."


def clip(content: str) -> str:
    """Prompt form of a message: cut to ``message_content_limit`` characters + ``...``."""
    if MESSAGE_CONTENT_LIMIT and len(content) > MESSAGE_CONTENT_LIMIT:
        return content[:MESSAGE_CONTENT_LIMIT] + "..."
    return content


def prompt_entries(messages: list[dict[str, Any]]) -> str:
    """``User: …`` / ``Assistant: …`` entries separated by a blank line."""
    entries = []
    for message in messages:
        prefix = {"user": "User", "assistant": "Assistant"}[message["role"]]
        entries.append(f"{prefix}: {clip(message['content'])}")
    return "\n\n".join(entries)


def _item_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(p.get("text", "")) for p in content if isinstance(p, dict) and "text" in p)
    raise AssertionError(f"unexpected input content shape: {content!r}")


def system_instructions(body: dict[str, Any]) -> str:
    """Top-level ``instructions`` or the leading ``system`` / ``developer`` input item."""
    if body.get("instructions"):
        return str(body["instructions"])
    items = body.get("input") or []
    assert items and items[0].get("role") in ("system", "developer"), f"no system prompt: {body}"
    return _item_text(items[0].get("content"))


def user_prompt(body: dict[str, Any]) -> str:
    users = [i for i in body.get("input") or [] if isinstance(i, dict) and i.get("role") == "user"]
    assert len(users) == 1, f"the summary request carries one user prompt: {body.get('input')}"
    others = [
        i for i in body.get("input") or [] if i.get("role") not in ("user", "system", "developer")
    ]
    assert not others, f"the summary request carries no conversation items: {others}"
    return _item_text(users[0].get("content"))


def conversation(body: dict[str, Any]) -> list[tuple[str, str]]:
    return [
        (item.get("role"), _item_text(item.get("content")))
        for item in body.get("input") or []
        if item.get("role") not in ("system", "developer")
    ]


def assert_in_order(text: str, parts: list[str], start: int = 0) -> int:
    """Every part occurs in ``text`` after the previous one; returns the end offset."""
    position = start
    for part in parts:
        found = text.find(part, position)
        assert found >= 0, f"{part!r} not found (in order) after offset {position} in prompt:\n{text}"
        position = found + len(part)
    return position


def assert_analysis_instruction(prompt: str, after: int) -> None:
    assert_in_order(prompt, list(ANALYSIS_LINES), after)
    assert prompt.rstrip().endswith(ANALYSIS_LINES[-1]), f"the prompt must end with the analysis instruction:\n{prompt}"


# --------------------------------------------------------------------------
# HTTP / DB helpers
# --------------------------------------------------------------------------


def new_chat(server, user: str = "A1") -> str:
    resp = server.post(CHATS, user, json={"title": "S6 thread summary", "model": CHAT_MODEL})
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def send_done(server, chat_id: str, content: str, user: str = "A1") -> tuple[Any, str]:
    request_id = new_uuid()
    result = server.stream(
        "POST", f"{CHATS}/{chat_id}/messages:stream", user, json={"content": content, "request_id": request_id}
    )
    assert result.status_code == 200, (result.status_code, result.text or result.body)
    assert result.is_sse, result.headers
    assert result.terminal is not None and result.terminal[0] == "done", (result.names(), result.events[-1:])
    return result, request_id


def stream_requests(mock) -> list[dict[str, Any]]:
    return [r for r in mock.requests_for("responses") if r["stream"]]


def summary_requests(mock, chat_id: str | None = None) -> list[dict[str, Any]]:
    """Non-streaming Responses API calls (the thread-summary calls), optionally
    only those of ``chat_id`` (``metadata.chat_id``): the summary worker runs in
    the background, so a call of another chat may land during a test."""
    return [
        r
        for r in mock.requests_for("responses")
        if not r["stream"]
        and (chat_id is None or ((r.get("json") or {}).get("metadata") or {}).get("chat_id") == chat_id)
    ]


def live_messages(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query(
        "SELECT id, request_id, role, content, created_at, is_compressed FROM messages "
        "WHERE chat_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
        (db_uuid(chat_id),),
    )


def summary_rows(server, chat_id: str) -> list[dict[str, Any]]:
    return server.query("SELECT * FROM thread_summaries WHERE chat_id = ?", (db_uuid(chat_id),))


def wait_summary_row(server, chat_id: str, frontier_id: str, timeout: float = 45.0) -> dict[str, Any]:
    def probe() -> dict[str, Any] | None:
        rows = summary_rows(server, chat_id)
        for row in rows:
            if row["summarized_up_to_message_id"] == frontier_id:
                return row
        return None

    return wait_until(probe, timeout=timeout, interval=0.2, description=f"thread summary up to {frontier_id}")


def compressed_flags(server, chat_id: str) -> dict[str, int]:
    return {m["id"]: int(m["is_compressed"]) for m in live_messages(server, chat_id)}


def quota_rows(server, user: str = "A1") -> list[dict[str, Any]]:
    ident = server.identity(user)
    return server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, calls, "
        "input_tokens, output_tokens FROM quota_usage WHERE tenant_id = ? AND user_id = ? "
        "ORDER BY period_type, period_start, bucket",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )


def turn_count(server, chat_id: str) -> int:
    return server.query("SELECT COUNT(*) AS n FROM chat_turns WHERE chat_id = ?", (db_uuid(chat_id),))[0]["n"]


def queue_activity(server) -> int:
    return outbox_snapshot(server, THREAD_SUMMARY_QUEUE).activity


def _json_payload(raw: Any) -> Any:
    if isinstance(raw, (bytes, bytearray, memoryview)):
        raw = bytes(raw).decode("utf-8", errors="replace")
    try:
        return json.loads(str(raw))
    except json.JSONDecodeError:
        return None


def dead_letter_payloads(server) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    conn = server.sqlite()
    try:
        for tables in outbox_tables(server):
            exists = conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", (tables.dead_letters,)
            ).fetchone()
            if not exists:
                continue
            columns = {row[1] for row in conn.execute(f"PRAGMA table_info({tables.dead_letters})")}
            if "payload" not in columns:
                continue
            for (raw,) in conn.execute(f"SELECT payload FROM {tables.dead_letters}"):
                payload = _json_payload(raw)
                if isinstance(payload, dict):
                    found.append(payload)
    finally:
        conn.close()
    return found


def dead_summary_tasks(server, chat_id: str) -> list[dict[str, Any]]:
    return [
        p
        for p in dead_letter_payloads(server)
        if p.get("system_task_type") == SYSTEM_TASK_TYPE and str(p.get("chat_id", "")).lower() == chat_id
    ]


def same_ts(a: Any, b: Any) -> bool:
    return parse_ts_ns(str(a)) == parse_ts_ns(str(b))


# --------------------------------------------------------------------------
# Outbox capture
# --------------------------------------------------------------------------


class OutboxRecorder:
    """Captures the outbox payloads of one chat (bodies are vacuumed after ack)."""

    def __init__(self, server, chat_id: str, interval: float = 0.005) -> None:
        self._server = server
        self._chat_id = chat_id
        self._interval = interval
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._seen: dict[str, dict[str, Any]] = {}
        self._thread = threading.Thread(target=self._run, name="s6-outbox-recorder", daemon=True)

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

    def summary_tasks(self) -> list[dict[str, Any]]:
        return [p for p in self.payloads() if "frozen_target_message_id" in p]

    def system_usage_events(self) -> list[dict[str, Any]]:
        return [
            p
            for p in self.payloads()
            if "dedupe_key" in p and f"/{SYSTEM_TASK_TYPE}/" in str(p.get("dedupe_key", ""))
        ]


# --------------------------------------------------------------------------
# The three-turn conversation
# --------------------------------------------------------------------------


@dataclass
class Conversation:
    chat_id: str
    tenant_id: str
    tag: str
    texts: dict[str, str]
    request_ids: list[str]
    messages: list[dict[str, Any]] = field(default_factory=list)
    activity_before: int = 0

    def label(self, key: str) -> str:
        return f"[{self.tag}-{key}]"

    def by_key(self) -> dict[str, dict[str, Any]]:
        """Messages keyed ``U1``, ``A1``, … by their content label."""
        found = {}
        for message in self.messages:
            for key in self.texts:
                if message["content"].startswith(self.label(key)):
                    found[key] = message
        return found


def three_turns(server, mock, chat_id: str | None = None) -> Conversation:
    """Turns 1-3 of :data:`SIZES` on :data:`CHAT_MODEL`; turn 3 reaches the
    proactive threshold without truncation (preconditions asserted)."""
    tag = uuid.uuid4().hex[:8]
    texts = {key: text_of(f"{tag}-{key}", size) for key, size in SIZES.items()}
    chat_id = chat_id or new_chat(server)
    conv = Conversation(chat_id, server.identity("A1").tenant_id, tag, texts, [])

    p1 = plan_context(server, [], texts["U1"])
    p2 = plan_context(server, [("user", texts["U1"]), ("assistant", texts["A1"])], texts["U2"])
    history3 = [
        ("user", texts["U1"]),
        ("assistant", texts["A1"]),
        ("user", texts["U2"]),
        ("assistant", texts["A2"]),
    ]
    p3 = plan_context(server, history3, texts["U3"])
    assert not p1.reaches_threshold and not p2.reaches_threshold, ("test design", p1, p2)
    assert p3.reaches_threshold and not p3.truncated, ("test design", p3)

    conv.activity_before = queue_activity(server)
    mock.enqueue({"text": texts["A1"]}, {"text": texts["A2"]}, {"text": texts["A3"]})
    for key in ("U1", "U2"):
        _, request_id = send_done(server, chat_id, texts[key])
        conv.request_ids.append(request_id)
    assert summary_requests(mock, chat_id) == [], "no summary below the threshold"
    _, request_id = send_done(server, chat_id, texts["U3"])
    conv.request_ids.append(request_id)
    conv.messages = live_messages(server, chat_id)
    keys = conv.by_key()
    assert set(keys) == set(SIZES), f"unexpected stored conversation: {sorted(keys)}"
    for key, text in texts.items():
        assert keys[key]["content"] == text, key
    return conv


def wait_enqueued(server, before: int, expected: int) -> None:
    wait_until(
        lambda: queue_activity(server) >= before + expected,
        timeout=15,
        description=f"{expected} message(s) on {THREAD_SUMMARY_QUEUE!r}",
    )
    time.sleep(1.0)
    after = outbox_snapshot(server, THREAD_SUMMARY_QUEUE)
    assert after.registered_partitions > 0, f"queue {THREAD_SUMMARY_QUEUE!r} is not registered"
    assert after.activity - before == expected, (before, after)


def summarized(server, mock, recorder_chat: str | None = None) -> Conversation:
    """:func:`three_turns` with a successful summary (:data:`RAW_SUMMARY`) committed."""
    mock.enqueue_summary({"text": RAW_SUMMARY, "usage": SUMMARY_USAGE})
    conv = three_turns(server, mock, recorder_chat)
    wait_summary_row(server, conv.chat_id, conv.by_key()["A2"]["id"])
    return conv


# --------------------------------------------------------------------------
# Scheduling
# --------------------------------------------------------------------------


def test_turn_reaching_the_threshold_schedules_one_summary_with_the_frozen_target(summary_server, mock):
    server = summary_server
    chat_id = new_chat(server)
    with OutboxRecorder(server, chat_id) as recorder:
        conv = three_turns(server, mock, chat_id)
        # Turns 1 and 2 stayed below the threshold; turn 3 enqueued exactly one task.
        wait_enqueued(server, conv.activity_before, 1)
        tasks = wait_until(recorder.summary_tasks, timeout=15, description="thread summary payload")

    assert len(tasks) == 1, tasks
    task = tasks[0]
    keys = conv.by_key()
    assert str(task["tenant_id"]).lower() == conv.tenant_id, task
    assert str(task["chat_id"]).lower() == chat_id, task
    assert is_uuid(task["system_request_id"]), task
    assert uuid.UUID(str(task["system_request_id"])).version == 4, task
    assert str(task["system_request_id"]).lower() not in [r.lower() for r in conv.request_ids], (
        "system_request_id is server-generated, not a turn request_id"
    )
    assert task["system_task_type"] == SYSTEM_TASK_TYPE, task
    # No summary yet: empty base frontier.
    assert task.get("base_frontier_created_at") is None, task
    assert task.get("base_frontier_message_id") is None, task
    # Frozen target: the latest message that does not belong to the causing turn (A2).
    assert str(task["frozen_target_message_id"]).lower() == keys["A2"]["id"], task
    assert is_rfc3339(task["frozen_target_created_at"]), task
    assert same_ts(task["frozen_target_created_at"], keys["A2"]["created_at"]), (task, keys["A2"])
    # Let the scheduled task finish (mock default summary) so it does not run
    # into the next test on the shared server.
    wait_summary_row(server, chat_id, keys["A2"]["id"])


def test_summary_is_a_non_streaming_system_call_with_the_b55_prompt(summary_server, mock):
    server = summary_server
    conv = three_turns(server, mock)
    requests = wait_until(
        lambda: summary_requests(mock, conv.chat_id), timeout=45, description="summary provider call"
    )
    record = requests[0]
    body = record["json"]
    keys = conv.by_key()

    assert not record["stream"], "the summary call is non-streaming"
    assert body.get("stream") in (False, None), body.get("stream")
    summary_entry = catalog_entry(server, SUMMARY_MODEL)
    assert body["model"] == summary_entry["provider_model_id"], body["model"]
    assert body["max_output_tokens"] == summary_entry["max_output_tokens"], (
        "max_output_tokens is the summary model's catalog value, not capped by streaming.max_output_tokens"
    )
    assert not body.get("tools"), body.get("tools")
    expected_user = uuid.UUID(conv.tenant_id).hex + uuid.UUID(SYSTEM_SUBJECT_ID).hex
    assert body["user"] == expected_user, body["user"]
    assert body["metadata"] == {
        "tenant_id": conv.tenant_id,
        "user_id": SYSTEM_SUBJECT_ID,
        "chat_id": conv.chat_id,
        "request_type": "summary",
        "feature": "none",
    }, body["metadata"]

    # The catalog thread_summary_prompt of the summary model wins over the config fallback.
    assert system_instructions(body) == CATALOG_SUMMARY_PROMPT

    prompt = user_prompt(body)
    assert prompt.startswith(OPENING_FIRST), prompt[:200]
    assert "<existing_summary>" not in prompt
    covered = [keys[k] for k in ("U1", "A1", "U2", "A2")]
    assert [len(m["content"]) > MESSAGE_CONTENT_LIMIT for m in covered] == [True, True, False, True], (
        "test design: both clipped and unclipped entries"
    )
    entries = prompt_entries(covered)
    start = prompt.find(entries, len(OPENING_FIRST))
    assert start >= 0, f"entries block missing or not in order:\n{prompt}"
    for key in ("U1", "A1", "A2"):
        assert keys[key]["content"][MESSAGE_CONTENT_LIMIT:] not in prompt, f"{key} must be cut"
    # The causing turn is never summarized.
    for key in ("U3", "A3"):
        assert conv.label(key) not in prompt, f"causing turn message {key} in the summary prompt"
    assert_analysis_instruction(prompt, start + len(entries))


# --------------------------------------------------------------------------
# Commit
# --------------------------------------------------------------------------


def test_summary_commit_stores_frontier_text_estimate_and_compresses_exact_range(summary_server, mock):
    server = summary_server
    mock.enqueue_summary({"text": RAW_SUMMARY, "usage": SUMMARY_USAGE})
    conv = three_turns(server, mock)
    quota_after_turns = quota_rows(server)
    keys = conv.by_key()

    row = wait_summary_row(server, conv.chat_id, keys["A2"]["id"])
    rows = summary_rows(server, conv.chat_id)
    assert len(rows) == 1, rows
    assert row["tenant_id"] == conv.tenant_id, row
    assert row["summary_text"] == STORED_SUMMARY, row["summary_text"]
    assert int(row["token_estimate"]) == SUMMARY_TOKEN_ESTIMATE, row
    assert row["summarized_up_to_created_at"] == keys["A2"]["created_at"], (row, keys["A2"])

    flags = compressed_flags(server, conv.chat_id)
    for key in ("U1", "A1", "U2", "A2"):
        assert flags[keys[key]["id"]] == 1, f"{key} is in the summarized range"
    for key in ("U3", "A3"):
        assert flags[keys[key]["id"]] == 0, f"{key} belongs to the causing turn"

    time.sleep(1.0)
    assert len(summary_requests(mock)) == 1, "one successful call, no retry"
    assert turn_count(server, conv.chat_id) == 3, "a system task creates no chat_turns row"
    assert quota_rows(server) == quota_after_turns, "a system task debits no per-user quota"


def test_summary_commit_emits_a_system_task_usage_event(summary_server, mock):
    server = summary_server
    chat_id = new_chat(server)
    mock.enqueue_summary({"text": RAW_SUMMARY, "usage": SUMMARY_USAGE})
    with OutboxRecorder(server, chat_id) as recorder:
        conv = three_turns(server, mock, chat_id)
        wait_summary_row(server, chat_id, conv.by_key()["A2"]["id"])
        events = wait_until(recorder.system_usage_events, timeout=20, interval=0.05, description="system usage event")
        time.sleep(1.0)
        events = recorder.system_usage_events()
        tasks = recorder.summary_tasks()

    assert len(events) == 1, events
    event = events[0]
    assert len(tasks) == 1, tasks
    system_request_id = str(tasks[0]["system_request_id"]).lower()

    assert "user_id" not in event, f"user_id is omitted for system tasks: {event}"
    assert "turn_id" not in event, f"turn_id is omitted for system tasks: {event}"
    assert str(event["tenant_id"]).lower() == conv.tenant_id, event
    assert str(event["chat_id"]).lower() == chat_id, event
    assert str(event["request_id"]).lower() == system_request_id, event
    assert event["dedupe_key"] == (
        f"{uuid.UUID(conv.tenant_id).hex}/{SYSTEM_TASK_TYPE}/{uuid.UUID(system_request_id).hex}"
    ), event
    assert event["system_task_type"] == SYSTEM_TASK_TYPE, event
    assert event["requester_type"] == "system", event
    assert event["billing_outcome"] == "system_task", event
    assert event["settlement_method"] == "none", event
    assert int(event["actual_credits_micro"]) == 0, event
    assert event["terminal_state"] == "completed", event
    assert event["effective_model"] == SUMMARY_MODEL, event
    assert event["selected_model"] == SUMMARY_MODEL, event
    assert (event["web_search_calls"], event["code_interpreter_calls"], event["file_search_calls"]) == (0, 0, 0)
    assert is_rfc3339(event["timestamp"]), event
    usage = event["usage"]
    assert usage is not None, event
    assert usage["input_tokens"] == SUMMARY_USAGE["input_tokens"], usage
    assert usage["output_tokens"] == SUMMARY_USAGE["output_tokens"], usage
    assert usage["reasoning_tokens"] == SUMMARY_USAGE["reasoning_tokens"], usage
    assert usage["cache_read_input_tokens"] == SUMMARY_USAGE["cached_tokens"], usage


# --------------------------------------------------------------------------
# Use of the summary in the next turn
# --------------------------------------------------------------------------


def test_next_turn_sends_the_summary_with_preamble_instead_of_compressed_messages(summary_server, mock):
    server = summary_server
    conv = summarized(server, mock)
    keys = conv.by_key()
    u4 = text_of(f"{conv.tag}-U4", SHORT_U4)
    history = [("user", conv.texts["U3"]), ("assistant", conv.texts["A3"])]
    plan = plan_context(server, history, u4, STORED_SUMMARY)
    assert plan.summary_kept and not plan.truncated, ("test design", plan)
    before = queue_activity(server)
    calls_before = len(summary_requests(mock))

    result, _ = send_done(server, conv.chat_id, u4)

    started = result.first("stream_started")
    assert started.get("thread_summary_applied") == {"token_estimate": SUMMARY_TOKEN_ESTIMATE}, started
    body = stream_requests(mock)[-1]["json"]
    items = conversation(body)
    assert len(items) == 4, items
    role, summary_message = items[0]
    assert role == "user", items[0]
    assert summary_message.startswith(PREAMBLE), summary_message
    assert summary_message.endswith(STORED_SUMMARY), summary_message
    assert items[1:] == [
        ("user", conv.texts["U3"]),
        ("assistant", conv.texts["A3"]),
        ("user", u4),
    ], items[1:]
    sent = json.dumps(body)
    for key in ("U1", "A1", "U2", "A2"):
        assert conv.label(key) not in sent, f"compressed message {key} was sent to the provider"
    assert keys["A2"]["id"] == summary_rows(server, conv.chat_id)[0]["summarized_up_to_message_id"]

    # A summary exists and nothing was truncated: no new summary work.
    time.sleep(1.5)
    assert queue_activity(server) == before, "no thread-summary task without truncation"
    assert len(summary_requests(mock)) == calls_before


# --------------------------------------------------------------------------
# Failures, retries, dead letters
# --------------------------------------------------------------------------


def test_provider_failure_of_the_summary_call_is_retried(summary_server, mock):
    server = summary_server
    mock.enqueue_summary(
        {"kind": "http_error", "status": 500},
        {"text": RAW_SUMMARY, "usage": SUMMARY_USAGE},
    )
    conv = three_turns(server, mock)
    keys = conv.by_key()
    flags_before = compressed_flags(server, conv.chat_id)

    row = wait_summary_row(server, conv.chat_id, keys["A2"]["id"])

    calls = summary_requests(mock)
    assert len(calls) == 2, f"one failed call and one retry: {len(calls)}"
    assert calls[0]["json"]["input"] == calls[1]["json"]["input"], "the retry summarizes the same frozen range"
    assert row["summary_text"] == STORED_SUMMARY, row
    assert all(v == 0 for v in flags_before.values()), "nothing compressed before the commit"
    assert len(summary_rows(server, conv.chat_id)) == 1


def test_empty_summary_result_is_retried(summary_server, mock):
    server = summary_server
    mock.enqueue_summary(
        {"text": "<analysis>Nothing worth keeping.</analysis>"},
        {"text": RAW_SUMMARY, "usage": SUMMARY_USAGE},
    )
    conv = three_turns(server, mock)

    row = wait_summary_row(server, conv.chat_id, conv.by_key()["A2"]["id"])

    assert len(summary_requests(mock)) == 2, "the empty result is retried"
    assert row["summary_text"] == STORED_SUMMARY, row
    assert int(row["token_estimate"]) == SUMMARY_TOKEN_ESTIMATE, row


def test_summary_failing_on_every_attempt_is_dead_lettered_after_max_attempts(summary_server, mock):
    server = summary_server
    mock.enqueue_summary(*[{"kind": "http_error", "status": 500}] * SUMMARY_MAX_ATTEMPTS)
    conv = three_turns(server, mock)

    dead = wait_until(
        lambda: dead_summary_tasks(server, conv.chat_id), timeout=60, description="dead-lettered summary task"
    )
    time.sleep(2.0)

    assert len(dead) == 1, dead
    assert str(dead[0]["frozen_target_message_id"]).lower() == conv.by_key()["A2"]["id"], dead
    assert len(summary_requests(mock)) == SUMMARY_MAX_ATTEMPTS, "no call after the last attempt"
    assert summary_rows(server, conv.chat_id) == [], "the previous (empty) summary is kept"
    assert all(v == 0 for v in compressed_flags(server, conv.chat_id).values()), "nothing compressed"


@pytest.mark.parametrize("summary_model_id", ["gpt-summary-missing", "gpt-disabled"])
def test_unavailable_summary_model_rejects_the_task_without_a_provider_call(start_server, mock, summary_model_id):
    server = start_server(summary_overrides(summary_model_id=summary_model_id), name="s6-summary-model")
    conv = three_turns(server, mock)
    wait_enqueued(server, conv.activity_before, 1)

    dead = wait_until(
        lambda: dead_summary_tasks(server, conv.chat_id), timeout=30, description="dead-lettered summary task"
    )
    time.sleep(1.5)

    assert len(dead) == 1, dead
    assert summary_requests(mock) == [], "a missing or disabled summary model is never called"
    assert summary_rows(server, conv.chat_id) == []
    assert all(v == 0 for v in compressed_flags(server, conv.chat_id).values())
    # The chat keeps working.
    send_done(server, conv.chat_id, text_of(f"{conv.tag}-U4", SHORT_U4))


def test_configured_summary_prompt_is_used_when_the_catalog_prompt_is_empty(start_server, mock):
    server = start_server(summary_overrides(catalog_prompt=""), name="s6-summary-config-prompt")
    three_turns(server, mock)

    requests = wait_until(lambda: summary_requests(mock), timeout=45, description="summary provider call")

    assert system_instructions(requests[0]["json"]) == CONFIG_SUMMARY_PROMPT


# --------------------------------------------------------------------------
# Urgent re-summarize
# --------------------------------------------------------------------------


def test_truncation_with_an_existing_summary_triggers_an_urgent_resummarize(summary_server, mock):
    server = summary_server
    chat_id = new_chat(server)
    with OutboxRecorder(server, chat_id) as recorder:
        conv = summarized(server, mock, chat_id)
        keys = conv.by_key()
        first = summary_rows(server, chat_id)[0]
        u4 = text_of(f"{conv.tag}-U4", HUGE_U4)
        history = [("user", conv.texts["U3"]), ("assistant", conv.texts["A3"])]
        plan = plan_context(server, history, u4, STORED_SUMMARY)
        assert plan.truncated and plan.summary_kept, ("test design", plan)
        mock.enqueue_summary({"text": RAW_SUMMARY_2, "usage": SUMMARY_USAGE_2})
        calls_before = len(summary_requests(mock))
        before = queue_activity(server)

        result, _ = send_done(server, chat_id, u4)

        started = result.first("stream_started")
        assert started.get("thread_summary_applied") == {"token_estimate": SUMMARY_TOKEN_ESTIMATE}, started
        wait_enqueued(server, before, 1)
        row = wait_summary_row(server, chat_id, keys["A3"]["id"])
        tasks = recorder.summary_tasks()

    # Durable task: base = the previous frontier, target = the causing turn's predecessor (A3).
    urgent = [t for t in tasks if str(t["frozen_target_message_id"]).lower() == keys["A3"]["id"]]
    assert len(urgent) == 1, tasks
    task = urgent[0]
    assert str(task["base_frontier_message_id"]).lower() == keys["A2"]["id"], task
    assert same_ts(task["base_frontier_created_at"], keys["A2"]["created_at"]), task
    assert same_ts(task["frozen_target_created_at"], keys["A3"]["created_at"]), task

    calls = summary_requests(mock)[calls_before:]
    assert len(calls) == 1, calls
    prompt = user_prompt(calls[0]["json"])
    assert OPENING_FIRST not in prompt, "a re-summarize uses the merge opening"
    assert prompt.startswith(MERGE_OPENING[0]), prompt[:300]
    block = re.search(r"<existing_summary>\s*(.*?)\s*</existing_summary>", prompt, re.S)
    assert block is not None, f"no <existing_summary> block:\n{prompt}"
    assert block.group(1) == first["summary_text"] == STORED_SUMMARY, block.group(1)
    after_opening = assert_in_order(prompt, [MERGE_OPENING[0], MERGE_OPENING[1]])
    assert block.start() >= after_opening, "the existing summary follows the merge instruction"
    header = prompt.find(NEW_MESSAGES_HEADER, block.end())
    assert header >= 0, f"{NEW_MESSAGES_HEADER!r} missing after the existing summary:\n{prompt}"
    entries = prompt_entries([keys["U3"], keys["A3"]])
    start = prompt.find(entries, header + len(NEW_MESSAGES_HEADER))
    assert start >= 0, f"new-message entries missing:\n{prompt}"
    for key in ("U1", "A1", "U2", "A2", "U4"):
        assert conv.label(key) not in prompt, f"{key} is outside (base, target]"
    assert_analysis_instruction(prompt, start + len(entries))

    # CAS commit: one row, advanced frontier, ceil(bytes / 4) estimate.
    rows = summary_rows(server, chat_id)
    assert len(rows) == 1, rows
    assert row["summary_text"] == STORED_SUMMARY_2, row
    assert row["summarized_up_to_created_at"] == keys["A3"]["created_at"], row
    assert int(row["token_estimate"]) == -(-len(STORED_SUMMARY_2.encode("utf-8")) // 4), row
    flags = compressed_flags(server, chat_id)
    for key in ("U1", "A1", "U2", "A2", "U3", "A3"):
        assert flags[keys[key]["id"]] == 1, key
    newest = [m for m in live_messages(server, chat_id) if m["id"] not in {k["id"] for k in keys.values()}]
    assert len(newest) == 2 and all(int(m["is_compressed"]) == 0 for m in newest), newest
