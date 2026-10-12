"""S3 streaming: pre-stream rejections and post-open provider errors.

Contract sources (gears/mini-chat/docs/DESIGN.md, docs/api/api.json):

* §3.3 "Streaming Contract" (``StreamMessageRequest``): request body, the
  "Error model (Option A)" (validation / authorization / preflight failures are
  JSON ``Problem`` responses, no SSE stream), ``attachment_ids`` validation and
  the "Attachment Preflight Validation Invariant" (the reserve, the user
  message and the links are rolled back, no provider call);
* §3.3 "Error Codes" REST table: ``EMPTY_CONTENT``, 422 ``invalid_json_body``,
  400 ``json_syntax_error``, 415 ``missing_json_content_type``,
  ``invalid_path_params``, ``not_found`` with the chat resource type,
  ``invalid_attachment``, ``TOO_MANY_IMAGES``, ``VISION_NOT_SUPPORTED``,
  ``FEATURE_DISABLED``, ``INVALID_MODEL``, ``INPUT_TOO_LONG``,
  ``CONTEXT_BUDGET_EXCEEDED``;
* §2.2 "Model Capability Constraint (Images)" (``field_violations[content_type]``),
  "Context Window Budget", kill switches ``disable_images`` /
  ``disable_web_search``;
* §3.6 "Send Message with Streaming Response": every check runs before the
  single reserve transaction, "A failure before it leaves nothing behind";
* "Context Plan Truncation Algorithm" and §5.5.4 (estimation) for the budget
  numbers; catalog field ``max_input_tokens`` (``INPUT_TOO_LONG``);
* "Provider Event Translation", "Streaming error codes", ``event: error``
  (``ErrorData`` ``{code, message}``), the provider identifier non-exposure
  invariant (sanitization to ``[provider_id]`` / ``[credential]`` / ``[url]``),
  "SSE Event Ordering" and the stream close rules;
* Turn Status API (internal ``failed`` -> ``error`` with ``error_code``, no
  ``assistant_message_id``).

Pre-stream checks assert: a canonical Problem (no SSE), no streaming provider
request recorded by the mock, no ``chat_turns`` / ``messages`` /
``message_attachments`` rows, an untouched ``chats.updated_at`` and unchanged
``quota_usage`` rows of the caller (no reserve left behind).
"""

from __future__ import annotations

import copy
import json
import uuid
from typing import Any, Iterator

import httpx
import pytest

from conftest import create_server, default_model_catalog
from mc_helpers import (
    RT_CHAT,
    SseResult,
    assert_problem,
    db_uuid,
    new_uuid,
    problem_category,
)
from mc_seed import is_rfc3339, seed_attachment, seed_chat

CHATS = "/mini-chat/v1/chats"
TURN_STATUS_FIELDS = {"request_id", "state", "updated_at", "error_code", "assistant_message_id"}

#: Provider error text with every class of identifier DESIGN requires to be
#: sanitized, plus ordinary words the length floor must keep intact.
RAW_PROVIDER_MESSAGE = (
    "Upstream failure for response resp_abc123def456ghi789 (msg_ZyXw9876543210) "
    "while reading file-AbCdEf123456789012 from vs_QwErTy1234567890 "
    "with key sk-AbCdEfGhIjKlMnOp0123 at https://internal-llm.example.com/v1/responses "
    "and file-based file_search was skipped"
)
SANITIZED_PROVIDER_MESSAGE = (
    "Upstream failure for response [provider_id] ([provider_id]) "
    "while reading [provider_id] from [provider_id] "
    "with key [credential] at [url] "
    "and file-based file_search was skipped"
)
#: Raw fragments that must never reach a client.
RAW_IDENTIFIERS = (
    "resp_abc123def456ghi789",
    "msg_ZyXw9876543210",
    "file-AbCdEf123456789012",
    "vs_QwErTy1234567890",
    "sk-AbCdEfGhIjKlMnOp0123",
    "AbCdEfGhIjKlMnOp0123",
    "internal-llm.example.com",
    "https://",
)
#: Provider identifiers and the URL of ``mock_provider.DEFAULT_FAILED_MESSAGE``
#: (its ``sk-proj-...`` key is left out: DESIGN only defines an ``sk-`` key as
#: the prefix followed by 10 letters or digits; :data:`RAW_PROVIDER_MESSAGE`
#: covers the credential rule exactly).
DEFAULT_MESSAGE_IDENTIFIERS = (
    "resp_abc123def456ghi789",
    "file-AbCdEf123456789012",
    "internal-llm.example.com",
)


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def _stream_path(chat_id: str) -> str:
    return f"{CHATS}/{chat_id}/messages:stream"


def _turn_path(chat_id: str, request_id: str) -> str:
    return f"{CHATS}/{chat_id}/turns/{request_id}"


def _new_chat(server, user: str = "A1", model: str | None = None) -> str:
    body: dict[str, Any] = {"title": "S3 preflight"}
    if model is not None:
        body["model"] = model
    resp = server.post(CHATS, user, json=body)
    assert resp.status_code == 201, resp.text
    return resp.json()["id"]


def _seed_att(server, chat_id: str, owner: str = "A1", **kwargs: Any) -> str:
    ident = server.identity(owner)
    return seed_attachment(server, chat_id, ident.tenant_id, ident.user_id, **kwargs)


def _stream_requests(mock) -> list[dict[str, Any]]:
    """Streaming LLM calls recorded by the mock (thread summaries are non-streaming)."""
    return [r for r in mock.requests() if r.get("stream")]


def _quota_rows(server, user: str) -> dict[tuple[str, str, str], tuple[int, int, int]]:
    ident = server.identity(user)
    rows = server.query(
        "SELECT period_type, period_start, bucket, spent_credits_micro, reserved_credits_micro, calls "
        "FROM quota_usage WHERE tenant_id = ? AND user_id = ?",
        (db_uuid(ident.tenant_id), db_uuid(ident.user_id)),
    )
    return {
        (str(r["period_type"]), str(r["period_start"]), str(r["bucket"])): (
            int(r["spent_credits_micro"] or 0),
            int(r["reserved_credits_micro"] or 0),
            int(r["calls"] or 0),
        )
        for r in rows
    }


def _count(server, table: str, chat_id: str) -> int:
    return int(
        server.query(f"SELECT COUNT(*) AS n FROM {table} WHERE chat_id = ?", (db_uuid(chat_id),))[0]["n"]
    )


def _snapshot(server, mock, user: str, chat_id: str | None) -> dict[str, Any]:
    snap: dict[str, Any] = {
        "stream_requests": len(_stream_requests(mock)),
        "quota": _quota_rows(server, user),
    }
    if chat_id is not None:
        snap["messages"] = _count(server, "messages", chat_id)
        snap["turns"] = _count(server, "chat_turns", chat_id)
        snap["links"] = _count(server, "message_attachments", chat_id)
        rows = server.query("SELECT updated_at FROM chats WHERE id = ?", (db_uuid(chat_id),))
        snap["chat_updated_at"] = rows[0]["updated_at"] if rows else None
    return snap


def _assert_nothing_left_behind(
    server, mock, user: str, chat_id: str | None, before: dict[str, Any], fresh_chat: bool = True
) -> None:
    after = _snapshot(server, mock, user, chat_id)
    assert after["stream_requests"] == before["stream_requests"] == 0, (
        f"a pre-stream rejection must not call the provider: {_stream_requests(mock)}"
    )
    assert after["quota"] == before["quota"], (
        f"quota_usage changed by a rejected request (reserve left behind?): "
        f"before={before['quota']} after={after['quota']}"
    )
    if chat_id is not None:
        for key in ("messages", "turns", "links", "chat_updated_at"):
            assert after[key] == before[key], f"{key} changed: {before[key]!r} -> {after[key]!r}"
        if fresh_chat:
            assert after["turns"] == 0 and after["messages"] == 0 and after["links"] == 0, after


def _assert_not_sse(resp: httpx.Response) -> None:
    content_type = resp.headers.get("content-type", "")
    assert not content_type.startswith("text/event-stream"), (
        f"a pre-stream rejection must not open an SSE stream: {resp.status_code} {resp.text[:500]}"
    )


def _reject(
    server,
    mock,
    chat_id: str | None,
    body: Any,
    *,
    user: str = "A1",
    path_chat_id: str | None = None,
    raw: bytes | None = None,
    headers: dict[str, str] | None = None,
    fresh_chat: bool = True,
    **problem: Any,
) -> dict[str, Any]:
    """POST a stream request that must be rejected before the stream opens."""
    before = _snapshot(server, mock, user, chat_id)
    target = path_chat_id if path_chat_id is not None else chat_id
    if raw is not None:
        resp = server.post(_stream_path(target), user, content=raw, headers=headers or {})
    else:
        resp = server.post(_stream_path(target), user, json=body, headers=headers or {})
    _assert_not_sse(resp)
    status = problem.pop("status")
    category = problem.pop("category", None)
    result = assert_problem(resp, status, category, **problem)
    _assert_nothing_left_behind(server, mock, user, chat_id, before, fresh_chat)
    return result


def _assert_extractor_problem(resp_problem: dict[str, Any]) -> None:
    assert problem_category(resp_problem) == "invalid_argument", resp_problem


def _catalog_variant(base_id: str, model_id: str, sort_order: int, **changes: Any) -> dict[str, Any]:
    entry = copy.deepcopy(next(e for e in default_model_catalog() if e["id"] == base_id))
    budgets = changes.pop("estimation_budgets", None)
    entry.update(changes)
    entry["id"] = model_id
    entry["provider_model_id"] = f"{model_id}-provider"
    entry["display_name"] = model_id
    entry["preference"] = {"is_default": False, "sort_order": sort_order}
    if budgets:
        entry["estimation_budgets"] = {**entry["estimation_budgets"], **budgets}
    return entry


@pytest.fixture(scope="module")
def _module_servers(mock) -> Iterator[list[Any]]:
    servers: list[Any] = []
    yield servers
    for server in servers:
        server.stop()
        server.cleanup()


def _module_server(mock, servers: list[Any], name: str, overrides: dict[str, Any]):
    server = create_server(mock, overrides, name=name)
    servers.append(server)
    server.start()
    return server.wait_ready()


# Kill switches: one server with both stream-relevant switches on.
KILL_SWITCH_OVERRIDES = {
    "gears": {
        "static-mini-chat-model-policy-plugin": {
            "config": {"kill_switches": {"disable_images": True, "disable_web_search": True}}
        }
    }
}

# Budget models. ``fixed_overhead_tokens`` and ``safety_margin_pct`` are 0, so
# every item estimate is exactly ceil(utf8_bytes / 4) (§5.5.4, "Context Plan
# Truncation Algorithm"); no tools, so no surcharges apply.
ZERO_OVERHEAD = {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 0, "safety_margin_pct": 0}
INPUT_LIMIT_MODEL = "gpt-e2e-input-50"
SMALL_CONTEXT_MODEL = "gpt-e2e-context-1000"
OVERFLOW_CONTEXT_MODEL = "gpt-e2e-output-exceeds-context"
SMALL_CONTEXT_SYSTEM_PROMPT = "S" * 200  # 50 tokens


def _budget_catalog() -> list[dict[str, Any]]:
    return default_model_catalog() + [
        # max_input_tokens = 50: the current-message estimate above 50 is INPUT_TOO_LONG.
        _catalog_variant(
            "gpt-text-only",
            INPUT_LIMIT_MODEL,
            10,
            context_window=128_000,
            max_output_tokens=1_000,
            max_input_tokens=50,
            system_prompt="You are terse.",
            estimation_budgets=ZERO_OVERHEAD,
        ),
        # input_limit = min(1000, 1000 - 900) = 100; token_budget = 100 - 0 - 0 = 100.
        # System prompt 200 bytes = 50 tokens.
        _catalog_variant(
            "gpt-text-only",
            SMALL_CONTEXT_MODEL,
            11,
            context_window=1_000,
            max_output_tokens=900,
            max_input_tokens=1_000,
            system_prompt=SMALL_CONTEXT_SYSTEM_PROMPT,
            estimation_budgets=ZERO_OVERHEAD,
        ),
        # max_output_tokens_applied (600) >= context_window (500): always CONTEXT_BUDGET_EXCEEDED.
        _catalog_variant(
            "gpt-text-only",
            OVERFLOW_CONTEXT_MODEL,
            12,
            context_window=500,
            max_output_tokens=600,
            max_input_tokens=500,
            system_prompt="You are terse.",
            estimation_budgets=ZERO_OVERHEAD,
        ),
    ]


@pytest.fixture(scope="module")
def killswitch_server(mock, _module_servers):
    return _module_server(mock, _module_servers, "killswitch", KILL_SWITCH_OVERRIDES)


@pytest.fixture(scope="module")
def budget_server(mock, _module_servers):
    overrides = {
        "gears": {
            "static-mini-chat-model-policy-plugin": {"config": {"model_catalog": _budget_catalog()}}
        }
    }
    return _module_server(mock, _module_servers, "budget", overrides)


@pytest.fixture(scope="module")
def timeout_server(mock, _module_servers):
    overrides = {"gears": {"oagw": {"config": {"proxy_timeout_secs": 2}}}}
    return _module_server(mock, _module_servers, "oagw-timeout", overrides)


def _assert_stream_ok_control(server, chat_id: str, content: str) -> None:
    result = server.stream("POST", _stream_path(chat_id), "A1", json={"content": content})
    assert result.status_code == 200 and result.is_sse, (result.status_code, result.text[:500])
    assert result.terminal is not None and result.terminal[0] == "done", result.events


# --------------------------------------------------------------------------
# Request body validation
# --------------------------------------------------------------------------


@pytest.mark.parametrize("content", ["", "   ", " \n\t  \r\n "], ids=["empty", "spaces", "whitespace"])
def test_empty_or_whitespace_content_is_empty_content(mc, mock, content):
    chat_id = _new_chat(mc)

    _reject(
        mc,
        mock,
        chat_id,
        {"content": content},
        status=400,
        category="invalid_argument",
        field_name="content",
        field_reason="EMPTY_CONTENT",
    )


@pytest.mark.parametrize(
    "body",
    [
        {},
        {"request_id": "6f9619ff-8b86-d011-b42d-00cf4fc964ff"},
        {"content": 5},
        {"content": "hi", "request_id": "not-a-uuid"},
        {"content": "hi", "request_id": 42},
        {"content": "hi", "attachment_ids": ["not-a-uuid"]},
        {"content": "hi", "attachment_ids": "6f9619ff-8b86-d011-b42d-00cf4fc964ff"},
        {"content": "hi", "web_search": {"enabled": "yes"}},
    ],
    ids=[
        "missing-content",
        "missing-content-with-request-id",
        "content-not-string",
        "request-id-not-uuid",
        "request-id-not-string",
        "attachment-id-not-uuid",
        "attachment-ids-not-array",
        "web-search-enabled-not-bool",
    ],
)
def test_schema_mismatch_is_422(mc, mock, body):
    chat_id = _new_chat(mc)

    problem = _reject(
        mc,
        mock,
        chat_id,
        body,
        status=422,
        field_name="body",
        field_reason="invalid_json_body",
    )
    _assert_extractor_problem(problem)


def test_malformed_json_is_400_json_syntax_error(mc, mock):
    chat_id = _new_chat(mc)

    problem = _reject(
        mc,
        mock,
        chat_id,
        None,
        raw=b'{"content": "broken"',
        headers={"Content-Type": "application/json"},
        status=400,
        field_name="body",
        field_reason="json_syntax_error",
    )
    _assert_extractor_problem(problem)


def test_body_without_json_content_type_is_415(mc, mock):
    chat_id = _new_chat(mc)

    problem = _reject(
        mc,
        mock,
        chat_id,
        None,
        raw=b'{"content": "no content type"}',
        status=415,
        field_name="body",
        field_reason="missing_json_content_type",
    )
    _assert_extractor_problem(problem)


def test_non_uuid_chat_path_is_invalid_path_params(mc, mock):
    _reject(
        mc,
        mock,
        None,
        {"content": "hi"},
        path_chat_id="not-a-uuid",
        status=400,
        category="invalid_argument",
        field_reason="invalid_path_params",
    )


# --------------------------------------------------------------------------
# Chat resolution (404 masking)
# --------------------------------------------------------------------------


@pytest.mark.parametrize("requester", ["A2", "B1"], ids=["same-tenant-other-user", "other-tenant"])
def test_foreign_chat_is_not_found(mc, mock, requester):
    chat_id = _new_chat(mc, "A1")
    before_owner = _snapshot(mc, mock, "A1", chat_id)

    _reject(
        mc,
        mock,
        chat_id,
        {"content": "let me in"},
        user=requester,
        status=404,
        category="not_found",
        resource_type=RT_CHAT,
    )
    _assert_nothing_left_behind(mc, mock, "A1", chat_id, before_owner)


def test_deleted_chat_is_not_found(mc, mock):
    chat_id = _new_chat(mc)
    resp = mc.delete(f"{CHATS}/{chat_id}", "A1")
    assert resp.status_code == 204, resp.text

    _reject(
        mc,
        mock,
        chat_id,
        {"content": "anyone there?"},
        status=404,
        category="not_found",
        resource_type=RT_CHAT,
    )


def test_unknown_chat_is_not_found(mc, mock):
    _reject(
        mc,
        mock,
        None,
        {"content": "hello"},
        path_chat_id=new_uuid(),
        status=404,
        category="not_found",
        resource_type=RT_CHAT,
    )


# --------------------------------------------------------------------------
# attachment_ids validation
# --------------------------------------------------------------------------


def _reject_attachment(mc, mock, chat_id: str, attachment_ids: list[str]) -> None:
    _reject(
        mc,
        mock,
        chat_id,
        {"content": "see attached", "attachment_ids": attachment_ids},
        status=400,
        category="invalid_argument",
        field_name="attachment",
        field_reason="invalid_attachment",
    )


def test_duplicate_attachment_ids_are_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc)
    doc = _seed_att(mc, chat_id)

    _reject_attachment(mc, mock, chat_id, [doc, doc])


def test_unknown_attachment_id_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc)

    _reject_attachment(mc, mock, chat_id, [new_uuid()])


def test_attachment_of_another_chat_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc)
    other_chat = _new_chat(mc)
    foreign_doc = _seed_att(mc, other_chat)

    _reject_attachment(mc, mock, chat_id, [foreign_doc])


@pytest.mark.parametrize("status", ["pending", "uploaded", "failed"])
def test_not_ready_attachment_is_invalid_attachment(mc, mock, status):
    chat_id = _new_chat(mc)
    doc = _seed_att(mc, chat_id, status=status)

    _reject_attachment(mc, mock, chat_id, [doc])


def test_not_ready_image_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc)
    image = _seed_att(mc, chat_id, kind="image", status="uploaded")

    _reject_attachment(mc, mock, chat_id, [image])


def test_soft_deleted_attachment_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc)
    doc = _seed_att(mc, chat_id, deleted=True)

    _reject_attachment(mc, mock, chat_id, [doc])


def test_attachment_uploaded_by_another_user_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc, "A1")
    a2_doc = _seed_att(mc, chat_id, owner="A2")

    _reject_attachment(mc, mock, chat_id, [a2_doc])


def test_attachment_of_another_tenant_is_invalid_attachment(mc, mock):
    chat_id = _new_chat(mc, "A1")
    b1_doc = _seed_att(mc, chat_id, owner="B1")

    _reject_attachment(mc, mock, chat_id, [b1_doc])


def test_one_bad_id_rejects_the_whole_list(mc, mock):
    chat_id = _new_chat(mc)
    good = _seed_att(mc, chat_id)
    bad = _seed_att(mc, chat_id, status="pending")

    _reject_attachment(mc, mock, chat_id, [good, bad])


def test_more_attachment_ids_than_documents_plus_images_limit_is_invalid_attachment(mc, mock):
    rag = mc.gear_config.get("rag", {})
    limit = int(rag.get("max_documents_per_chat", 50)) + int(rag.get("max_images_per_message", 4))
    chat_id = _new_chat(mc)
    # Every id is individually valid (ready, own chat, own user): only the count is wrong.
    docs = [_seed_att(mc, chat_id, filename=f"doc-{i}.pdf") for i in range(limit + 1)]

    _reject_attachment(mc, mock, chat_id, docs)


def test_more_images_than_max_images_per_message_is_too_many_images(mc, mock):
    max_images = int(mc.gear_config.get("rag", {}).get("max_images_per_message", 4))
    chat_id = _new_chat(mc, model="gpt-4.1")
    images = [
        _seed_att(mc, chat_id, kind="image", filename=f"img-{i}.png") for i in range(max_images + 1)
    ]

    _reject(
        mc,
        mock,
        chat_id,
        {"content": "describe these", "attachment_ids": images},
        status=400,
        category="out_of_range",
        field_name="image_count",
        field_reason="TOO_MANY_IMAGES",
    )


# --------------------------------------------------------------------------
# Model capability, kill switches, catalog
# --------------------------------------------------------------------------


def test_image_on_model_without_vision_is_vision_not_supported(mc, mock):
    chat_id = _new_chat(mc, model="gpt-text-only")
    image = _seed_att(mc, chat_id, kind="image")

    _reject(
        mc,
        mock,
        chat_id,
        {"content": "what is in this picture?", "attachment_ids": [image]},
        status=400,
        category="invalid_argument",
        field_name="content_type",
        field_reason="VISION_NOT_SUPPORTED",
    )


@pytest.mark.slow
def test_image_with_disable_images_kill_switch_is_feature_disabled(killswitch_server, mock):
    server = killswitch_server
    chat_id = _new_chat(server, model="gpt-4.1")
    image = _seed_att(server, chat_id, kind="image")

    _reject(
        server,
        mock,
        chat_id,
        {"content": "what is in this picture?", "attachment_ids": [image]},
        status=400,
        category="failed_precondition",
        violation_subject="images",
        violation_type="FEATURE_DISABLED",
    )


@pytest.mark.slow
def test_web_search_with_disable_web_search_kill_switch_is_feature_disabled(killswitch_server, mock):
    server = killswitch_server
    chat_id = _new_chat(server, model="gpt-4.1")

    _reject(
        server,
        mock,
        chat_id,
        {"content": "search the web for news", "web_search": {"enabled": True}},
        status=400,
        category="failed_precondition",
        violation_subject="web_search",
        violation_type="FEATURE_DISABLED",
    )


@pytest.mark.slow
def test_kill_switches_do_not_block_plain_messages(killswitch_server, mock):
    server = killswitch_server
    chat_id = _new_chat(server, model="gpt-4.1")
    mock.enqueue({"text": "plain answer"})

    result = server.stream(
        "POST",
        _stream_path(chat_id),
        "A1",
        json={"content": "no images, no web", "web_search": {"enabled": False}},
    )

    assert result.status_code == 200 and result.is_sse, (result.status_code, result.text[:500])
    assert result.terminal is not None and result.terminal[0] == "done", result.events
    tools = _stream_requests(mock)[-1]["json"].get("tools") or []
    assert not any(t.get("type", "").startswith("web_search") for t in tools if isinstance(t, dict)), tools


def test_chat_whose_model_is_not_in_catalog_is_invalid_model(mc, mock):
    ident = mc.identity("A1")
    chat_id = seed_chat(mc, ident.tenant_id, ident.user_id, model="gpt-e2e-removed-from-catalog")

    _reject(
        mc,
        mock,
        chat_id,
        {"content": "hello?"},
        status=400,
        category="invalid_argument",
        field_name="model",
        field_reason="INVALID_MODEL",
    )


@pytest.mark.slow
def test_chat_model_removed_from_catalog_after_restart_is_invalid_model(start_server, mock):
    first = start_server(name="catalog-full")
    chat_id = _new_chat(first, model="gpt-text-only")
    home_dir = str(first.home_dir)
    first.stop()

    pruned = [e for e in default_model_catalog() if e["id"] != "gpt-text-only"]
    second = start_server(
        {
            "server": {"home_dir": home_dir},
            "gears": {"static-mini-chat-model-policy-plugin": {"config": {"model_catalog": pruned}}},
        },
        name="catalog-pruned",
    )
    assert second.db_path == first.db_path
    resp = second.get(f"{CHATS}/{chat_id}", "A1")
    assert resp.status_code == 200 and resp.json()["model"] == "gpt-text-only", resp.text

    _reject(
        second,
        mock,
        chat_id,
        {"content": "is my model still there?"},
        status=400,
        category="invalid_argument",
        field_name="model",
        field_reason="INVALID_MODEL",
    )


# --------------------------------------------------------------------------
# Token limits
# --------------------------------------------------------------------------


@pytest.mark.slow
def test_message_above_max_input_tokens_is_input_too_long(budget_server, mock):
    server = budget_server
    chat_id = _new_chat(server, model=INPUT_LIMIT_MODEL)
    # ceil(400 / 4) = 100 tokens > max_input_tokens 50.
    long_message = "x" * 400

    _reject(
        server,
        mock,
        chat_id,
        {"content": long_message},
        status=400,
        category="out_of_range",
        field_reason="INPUT_TOO_LONG",
    )


@pytest.mark.slow
def test_message_within_max_input_tokens_is_streamed(budget_server, mock):
    server = budget_server
    chat_id = _new_chat(server, model=INPUT_LIMIT_MODEL)
    # ceil(40 / 4) = 10 tokens <= 50; mandatory context 4 + 10 <= token budget 50.
    _assert_stream_ok_control(server, chat_id, "x" * 40)


@pytest.mark.slow
def test_mandatory_context_above_budget_is_context_budget_exceeded(budget_server, mock):
    server = budget_server
    chat_id = _new_chat(server, model=SMALL_CONTEXT_MODEL)
    # token_budget = min(1000, 1000 - 900) - 0 surcharges - 0 overhead = 100.
    # System prompt 50 + message ceil(400 / 4) = 100 -> 150 > 100. The message
    # alone (100) is below max_input_tokens (1000), so it is not INPUT_TOO_LONG.
    _reject(
        server,
        mock,
        chat_id,
        {"content": "y" * 400},
        status=400,
        category="out_of_range",
        field_reason="CONTEXT_BUDGET_EXCEEDED",
    )


@pytest.mark.slow
def test_mandatory_context_within_budget_is_streamed(budget_server, mock):
    server = budget_server
    chat_id = _new_chat(server, model=SMALL_CONTEXT_MODEL)
    # System prompt 50 + message 10 = 60 <= 100.
    _assert_stream_ok_control(server, chat_id, "y" * 40)


@pytest.mark.slow
def test_max_output_tokens_not_below_context_window_is_context_budget_exceeded(budget_server, mock):
    server = budget_server
    chat_id = _new_chat(server, model=OVERFLOW_CONTEXT_MODEL)

    _reject(
        server,
        mock,
        chat_id,
        {"content": "hi"},
        status=400,
        category="out_of_range",
        field_reason="CONTEXT_BUDGET_EXCEEDED",
    )


# --------------------------------------------------------------------------
# Provider errors after the stream opened
# --------------------------------------------------------------------------


def _stream(server, chat_id: str, content: str, request_id: str, user: str = "A1") -> SseResult:
    return server.stream(
        "POST",
        _stream_path(chat_id),
        user,
        json={"content": content, "request_id": request_id},
        timeout=60.0,
    )


def _assert_error_stream(result: SseResult, request_id: str, code: str) -> dict[str, Any]:
    """SSE 200; ``stream_started`` first; one terminal ``error`` last; nothing after it."""
    assert result.status_code == 200, (result.status_code, result.text[:500])
    assert result.is_sse, result.headers
    assert result.transport_error is None, result.transport_error
    names = result.names()
    assert names and names[0] == "stream_started", names
    assert names[-1] == "error", f"terminal event must be error, got {names}"
    assert names.count("error") == 1 and "done" not in names, names
    assert names.count("stream_started") == 1, names
    assert set(names[1:-1]) <= {"ping", "delta", "tool"}, f"unexpected events on an error stream: {names}"
    started = result.events[0][1]
    assert started["request_id"] == request_id, started
    assert started["is_new_turn"] is True, started
    assert uuid.UUID(started["message_id"]), started
    error = result.events[-1][1]
    assert isinstance(error, dict), error
    assert {"code", "message"} <= set(error), error
    assert isinstance(error["message"], str), error
    assert error["code"] == code, f"expected SSE error code {code!r}, got {error}"
    return error


def _assert_no_provider_identifiers(texts: list[str], fragments: tuple[str, ...], mock) -> None:
    response_ids = [r["response_id"] for r in mock.requests() if r.get("response_id")]
    for text in texts:
        for fragment in (*fragments, *response_ids):
            assert fragment not in text, f"provider identifier {fragment!r} exposed to the client: {text}"


def _turn_row(server, chat_id: str, request_id: str) -> dict[str, Any]:
    rows = server.query(
        "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?",
        (db_uuid(chat_id), db_uuid(request_id)),
    )
    assert len(rows) == 1, rows
    return rows[0]


def _assert_failed_turn(server, chat_id: str, request_id: str, error_code: str) -> dict[str, Any]:
    row = _turn_row(server, chat_id, request_id)
    assert row["state"] == "failed", row
    assert row["error_code"] == error_code, row
    assert row["assistant_message_id"] is None, row
    assert row["completed_at"] is not None, row
    roles = [
        r["role"]
        for r in server.query(
            "SELECT role FROM messages WHERE chat_id = ? AND request_id = ?",
            (db_uuid(chat_id), db_uuid(request_id)),
        )
    ]
    assert roles == ["user"], f"a failed turn keeps the user message and has no assistant message: {roles}"

    resp = server.get(_turn_path(chat_id, request_id), "A1")
    assert resp.status_code == 200, resp.text
    status = resp.json()
    assert set(status) <= TURN_STATUS_FIELDS, status
    assert status["request_id"] == request_id, status
    assert status["state"] == "error", status
    assert status["error_code"] == error_code, status
    assert "assistant_message_id" not in status, status
    assert is_rfc3339(status["updated_at"]), status
    return status


def _sse_texts(result: SseResult) -> list[str]:
    return [json.dumps(data) if not isinstance(data, str) else data for _, data in result.events]


def test_provider_response_failed_is_provider_error_with_sanitized_message(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue({"kind": "failed", "error_code": "server_error", "error_message": RAW_PROVIDER_MESSAGE})

    result = _stream(mc, chat_id, "trigger a provider failure", request_id)

    error = _assert_error_stream(result, request_id, "provider_error")
    assert error["message"] == SANITIZED_PROVIDER_MESSAGE, error
    _assert_no_provider_identifiers(_sse_texts(result), RAW_IDENTIFIERS, mock)
    assert len(_stream_requests(mock)) == 1
    status = _assert_failed_turn(mc, chat_id, request_id, "provider_error")
    _assert_no_provider_identifiers([json.dumps(status)], RAW_IDENTIFIERS, mock)


def test_provider_response_failed_default_message_exposes_no_identifiers(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue({"kind": "failed", "text": "partial answer before failure"})

    result = _stream(mc, chat_id, "trigger a provider failure", request_id)

    error = _assert_error_stream(result, request_id, "provider_error")
    for placeholder in ("[provider_id]", "[url]"):
        assert placeholder in error["message"], error
    _assert_no_provider_identifiers(_sse_texts(result), DEFAULT_MESSAGE_IDENTIFIERS, mock)
    _assert_failed_turn(mc, chat_id, request_id, "provider_error")


def test_provider_sse_error_event_is_provider_error(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue({"kind": "sse_error", "error_code": "server_error", "error_message": RAW_PROVIDER_MESSAGE})

    result = _stream(mc, chat_id, "trigger an sse error", request_id)

    error = _assert_error_stream(result, request_id, "provider_error")
    assert error["message"] == SANITIZED_PROVIDER_MESSAGE, error
    _assert_no_provider_identifiers(_sse_texts(result), RAW_IDENTIFIERS, mock)
    _assert_failed_turn(mc, chat_id, request_id, "provider_error")


def test_provider_http_500_is_provider_error(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue({"kind": "http_error", "status": 500, "error_message": RAW_PROVIDER_MESSAGE})

    result = _stream(mc, chat_id, "trigger an http 500", request_id)

    error = _assert_error_stream(result, request_id, "provider_error")
    assert error["message"] == SANITIZED_PROVIDER_MESSAGE, error
    _assert_no_provider_identifiers(_sse_texts(result), RAW_IDENTIFIERS, mock)
    _assert_failed_turn(mc, chat_id, request_id, "provider_error")


def test_provider_http_429_is_rate_limited_with_retry_after(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue(
        {
            "kind": "http_error",
            "status": 429,
            "retry_after": 7,
            "error_code": "rate_limit_exceeded",
            "error_message": RAW_PROVIDER_MESSAGE,
        }
    )

    result = _stream(mc, chat_id, "trigger a provider 429", request_id)

    error = _assert_error_stream(result, request_id, "rate_limited")
    assert "7" in error["message"], f"rate_limited message must include the Retry-After delay: {error}"
    assert "retry_after" not in error, f"SSE error has no separate retry field: {error}"
    _assert_no_provider_identifiers(_sse_texts(result), RAW_IDENTIFIERS, mock)
    _assert_failed_turn(mc, chat_id, request_id, "rate_limited")


def test_provider_stream_dropped_without_terminal_is_provider_error(mc, mock):
    chat_id = _new_chat(mc)
    request_id = new_uuid()
    mock.enqueue({"kind": "drop", "chunks": ["one ", "two ", "three ", "four "]})

    result = _stream(mc, chat_id, "trigger a dropped provider stream", request_id)

    # A provider disconnect is a provider failure (Provider Event Translation:
    # "Provider HTTP error / disconnect"), not ``stream_interrupted`` (which the
    # relay synthesizes only when the provider task itself ends without a
    # terminal event) and never ``done`` (content durability invariant).
    _assert_error_stream(result, request_id, "provider_error")
    _assert_no_provider_identifiers(_sse_texts(result), (), mock)
    _assert_failed_turn(mc, chat_id, request_id, "provider_error")


@pytest.mark.slow
def test_gateway_timeout_is_provider_timeout(timeout_server, mock):
    server = timeout_server
    chat_id = _new_chat(server)
    request_id = new_uuid()
    # The response headers arrive after the OAGW proxy timeout (2 s).
    mock.enqueue({"initial_delay_ms": 6_000, "text": "too late"})

    result = _stream(server, chat_id, "trigger a gateway timeout", request_id)

    _assert_error_stream(result, request_id, "provider_timeout")
    _assert_failed_turn(server, chat_id, request_id, "provider_timeout")


def test_chat_accepts_a_new_turn_after_a_provider_error(mc, mock):
    chat_id = _new_chat(mc)
    failed_request = new_uuid()
    mock.enqueue({"kind": "failed"}, {"text": "recovered"})

    _assert_error_stream(_stream(mc, chat_id, "first", failed_request), failed_request, "provider_error")
    result = _stream(mc, chat_id, "second", new_uuid())

    assert result.status_code == 200 and result.terminal is not None, result.events
    assert result.terminal[0] == "done", result.events
    assert result.text_content() == "recovered"
