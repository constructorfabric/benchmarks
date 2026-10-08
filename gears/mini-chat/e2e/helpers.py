"""Shared helpers for the mini-chat black-box tests (assertions, mock scripts, files, DB access)."""

from __future__ import annotations

import datetime as dt
import json
import re
import struct
import threading
import time
import uuid
import zlib
from typing import Any, Iterable

import httpx

from harness import TOKENS, MiniChat, SseEvent, ub, wait_until

RT_CHAT = "gts.cf.core.mini_chat.chat.v1~"
RT_MESSAGE = "gts.cf.core.mini_chat.message.v1~"
RT_TURN = "gts.cf.core.mini_chat.turn.v1~"
RT_ATTACHMENT = "gts.cf.core.mini_chat.attachment.v1~"
RT_MODEL = "gts.cf.core.mini_chat.model.v1~"
RT_ODATA = "gts.cf.core.odata.query.v1~"

SYSTEM_PROMPT = "You are a helpful assistant."
MIME_XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"

PROVIDER_ID_RE = re.compile(r"(resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]{6,}|(file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}")


# ── identities ─────────────────────────────────────────────────────────────
def user_id(user: str) -> str:
    return TOKENS[user][1]


def tenant_id(user: str) -> str:
    return TOKENS[user][2]


def as_uuid(v: Any) -> str | None:
    if v is None:
        return None
    if isinstance(v, (bytes, bytearray, memoryview)):
        b = bytes(v)
        if len(b) == 16:
            return str(uuid.UUID(bytes=b))
        return str(uuid.UUID(b.decode()))
    return str(uuid.UUID(str(v)))


# ── canonical Problem assertions ───────────────────────────────────────────
def problem(r: httpx.Response, status: int) -> dict:
    assert r.status_code == status, f"expected {status}, got {r.status_code}: {r.text[:2000]}"
    assert "text/event-stream" not in r.headers.get("content-type", ""), "error must not open an SSE stream"
    j = r.json()
    assert isinstance(j, dict), j
    assert "code" not in j, f"canonical Problem must not have a top-level code: {j}"
    for k in ("type", "title", "status", "detail"):
        assert k in j, f"Problem field {k} missing: {j}"
    assert j["status"] == status, j
    return j


def ctx(j: dict) -> dict:
    return j.get("context") or {}


def field_reasons(j: dict) -> list[str]:
    return [fv.get("reason") for fv in ctx(j).get("field_violations") or []]


def field_violations(j: dict) -> list[dict]:
    return list(ctx(j).get("field_violations") or [])


def violations(j: dict) -> list[dict]:
    return list(ctx(j).get("violations") or [])


def assert_problem(
    r: httpx.Response,
    status: int,
    *,
    reason: str | None = None,
    field_reason: str | None = None,
    field: str | None = None,
    subject: str | None = None,
    vtype: str | None = None,
    description: str | None = None,
    resource_type: str | None = None,
    resource_name: str | None = None,
    category: str | None = None,
) -> dict:
    j = problem(r, status)
    c = ctx(j)
    if category is not None:
        assert f"cf.core.err.{category}.v1~" in j["type"], f"category {category} expected: {j['type']}"
    if reason is not None:
        assert c.get("reason") == reason, f"context.reason != {reason}: {j}"
    if field_reason is not None:
        fvs = field_violations(j)
        assert any(fv.get("reason") == field_reason for fv in fvs), f"field_violations lack {field_reason}: {j}"
        if field is not None:
            assert any(fv.get("reason") == field_reason and fv.get("field") == field for fv in fvs), (
                f"field_violation {field_reason} not on field {field}: {j}"
            )
    if subject is not None or vtype is not None or description is not None:
        vs = violations(j)
        assert vs, f"no context.violations: {j}"

        def ok(v: dict) -> bool:
            return (
                (subject is None or v.get("subject") == subject)
                and (vtype is None or v.get("type") == vtype)
                and (description is None or v.get("description") == description)
            )

        assert any(ok(v) for v in vs), f"no violation matching subject={subject} type={vtype} description={description}: {j}"
    if resource_type is not None:
        assert c.get("resource_type") == resource_type, f"resource_type != {resource_type}: {j}"
    if resource_name is not None:
        assert c.get("resource_name") == resource_name or resource_name in json.dumps(c), f"resource_name {resource_name}: {j}"
    return j


def assert_not_found(r: httpx.Response, resource_type: str) -> dict:
    return assert_problem(r, 404, resource_type=resource_type)


def assert_not_found_any(r: httpx.Response, *resource_types: str) -> dict:
    """404 whose resource_type is one of ``resource_types`` (e.g. chat or turn for a turn in a foreign chat)."""
    j = assert_problem(r, 404)
    assert ctx(j).get("resource_type") in resource_types, f"resource_type not in {resource_types}: {j}"
    return j


# ── SSE helpers ────────────────────────────────────────────────────────────
def names(events: list[SseEvent]) -> list[str]:
    return [e.event for e in events]


def non_ping(events: list[SseEvent]) -> list[SseEvent]:
    return [e for e in events if e.event != "ping"]


def first(events: list[SseEvent], name: str) -> SseEvent:
    for e in events:
        if e.event == name:
            return e
    raise AssertionError(f"no {name} event in {names(events)}")


def all_of(events: list[SseEvent], name: str) -> list[SseEvent]:
    return [e for e in events if e.event == name]


def text_of(events: list[SseEvent]) -> str:
    return "".join(e.data.get("content", "") for e in events if e.event == "delta" and e.data.get("type", "text") == "text")


def assert_sse_order(events: list[SseEvent]) -> None:
    """stream_started ping* (delta | tool)* citations? (done | error), nothing after the terminal event."""
    assert events, "empty SSE stream"
    assert events[0].event == "stream_started", names(events)
    assert sum(1 for e in events if e.event == "stream_started") == 1, names(events)
    terminals = [i for i, e in enumerate(events) if e.event in ("done", "error")]
    assert len(terminals) == 1, f"exactly one terminal event expected: {names(events)}"
    assert terminals[0] == len(events) - 1, f"events after terminal: {names(events)}"
    seen_content = False
    citations_at = None
    for i, e in enumerate(events[1:-1], start=1):
        assert e.event in ("ping", "delta", "tool", "citations"), f"unexpected event {e.event}: {names(events)}"
        if e.event in ("delta", "tool"):
            assert citations_at is None, f"delta/tool after citations: {names(events)}"
            seen_content = True
        elif e.event == "ping":
            assert not seen_content, f"ping after first delta/tool: {names(events)}"
            assert e.data in ({}, None), e.data
        elif e.event == "citations":
            assert citations_at is None, f"more than one citations event: {names(events)}"
            citations_at = i


def assert_ok_stream(r: httpx.Response, events: list[SseEvent]) -> SseEvent:
    assert r.status_code == 200, r.text[:2000]
    assert r.headers.get("content-type", "").startswith("text/event-stream"), r.headers
    assert_sse_order(events)
    assert events[-1].event == "done", f"expected done: {events[-1]}"
    return events[-1]


def assert_error_stream(r: httpx.Response, events: list[SseEvent], code: str) -> dict:
    assert r.status_code == 200, r.text[:2000]
    assert_sse_order(events)
    last = events[-1]
    assert last.event == "error", f"expected terminal error, got {names(events)} / {last.data}"
    assert set(last.data.keys()) >= {"code", "message"}, last.data
    assert last.data["code"] == code, last.data
    return last.data


def no_provider_ids(obj: Any) -> None:
    text = obj if isinstance(obj, str) else json.dumps(obj)
    m = PROVIDER_ID_RE.search(text)
    assert m is None, f"provider identifier leaked: {m.group(0)!r} in {text[:500]}"


# ── mock scripts ───────────────────────────────────────────────────────────
def ev(name: str, **data: Any) -> dict:
    return {"event": name, "data": {"type": name, **data}}


def sleep(ms: int) -> dict:
    return {"sleep_ms": ms}


def ws_searching(item_id: str = "ws_1") -> dict:
    return ev("response.web_search_call.searching", item_id=item_id, output_index=0)


def ws_completed(item_id: str = "ws_1") -> dict:
    return ev("response.web_search_call.completed", item_id=item_id, output_index=0)


def fs_searching(item_id: str = "fs_1") -> dict:
    return ev("response.file_search_call.searching", item_id=item_id, output_index=0)


def fs_completed(item_id: str = "fs_1", results: list | None = None) -> dict:
    d: dict[str, Any] = {"item_id": item_id, "output_index": 0}
    if results is not None:
        d["results"] = results
    return ev("response.file_search_call.completed", **d)


def ci_in_progress(item_id: str = "ci_1") -> dict:
    return ev("response.code_interpreter_call.in_progress", item_id=item_id, output_index=0)


def ci_done(logs: list[str], item_id: str = "ci_1") -> dict:
    return ev(
        "response.output_item.done",
        output_index=0,
        item={
            "id": item_id,
            "type": "code_interpreter_call",
            "status": "completed",
            "code": "print(42)",
            "outputs": [{"type": "logs", "logs": l} for l in logs],
        },
    )


def stream_script(
    *items: Any,
    usage: dict | None = None,
    resp_id: str | None = None,
    annotations: list | None = None,
    output_extra: list | None = None,
    terminal: str | None = "completed",
    error: dict | None = None,
    hang_ms: int | None = None,
    match: dict | None = None,
) -> dict:
    """Builds a streaming ``/responses`` script.

    ``items``: strings become ``response.output_text.delta`` events; dicts are passed through
    (provider events built with ``ev``/``ws_searching``/..., ``sleep(ms)`` pauses or ``{"raw": ...}``).
    ``terminal``: ``completed`` | ``incomplete`` | ``failed`` | ``error`` (SSE ``error`` event) | ``None``.
    """
    resp_id = resp_id or f"resp_{uuid.uuid4().hex[:24]}"
    usage = usage if usage is not None else {"input_tokens": 100, "output_tokens": 50}
    events: list[dict] = [ev("response.created", response={"id": resp_id, "status": "in_progress"})]
    text = ""
    for it in items:
        if isinstance(it, str):
            text += it
            events.append(ev("response.output_text.delta", delta=it, item_id="msg_item", output_index=0, content_index=0))
        else:
            events.append(it)
    output = list(output_extra or [])
    output.append(
        {
            "type": "message",
            "id": "msg_item",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": annotations or []}],
        }
    )
    if terminal == "completed":
        events.append(ev("response.completed", response={"id": resp_id, "status": "completed", "output": output, "usage": usage}))
    elif terminal == "incomplete":
        events.append(
            ev(
                "response.incomplete",
                response={
                    "id": resp_id,
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "output": output,
                    "usage": usage,
                },
            )
        )
    elif terminal == "failed":
        err = error or {"code": "server_error", "message": "mock failure"}
        resp: dict[str, Any] = {"id": resp_id, "status": "failed", "error": err}
        if usage:
            resp["usage"] = usage
        events.append(ev("response.failed", response=resp))
    elif terminal == "error":
        err = error or {"code": "server_error", "message": "mock failure"}
        events.append({"event": "error", "data": {"type": "error", **err}})
    script: dict[str, Any] = {"kind": "stream", "events": events, "match": match if match is not None else {"stream": True}}
    if hang_ms:
        script["hang_ms"] = hang_ms
    return script


def http_error(status: int = 500, message: str = "mock error", headers: dict | None = None, match: dict | None = None) -> dict:
    return {
        "kind": "http_error",
        "status": status,
        "body": {"error": {"message": message, "type": "server_error", "code": None}},
        "headers": headers or {},
        "match": match if match is not None else {"stream": True},
    }


def function_call_item(call_id: str, name: str = "search_knowledge", arguments: str = '{"query": "policy"}') -> dict:
    return {
        "type": "function_call",
        "id": f"fc_{call_id}",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed",
    }


def function_call_script(
    call_id: str = "call_1",
    name: str = "search_knowledge",
    arguments: str = '{"query": "policy"}',
    usage: dict | None = None,
    text: str = "",
    match: dict | None = None,
) -> dict:
    """A streaming ``/responses`` script whose response ends with a function call (Responses API events)."""
    resp_id = f"resp_{uuid.uuid4().hex[:24]}"
    item = function_call_item(call_id, name, arguments)
    events: list[dict] = [ev("response.created", response={"id": resp_id, "status": "in_progress"})]
    output: list[dict] = []
    if text:
        events.append(ev("response.output_text.delta", delta=text, item_id="msg_pre", output_index=0, content_index=0))
        output.append({"type": "message", "id": "msg_pre", "role": "assistant", "status": "completed",
                       "content": [{"type": "output_text", "text": text, "annotations": []}]})
    idx = len(output)
    events += [
        ev("response.output_item.added", output_index=idx, item={**item, "arguments": "", "status": "in_progress"}),
        ev("response.function_call_arguments.delta", item_id=item["id"], output_index=idx, delta=arguments),
        ev("response.function_call_arguments.done", item_id=item["id"], output_index=idx, arguments=arguments),
        ev("response.output_item.done", output_index=idx, item=item),
    ]
    output.append(item)
    events.append(
        ev(
            "response.completed",
            response={"id": resp_id, "status": "completed", "output": output,
                      "usage": usage if usage is not None else {"input_tokens": 20, "output_tokens": 5}},
        )
    )
    return {"kind": "stream", "events": events, "match": match if match is not None else {"stream": True}}


# ── provider request inspection ────────────────────────────────────────────
def item_text(item: dict) -> str:
    c = item.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def input_items(body: dict) -> list[dict]:
    inp = body.get("input")
    if isinstance(inp, str):
        return [{"role": "user", "content": inp}]
    return [i for i in (inp or []) if isinstance(i, dict)]


def input_pairs(body: dict) -> list[tuple[str, str]]:
    return [(i.get("role", ""), item_text(i)) for i in input_items(body)]


def tool_types(body: dict) -> list[str]:
    return [t.get("type") for t in body.get("tools") or []]


def tool(body: dict, kind: str) -> dict | None:
    for t in body.get("tools") or []:
        if t.get("type") == kind or (kind == "web_search" and str(t.get("type", "")).startswith("web_search")):
            return t
    return None


def input_images(body: dict) -> list[dict]:
    out = []
    for i in input_items(body):
        c = i.get("content")
        if isinstance(c, list):
            out.extend(p for p in c if isinstance(p, dict) and p.get("type") == "input_image")
    return out


# ── files ──────────────────────────────────────────────────────────────────
def make_png(w: int = 64, h: int = 48, color: tuple[int, int, int] = (200, 30, 30)) -> bytes:
    raw = b"".join(b"\x00" + bytes(color) * w for _ in range(h))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def make_pdf(text: str = "hello") -> bytes:
    return (
        b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[]/Count 0>>endobj\n"
        + f"% {text}\n".encode()
        + b"trailer<</Root 1 0 R>>\n%%EOF\n"
    )


def make_xlsx() -> bytes:
    # Not a real workbook; mini-chat does not parse documents (the provider does).
    return b"PK\x03\x04" + b"\x00" * 26 + b"xl/workbook.xml" + b"\x00" * 64


def upload(srv: MiniChat, chat_id: str, filename: str, data: bytes, content_type: str, user: str = "a1") -> httpx.Response:
    return httpx.post(
        f"{srv.base}/chats/{chat_id}/attachments",
        headers=srv.headers(user),
        files={"file": (filename, data, content_type)},
        timeout=90,
    )


def upload_ok(srv: MiniChat, chat_id: str, filename: str = "doc.pdf", data: bytes | None = None, content_type: str = "application/pdf", user: str = "a1") -> dict:
    r = upload(srv, chat_id, filename, data if data is not None else make_pdf(), content_type, user)
    assert r.status_code == 201, r.text
    return r.json()


def raw_multipart(srv: MiniChat, chat_id: str, body: bytes, content_type: str, user: str = "a1") -> httpx.Response:
    return httpx.post(
        f"{srv.base}/chats/{chat_id}/attachments",
        headers={**srv.headers(user), "Content-Type": content_type},
        content=body,
        timeout=60,
    )


# ── chats / turns convenience ──────────────────────────────────────────────
def new_chat(srv: MiniChat, model: str | None = None, user: str = "a1", title: str | None = None) -> str:
    body: dict[str, Any] = {}
    if model:
        body["model"] = model
    if title:
        body["title"] = title
    return srv.create_chat(user, **body)["id"]


def send_ok(srv: MiniChat, chat_id: str, content: str = "Hello", user: str = "a1", **body: Any) -> tuple[dict, dict, list[SseEvent]]:
    """Sends a message, asserts a well-formed completed stream; returns (stream_started, done, events)."""
    r, events = srv.stream(chat_id, content, user, **body)
    done = assert_ok_stream(r, events)
    return events[0].data, done.data, events


def turn_status(srv: MiniChat, chat_id: str, request_id: str, user: str = "a1") -> httpx.Response:
    return srv.req("GET", f"/chats/{chat_id}/turns/{request_id}", user)


def wait_turn_state(srv: MiniChat, chat_id: str, request_id: str, states: Iterable[str], timeout: float = 20) -> dict:
    states = set(states)

    def check():
        r = turn_status(srv, chat_id, request_id)
        if r.status_code == 200 and r.json().get("state") in states:
            return r.json()
        return None

    res = wait_until(check, timeout=timeout, interval=0.2)
    assert res, f"turn {request_id} did not reach {states}: {turn_status(srv, chat_id, request_id).text}"
    return res


def list_messages(srv: MiniChat, chat_id: str, user: str = "a1", **params: Any) -> list[dict]:
    r = srv.req("GET", f"/chats/{chat_id}/messages", user, params={"limit": 100, **params})
    assert r.status_code == 200, r.text
    return r.json()["items"]


def get_chat(srv: MiniChat, chat_id: str, user: str = "a1") -> dict:
    r = srv.req("GET", f"/chats/{chat_id}", user)
    assert r.status_code == 200, r.text
    return r.json()


# ── DB access ──────────────────────────────────────────────────────────────
def rows(srv: MiniChat, sql: str, params: tuple = ()) -> list[dict]:
    return [dict(r) for r in srv.query(sql, params)]


def turn_rows(srv: MiniChat, chat_id: str) -> list[dict]:
    return rows(srv, "SELECT * FROM chat_turns WHERE chat_id = ? ORDER BY started_at, id", (ub(chat_id),))


def turn_row(srv: MiniChat, chat_id: str, request_id: str) -> dict | None:
    rs = rows(srv, "SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", (ub(chat_id), ub(request_id)))
    return rs[0] if rs else None


def running_turn(srv: MiniChat, chat_id: str) -> dict | None:
    rs = rows(srv, "SELECT * FROM chat_turns WHERE chat_id = ? AND state = 'running' AND deleted_at IS NULL", (ub(chat_id),))
    return rs[0] if rs else None


def wait_running(srv: MiniChat, chat_id: str, timeout: float = 10) -> dict:
    res = wait_until(lambda: running_turn(srv, chat_id), timeout=timeout, interval=0.05)
    assert res, "no running turn appeared"
    return res


def message_rows(srv: MiniChat, chat_id: str) -> list[dict]:
    return rows(srv, "SELECT * FROM messages WHERE chat_id = ? ORDER BY created_at, id", (ub(chat_id),))


def attachment_row(srv: MiniChat, attachment_id: str) -> dict:
    rs = rows(srv, "SELECT * FROM attachments WHERE id = ?", (ub(attachment_id),))
    assert rs, f"attachment {attachment_id} not in DB"
    return rs[0]


def chat_row(srv: MiniChat, chat_id: str) -> dict:
    rs = rows(srv, "SELECT * FROM chats WHERE id = ?", (ub(chat_id),))
    assert rs, f"chat {chat_id} not in DB"
    return rs[0]


def vector_store_row(srv: MiniChat, chat_id: str) -> dict | None:
    rs = rows(srv, "SELECT * FROM chat_vector_stores WHERE chat_id = ?", (ub(chat_id),))
    return rs[0] if rs else None


# ── timestamps ─────────────────────────────────────────────────────────────
_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2})([T ])(\d{2}:\d{2}:\d{2})(\.\d+)?(.*)$")


def parse_ts(text: str) -> dt.datetime:
    m = _TS_RE.match(text.strip())
    assert m, f"unrecognized timestamp format: {text!r}"
    base = dt.datetime.strptime(f"{m.group(1)} {m.group(3)}", "%Y-%m-%d %H:%M:%S")
    frac = m.group(4) or ""
    if frac:
        base += dt.timedelta(microseconds=int((frac[1:] + "000000")[:6]))
    return base


def format_like(sample: str, when: dt.datetime) -> str:
    """Formats ``when`` (naive, same offset as sample) in the textual format of ``sample``."""
    m = _TS_RE.match(sample.strip())
    assert m, f"unrecognized timestamp format: {sample!r}"
    sep, frac, rest = m.group(2), m.group(4) or "", m.group(5)
    out = when.strftime("%Y-%m-%d") + sep + when.strftime("%H:%M:%S")
    if frac:
        digits = len(frac) - 1
        out += "." + (f"{when.microsecond:06d}" + "0" * 9)[:digits]
    return out + rest


def shift_ts(text: str, seconds: float) -> str:
    return format_like(text, parse_ts(text) + dt.timedelta(seconds=seconds))


def utc_today() -> dt.date:
    return dt.datetime.now(dt.timezone.utc).date()


# ── quota ──────────────────────────────────────────────────────────────────
def quota_rows(srv: MiniChat, user: str) -> list[dict]:
    return rows(
        srv,
        "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? ORDER BY period_type, bucket, period_start",
        (ub(tenant_id(user)), ub(user_id(user))),
    )


def quota_row(srv: MiniChat, user: str, period_type: str, bucket: str) -> dict | None:
    """Current-period row (latest period_start) of a bucket."""
    rs = rows(
        srv,
        "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? AND period_type = ? AND bucket = ? "
        "ORDER BY period_start DESC LIMIT 1",
        (ub(tenant_id(user)), ub(user_id(user)), period_type, bucket),
    )
    return rs[0] if rs else None


def quota_val(srv: MiniChat, user: str, period_type: str, bucket: str, col: str) -> int:
    r = quota_row(srv, user, period_type, bucket)
    return int(r[col]) if r else 0


def quota_snapshot(srv: MiniChat, user: str) -> dict[tuple[str, str], dict]:
    return {(r["period_type"], r["bucket"]): r for r in quota_rows(srv, user) if _is_current(r)}


def _is_current(r: dict) -> bool:
    ps = str(r["period_start"])[:10]
    today = utc_today()
    if r["period_type"] == "daily":
        return ps == today.isoformat()
    return ps == today.replace(day=1).isoformat()


def ensure_quota_rows(srv: MiniChat, user: str) -> None:
    """Makes the server create the user's current quota rows (all four buckets) with one premium turn."""
    snap = quota_snapshot(srv, user)
    needed = {("daily", "total"), ("monthly", "total"), ("daily", "tier:premium"), ("monthly", "tier:premium")}
    if needed <= set(snap):
        return
    chat = new_chat(srv, "gpt-4.1", user)
    send_ok(srv, chat, "warm up quota rows", user)
    snap = quota_snapshot(srv, user)
    assert needed <= set(snap), f"quota rows were not created by a premium turn: {list(snap)}"


def set_quota(srv: MiniChat, user: str, period_type: str, bucket: str, **cols: Any) -> None:
    row = quota_row(srv, user, period_type, bucket)
    assert row is not None, f"no quota row {period_type}/{bucket} for {user}"
    sets = ", ".join(f"{k} = ?" for k in cols)
    srv.execute(f"UPDATE quota_usage SET {sets} WHERE id = ?", (*cols.values(), row["id"]))


class QuotaRestorer:
    """Remembers current quota rows of a user and restores the enforcement/telemetry counters."""

    COLS = ("spent_credits_micro", "reserved_credits_micro", "web_search_calls", "code_interpreter_calls")

    def __init__(self, srv: MiniChat, user: str) -> None:
        self.srv = srv
        self.user = user
        self.saved = {k: {c: v[c] for c in self.COLS} | {"id": v["id"]} for k, v in quota_snapshot(srv, user).items()}

    def restore(self) -> None:
        for (pt, b), vals in self.saved.items():
            cols = {c: vals[c] for c in self.COLS}
            sets = ", ".join(f"{k} = ?" for k in cols)
            self.srv.execute(f"UPDATE quota_usage SET {sets} WHERE id = ?", (*cols.values(), vals["id"]))


def credits_micro(inp: int, out: int, in_mult: int, out_mult: int) -> int:
    return -(-inp * in_mult // 1_000_000) + -(-out * out_mult // 1_000_000)


def estimate_text_tokens(content: str, bytes_per_token: int = 4, overhead: int = 100, margin_pct: int = 10) -> int:
    base = -(-len(content.encode()) // bytes_per_token) + overhead
    return -(-base * (100 + margin_pct) // 100)


# ── outbox ─────────────────────────────────────────────────────────────────
def usage_events(srv: MiniChat) -> list[dict]:
    return [e["payload"] for e in srv.outbox_events() if isinstance(e["payload"], dict) and "billing_outcome" in e["payload"] and "dedupe_key" in e["payload"]]


def usage_events_for(srv: MiniChat, request_id: str) -> list[dict]:
    return [p for p in usage_events(srv) if str(p.get("request_id")) == request_id]


def wait_usage_event(srv: MiniChat, request_id: str, timeout: float = 10) -> dict:
    res = wait_until(lambda: usage_events_for(srv, request_id), timeout=timeout)
    assert res, f"no usage outbox event for request {request_id}"
    return res[0]


def audit_events(srv: MiniChat) -> list[dict]:
    """Captured audit outbox payloads (queue ``mini-chat.audit``; falls back to payload inspection)."""
    kinds = ("turn_completed", "turn_failed", "turn_retry", "turn_edit", "turn_delete")
    out = []
    for e in srv.outbox_events():
        p = e["payload"]
        if not isinstance(p, dict) or "billing_outcome" in p:
            continue
        if "audit" in e["queue"] or (not e["queue"] and any(k in json.dumps(p) for k in kinds)):
            out.append(p)
    return out


def audit_event_types(p: Any) -> set[str]:
    found: set[str] = set()

    def walk(x: Any) -> None:
        if isinstance(x, dict):
            for k, v in x.items():
                if k == "event_type" and isinstance(v, str):
                    found.add(v)
                walk(v)
        elif isinstance(x, list):
            for v in x:
                walk(v)
        elif isinstance(x, str) and x in ("turn_completed", "turn_failed", "turn_retry", "turn_edit", "turn_delete"):
            found.add(x)

    walk(p)
    return found


def outbox_mentions(srv: MiniChat, needle: str, queue_contains: str | None = None) -> list[dict]:
    return [e for e in srv.outbox_events(queue_contains) if needle in json.dumps(e["payload"])]


# ── background streaming ───────────────────────────────────────────────────
class BgStream(threading.Thread):
    """Runs an SSE request in a background thread and records its events."""

    def __init__(self, srv: MiniChat, method: str, path: str, user: str = "a1", json_body: Any = None) -> None:
        super().__init__(daemon=True)
        self.srv, self.method, self.path, self.user, self.json_body = srv, method, path, user, json_body
        self.response: httpx.Response | None = None
        self.events: list[SseEvent] = []
        self.error: BaseException | None = None
        self.started_evt = threading.Event()
        self.start()

    def run(self) -> None:
        try:
            for _, item in self.srv.sse_iter(self.method, self.path, self.user, self.json_body):
                if isinstance(item, httpx.Response):
                    self.response = item
                else:
                    self.events.append(item)
                    if item.event == "stream_started":
                        self.started_evt.set()
        except BaseException as e:  # noqa: BLE001
            self.error = e
        finally:
            self.started_evt.set()

    def wait(self, timeout: float = 30) -> "BgStream":
        self.join(timeout)
        assert not self.is_alive(), "background stream did not finish"
        return self


def bg_send(srv: MiniChat, chat_id: str, content: str = "Hello", user: str = "a1", **body: Any) -> BgStream:
    return BgStream(srv, "POST", f"/chats/{chat_id}/messages:stream", user, {"content": content, **body})


def now_s() -> float:
    return time.monotonic()


def api_ts(s: str) -> dt.datetime:
    """Parses an RFC 3339 timestamp from an API response (naive UTC, microsecond precision)."""
    s = s.strip().replace("Z", "+00:00")
    m = re.match(r"^(.*?)(\.\d+)?([+-]\d{2}:\d{2})$", s)
    assert m, f"not RFC 3339: {s!r}"
    frac = (m.group(2) or "")[:7]
    d = dt.datetime.fromisoformat(m.group(1) + frac + m.group(3))
    return d.astimezone(dt.timezone.utc).replace(tzinfo=None)
