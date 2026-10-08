"""Shared helpers for the mini-chat black-box E2E suite.

Everything here talks to the running server over HTTP (httpx), to the gear's
SQLite database (sqlite3) or to the in-process mock provider. No Rust code is
touched.
"""

from __future__ import annotations

import datetime as dt
import io
import json
import re
import socket
import sqlite3
import struct
import threading
import time
import uuid
import zipfile
import zlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

import httpx

# ---------------------------------------------------------------------------
# Identities (must match the static-authn tokens written by conftest.py)
# ---------------------------------------------------------------------------

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
TENANT_B = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"

USERS: dict = {
    "tok-a": ("11111111-6a88-4768-9dfc-6bcd5187d9ed", TENANT_A),
    "tok-a2": ("44444444-6a88-4768-9dfc-6bcd5187d9ed", TENANT_A),
    "tok-b": ("22222222-6a88-4768-9dfc-6bcd5187d9ed", TENANT_B),
}
for _i in range(1, 13):
    USERS[f"tok-q{_i}"] = (f"5555{_i:04d}-6a88-4768-9dfc-6bcd5187d9ed", TENANT_A)
for _i in range(1, 5):
    USERS[f"tok-l{_i}"] = (f"6666{_i:04d}-6a88-4768-9dfc-6bcd5187d9ed", TENANT_A)
for _i in range(1, 5):
    USERS[f"tok-k{_i}"] = (f"7777{_i:04d}-6a88-4768-9dfc-6bcd5187d9ed", TENANT_A)


def user_id(token: str) -> str:
    return USERS[token][0]


def tenant_id(token: str) -> str:
    return USERS[token][1]


# ---------------------------------------------------------------------------
# Catalog constants (must match conftest.py model_catalog)
# ---------------------------------------------------------------------------

DEFAULT_MODEL = "prem"
MODEL_MULTIPLIERS = {
    "prem": (3_000_000, 15_000_000),
    "prem-novision": (3_000_000, 15_000_000),
    "std": (1_000_000, 3_000_000),
    "std-novision": (1_000_000, 3_000_000),
    "std-tiny": (1_000_000, 3_000_000),
    "std-budget": (1_000_000, 3_000_000),
}
MODEL_MAX_OUTPUT = {"prem": 4096, "prem-novision": 4096, "std": 4096, "std-novision": 4096, "std-tiny": 1024, "std-budget": 3000}
PREMIUM_MODELS = {"prem", "prem-novision"}
STANDARD_MODELS = {"std", "std-novision", "std-tiny", "std-budget"}
ENABLED_MODELS = PREMIUM_MODELS | STANDARD_MODELS
DISABLED_MODEL = "off-model"
SYSTEM_PROMPT_MARKER = "E2E-SYSPROMPT"
STD_LIMIT_DAILY = 100_000_000
STD_LIMIT_MONTHLY = 1_000_000_000
PREM_LIMIT_DAILY = 50_000_000
PREM_LIMIT_MONTHLY = 500_000_000
WEB_SEARCH_DAILY_QUOTA = 75
SUMMARY_PREAMBLE = "This conversation has earlier messages that have been summarized"

# Provider details the mock embeds in error messages; none may reach a client.
FAKE_IDS_AND_SECRETS = [
    "file-abcdefghijklmnopqrstu",
    "resp_ABCDEF1234567890abcd",
    "sk-abcdefghijklmnop123456",
    "status.example.com",
    "vs_abcdefghijklmnop1234",
    "internal.example.net",
]

RT_CHAT = "gts.cf.core.mini_chat.chat.v1~"
RT_MESSAGE = "gts.cf.core.mini_chat.message.v1~"
RT_TURN = "gts.cf.core.mini_chat.turn.v1~"
RT_ATTACHMENT = "gts.cf.core.mini_chat.attachment.v1~"
RT_MODEL = "gts.cf.core.mini_chat.model.v1~"
RT_ODATA = "gts.cf.core.odata.query.v1~"

MIME_PDF = "application/pdf"
MIME_XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
MIME_PNG = "image/png"


def ceil_div(n: int, d: int) -> int:
    return (n + d - 1) // d


def credits_micro(in_tokens: int, out_tokens: int, model: str) -> int:
    """DESIGN §5.3 canonical formula, per-component ceil."""
    im, om = MODEL_MULTIPLIERS[model]
    return ceil_div(in_tokens * im, 1_000_000) + ceil_div(out_tokens * om, 1_000_000)


def estimated_text_tokens(content: str, bpt: int = 4, overhead: int = 100, margin: int = 10) -> int:
    """DESIGN §5.4.1 estimated_text_tokens with the catalog estimation budgets."""
    b = len(content.encode("utf-8"))
    return ceil_div((ceil_div(b, bpt) + overhead) * (100 + margin), 100)


def nonce() -> str:
    return uuid.uuid4().hex[:12]


def ub(x: Any) -> bytes:
    """UUID (str/UUID) -> 16-byte blob as stored by the gear."""
    return uuid.UUID(str(x)).bytes


def as_uuid(v: Any) -> Optional[str]:
    if v is None:
        return None
    if isinstance(v, (bytes, bytearray, memoryview)) and len(bytes(v)) == 16:
        return str(uuid.UUID(bytes=bytes(v)))
    try:
        return str(uuid.UUID(str(v)))
    except Exception:
        return str(v)


def hex32(x: Any) -> str:
    return uuid.UUID(str(x)).hex


def utc_today() -> dt.date:
    return dt.datetime.now(dt.timezone.utc).date()


def period_starts() -> dict:
    today = utc_today()
    return {"daily": today.isoformat(), "monthly": today.replace(day=1).isoformat()}


def parse_ts(s: str) -> dt.datetime:
    s = s.strip().replace("Z", "+00:00")
    # Trim nanoseconds to microseconds for fromisoformat
    s = re.sub(r"(\.\d{6})\d+", r"\1", s)
    return dt.datetime.fromisoformat(s)


def wait_for(fn: Callable[[], Any], timeout: float = 20.0, interval: float = 0.25, desc: str = "condition") -> Any:
    deadline = time.time() + timeout
    last_exc: Optional[BaseException] = None
    while True:
        try:
            v = fn()
            if v:
                return v
        except AssertionError as e:  # allow assertion-style predicates
            last_exc = e
        if time.time() >= deadline:
            raise AssertionError(f"timed out after {timeout}s waiting for {desc}" + (f": {last_exc}" if last_exc else ""))
        time.sleep(interval)


# ---------------------------------------------------------------------------
# Problem (RFC 9457) assertions
# ---------------------------------------------------------------------------


def problem(resp: httpx.Response) -> dict:
    ctype = resp.headers.get("content-type", "")
    assert "text/event-stream" not in ctype, f"expected JSON error, got SSE stream (status {resp.status_code})"
    try:
        body = resp.json()
    except Exception as e:  # pragma: no cover
        raise AssertionError(f"error body is not JSON: {resp.status_code} {resp.text[:500]}") from e
    return body


def assert_problem(
    resp: httpx.Response,
    status: int,
    category: Optional[str] = None,
    *,
    field_reason: Optional[str] = None,
    field: Optional[str] = None,
    reason: Optional[str] = None,
    resource_type: Optional[str] = None,
    resource_name: Optional[str] = None,
    violation_subject: Optional[str] = None,
    violation_type: Optional[str] = None,
) -> dict:
    """Assert a canonical Problem response (ADR-0004)."""
    assert resp.status_code == status, f"expected {status}, got {resp.status_code}: {resp.text[:800]}"
    body = problem(resp)
    for k in ("type", "title", "status", "detail", "context"):
        assert k in body, f"Problem lacks '{k}': {body}"
    assert "code" not in body, f"Problem must not carry a top-level 'code': {body}"
    assert body["status"] == status, body
    ctx = body.get("context") or {}
    if category:
        assert f"err.{category}." in str(body["type"]), f"expected category {category} in type, got {body['type']}"
    if field_reason is not None:
        fvs = ctx.get("field_violations") or []
        reasons = [fv.get("reason") for fv in fvs]
        assert field_reason in reasons, f"expected field_violations reason {field_reason}, got {fvs} (ctx={ctx})"
        if field is not None:
            fields = [fv.get("field") for fv in fvs if fv.get("reason") == field_reason]
            assert field in fields, f"expected field {field} for reason {field_reason}, got {fvs}"
    if reason is not None:
        assert ctx.get("reason") == reason, f"expected context.reason={reason}, got {ctx}"
    if resource_type is not None:
        assert ctx.get("resource_type") == resource_type, f"expected resource_type={resource_type}, got {ctx}"
    if resource_name is not None:
        assert ctx.get("resource_name") == resource_name, f"expected resource_name={resource_name}, got {ctx}"
    if violation_subject is not None:
        vs = ctx.get("violations") or []
        subjects = [v.get("subject") for v in vs]
        assert violation_subject in subjects, f"expected violation subject {violation_subject}, got {vs}"
        if violation_type is not None:
            types = [v.get("type") for v in vs if v.get("subject") == violation_subject]
            assert violation_type in types, f"expected violation type {violation_type}, got {vs}"
    return body


# ---------------------------------------------------------------------------
# SSE
# ---------------------------------------------------------------------------


@dataclass
class StreamResult:
    status_code: int
    headers: dict
    events: list = field(default_factory=list)  # list of (event, data)
    times: list = field(default_factory=list)  # arrival time (s since request start) per event
    body: Any = None  # JSON body for non-SSE responses
    text: str = ""
    raw: str = ""
    disconnected_early: bool = False
    t_request: float = 0.0

    @property
    def is_sse(self) -> bool:
        return "text/event-stream" in (self.headers.get("content-type") or "")

    def names(self, include_ping: bool = True) -> list:
        return [e for e, _ in self.events if include_ping or e != "ping"]

    def of(self, name: str) -> list:
        return [d for e, d in self.events if e == name]

    def first(self, name: str) -> Optional[dict]:
        xs = self.of(name)
        return xs[0] if xs else None

    @property
    def started(self) -> dict:
        assert self.events and self.events[0][0] == "stream_started", f"first event is not stream_started: {self.events[:3]}"
        return self.events[0][1]

    @property
    def request_id(self) -> str:
        return self.started["request_id"]

    @property
    def message_id(self) -> str:
        return self.started["message_id"]

    @property
    def terminal(self) -> tuple:
        assert self.events, "no events"
        return self.events[-1]

    @property
    def done(self) -> dict:
        e, d = self.terminal
        assert e == "done", f"expected terminal done, got {e}: {d}"
        return d

    @property
    def error(self) -> dict:
        e, d = self.terminal
        assert e == "error", f"expected terminal error, got {e}: {d}"
        return d

    @property
    def text_content(self) -> str:
        return "".join(d.get("content", "") for d in self.of("delta") if isinstance(d, dict) and d.get("type") == "text")


def _parse_sse_lines(lines: Iterable[str], on_event: Callable[[str, Any], bool]) -> None:
    event = None
    data_lines: list = []
    for line in lines:
        if line.endswith("\r"):
            line = line[:-1]
        if line == "":
            if event is not None or data_lines:
                raw = "\n".join(data_lines)
                try:
                    data = json.loads(raw) if raw else None
                except Exception:
                    data = raw
                stop = on_event(event or "message", data)
                event, data_lines = None, []
                if stop:
                    return
            continue
        if line.startswith(":"):
            continue
        if ":" in line:
            k, v = line.split(":", 1)
            if v.startswith(" "):
                v = v[1:]
        else:
            k, v = line, ""
        if k == "event":
            event = v
        elif k == "data":
            data_lines.append(v)
    if event is not None or data_lines:
        raw = "\n".join(data_lines)
        try:
            data = json.loads(raw) if raw else None
        except Exception:
            data = raw
        on_event(event or "message", data)


def parse_sse_text(text: str) -> list:
    out: list = []
    _parse_sse_lines(text.split("\n"), lambda e, d: out.append((e, d)) or False)
    return out


def assert_sse_grammar(res: StreamResult) -> None:
    """stream_started ping* (delta|tool)* citations? (done|error) — DESIGN §3.3."""
    names = res.names()
    assert names, "empty stream"
    assert names[0] == "stream_started", f"first event must be stream_started: {names}"
    assert names[-1] in ("done", "error"), f"last event must be terminal: {names}"
    assert sum(1 for n in names if n in ("done", "error")) == 1, f"exactly one terminal event: {names}"
    assert names.count("stream_started") == 1, names
    seen_content = False
    seen_citations = False
    for n in names[1:-1]:
        if n == "ping":
            assert not seen_content, f"ping after content started: {names}"
            assert not seen_citations, names
        elif n in ("delta", "tool"):
            seen_content = True
            assert not seen_citations, f"{n} after citations: {names}"
        elif n == "citations":
            assert not seen_citations, f"more than one citations event: {names}"
            seen_citations = True
        else:
            raise AssertionError(f"unexpected event {n!r} in {names}")
    for e, d in res.events:
        if e == "ping":
            assert d == {} or d is None, f"ping data must be {{}}: {d}"


# ---------------------------------------------------------------------------
# HTTP client
# ---------------------------------------------------------------------------


class Api:
    def __init__(self, base_url: str, token: Optional[str], timeout: float = 60.0):
        self.base_url = base_url.rstrip("/")
        self.token = token
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        self.c = httpx.Client(base_url=self.base_url + "/mini-chat", headers=headers, timeout=timeout)

    def close(self) -> None:
        self.c.close()

    # raw verbs
    def get(self, path: str, **kw) -> httpx.Response:
        return self.c.get(path, **kw)

    def post(self, path: str, **kw) -> httpx.Response:
        return self.c.post(path, **kw)

    def patch(self, path: str, **kw) -> httpx.Response:
        return self.c.patch(path, **kw)

    def put(self, path: str, **kw) -> httpx.Response:
        return self.c.put(path, **kw)

    def delete(self, path: str, **kw) -> httpx.Response:
        return self.c.delete(path, **kw)

    # chats
    def create_chat(self, **body) -> dict:
        r = self.c.post("/v1/chats", json=body)
        assert r.status_code == 201, f"create chat failed: {r.status_code} {r.text[:500]}"
        return r.json()

    def chat(self, chat_id: str) -> dict:
        r = self.c.get(f"/v1/chats/{chat_id}")
        assert r.status_code == 200, f"get chat failed: {r.status_code} {r.text[:500]}"
        return r.json()

    def list_chats(self, **params) -> httpx.Response:
        return self.c.get("/v1/chats", params=params)

    def messages(self, chat_id: str, **params) -> list:
        params.setdefault("limit", 100)
        r = self.c.get(f"/v1/chats/{chat_id}/messages", params=params)
        assert r.status_code == 200, f"list messages failed: {r.status_code} {r.text[:500]}"
        return r.json()["items"]

    def turn(self, chat_id: str, request_id: str) -> httpx.Response:
        return self.c.get(f"/v1/chats/{chat_id}/turns/{request_id}")

    def turn_state(self, chat_id: str, request_id: str) -> Optional[str]:
        r = self.turn(chat_id, request_id)
        if r.status_code != 200:
            return None
        return r.json().get("state")

    def wait_turn_state(self, chat_id: str, request_id: str, states: Iterable[str], timeout: float = 20.0) -> dict:
        states = set(states)

        def check():
            r = self.turn(chat_id, request_id)
            if r.status_code == 200 and r.json().get("state") in states:
                return r.json()
            return None

        return wait_for(check, timeout=timeout, desc=f"turn {request_id} in {states}")

    def quota(self) -> dict:
        r = self.c.get("/v1/quota/status")
        assert r.status_code == 200, f"quota status failed: {r.status_code} {r.text[:500]}"
        return r.json()

    def quota_period(self, tier: str, period: str) -> Optional[dict]:
        for t in self.quota()["tiers"]:
            if t["tier"] == tier:
                for p in t["periods"]:
                    if p["period"] == period:
                        return p
        return None

    # attachments
    def upload(
        self,
        chat_id: str,
        filename: Optional[str],
        content: bytes,
        content_type: Optional[str],
        timeout: float = 60.0,
    ) -> httpx.Response:
        if content_type is None:
            files = {"file": (filename, content)}
        else:
            files = {"file": (filename, content, content_type)}
        return self.c.post(f"/v1/chats/{chat_id}/attachments", files=files, timeout=timeout)

    def upload_raw(self, chat_id: str, body: bytes, content_type: str) -> httpx.Response:
        return self.c.post(
            f"/v1/chats/{chat_id}/attachments", content=body, headers={"Content-Type": content_type}
        )

    def upload_ready(self, chat_id: str, filename: str, content: bytes, content_type: str) -> dict:
        r = self.upload(chat_id, filename, content, content_type)
        assert r.status_code == 201, f"upload failed: {r.status_code} {r.text[:500]}"
        body = r.json()
        assert body["status"] == "ready", body
        return body

    def attachment(self, chat_id: str, att_id: str) -> httpx.Response:
        return self.c.get(f"/v1/chats/{chat_id}/attachments/{att_id}")

    # streaming
    def stream(
        self,
        chat_id: str,
        content: str,
        request_id: Optional[str] = None,
        attachment_ids: Optional[list] = None,
        web_search: Optional[bool] = None,
        **kw,
    ) -> StreamResult:
        body: dict = {"content": content}
        if request_id is not None:
            body["request_id"] = str(request_id)
        if attachment_ids is not None:
            body["attachment_ids"] = [str(a) for a in attachment_ids]
        if web_search is not None:
            body["web_search"] = {"enabled": bool(web_search)}
        return self.sse("POST", f"/v1/chats/{chat_id}/messages:stream", json_body=body, **kw)

    def retry(self, chat_id: str, request_id: str, **kw) -> StreamResult:
        return self.sse("POST", f"/v1/chats/{chat_id}/turns/{request_id}/retry", json_body=None, **kw)

    def edit(self, chat_id: str, request_id: str, content: str, **kw) -> StreamResult:
        return self.sse("PATCH", f"/v1/chats/{chat_id}/turns/{request_id}", json_body={"content": content}, **kw)

    def sse(
        self,
        method: str,
        path: str,
        json_body: Any = None,
        stop_after: Optional[int] = None,
        stop_when: Optional[Callable[[str, Any], bool]] = None,
        timeout: float = 60.0,
        on_event: Optional[Callable[[str, Any], None]] = None,
        client: Optional[httpx.Client] = None,
        on_response: Optional[Callable[[httpx.Response], None]] = None,
    ) -> StreamResult:
        """Send a streaming request and collect ``(event, data)`` pairs.

        ``stop_after``/``stop_when`` close the connection early (client
        disconnect) once K events were received / the predicate is true.
        """
        c = client or self.c
        t0 = time.time()
        kwargs: dict = {"timeout": httpx.Timeout(timeout, read=timeout)}
        if json_body is not None:
            kwargs["json"] = json_body
        with c.stream(method, path, **kwargs) as resp:
            if on_response:
                on_response(resp)
            res = StreamResult(status_code=resp.status_code, headers={k.lower(): v for k, v in resp.headers.items()}, t_request=t0)
            if not res.is_sse:
                resp.read()
                res.text = resp.text
                try:
                    res.body = resp.json()
                except Exception:
                    res.body = None
                return res

            raw_parts: list = []

            def handle(e: str, d: Any) -> bool:
                res.events.append((e, d))
                res.times.append(time.time() - t0)
                if on_event:
                    on_event(e, d)
                if stop_after is not None and len(res.events) >= stop_after:
                    res.disconnected_early = True
                    return True
                if stop_when is not None and stop_when(e, d):
                    res.disconnected_early = True
                    return True
                return False

            def lines():
                for line in resp.iter_lines():
                    raw_parts.append(line)
                    yield line

            _parse_sse_lines(lines(), handle)
            res.raw = "\n".join(raw_parts)
        return res

    def as_response(self, res: StreamResult) -> httpx.Response:
        """Wrap a non-SSE StreamResult as an httpx.Response for assert_problem."""
        headers = {k: v for k, v in res.headers.items() if k not in ("content-encoding", "content-length", "transfer-encoding")}
        return httpx.Response(res.status_code, headers=headers, content=(res.text or res.raw).encode())


class BackgroundStream:
    """Run a streaming request in a thread (own HTTP client).

    ``started`` is set once ``stream_started`` arrived; ``first_delta`` once a
    delta arrived. ``stop()`` disconnects the client.
    """

    def __init__(self, api: Api, method: str, path: str, json_body: Any = None, timeout: float = 120.0):
        self.api = api
        self.method = method
        self.path = path
        self.json_body = json_body
        self.timeout = timeout
        self.started = threading.Event()
        self.first_delta = threading.Event()
        self.finished = threading.Event()
        self._stop = threading.Event()
        self.result: Optional[StreamResult] = None
        self.exc: Optional[BaseException] = None
        self.events: list = []
        self._client = httpx.Client(base_url=api.base_url + "/mini-chat", headers=dict(api.c.headers), timeout=timeout)
        self._resp: Optional[httpx.Response] = None
        self._t = threading.Thread(target=self._run, daemon=True)

    @classmethod
    def send(cls, api: Api, chat_id: str, content: str, request_id: Optional[str] = None, **body_extra) -> "BackgroundStream":
        body = {"content": content}
        if request_id:
            body["request_id"] = str(request_id)
        body.update(body_extra)
        return cls(api, "POST", f"/v1/chats/{chat_id}/messages:stream", body).start()

    def start(self) -> "BackgroundStream":
        self._t.start()
        return self

    def _run(self) -> None:
        def on_event(e, d):
            self.events.append((e, d))
            if e == "stream_started":
                self.started.set()
            if e in ("delta", "tool"):
                self.first_delta.set()

        try:
            self.result = self.api.sse(
                self.method,
                self.path,
                json_body=self.json_body,
                timeout=self.timeout,
                on_event=on_event,
                stop_when=lambda e, d: self._stop.is_set(),
                client=self._client,
                on_response=self._set_resp,
            )
        except BaseException as e:  # noqa: BLE001
            if not self._stop.is_set():
                self.exc = e
        finally:
            self.started.set()
            self.finished.set()

    def _set_resp(self, resp: httpx.Response) -> None:
        self._resp = resp

    def _shutdown_socket(self) -> None:
        """Interrupt a blocked read and send FIN at once (closing the fd from
        another thread would not wake the reader)."""
        resp = self._resp
        if resp is None:
            return
        try:
            ns = resp.extensions.get("network_stream")
            sock = ns.get_extra_info("socket") if ns is not None else None
            if sock is not None:
                sock.shutdown(socket.SHUT_RDWR)
        except Exception:
            pass

    @property
    def request_id(self) -> Optional[str]:
        for e, d in self.events:
            if e == "stream_started":
                return d.get("request_id")
        return None

    def wait_started(self, timeout: float = 30.0) -> "BackgroundStream":
        assert self.started.wait(timeout), "stream did not start"
        if self.exc:
            raise self.exc
        assert self.request_id, f"stream did not produce stream_started (result={self.result})"
        return self

    def wait_delta(self, timeout: float = 30.0) -> "BackgroundStream":
        assert self.first_delta.wait(timeout), f"no delta received (events={self.events})"
        return self

    def stop(self, wait: float = 10.0) -> None:
        """Disconnect: close the underlying client (drops the TCP connection)."""
        self._stop.set()
        self._shutdown_socket()
        try:
            self._client.close()
        except Exception:
            pass
        self.finished.wait(wait)

    def join(self, timeout: float = 60.0) -> Optional[StreamResult]:
        self.finished.wait(timeout)
        return self.result


# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------


class DB:
    def __init__(self, path_resolver: Callable[[], Path]):
        self._resolve = path_resolver
        self._path: Optional[Path] = None

    @property
    def path(self) -> Path:
        if self._path is None or not self._path.exists():
            self._path = self._resolve()
        return self._path

    def _connect(self, readonly: bool = True) -> sqlite3.Connection:
        if readonly:
            try:
                con = sqlite3.connect(f"file:{self.path}?mode=ro", uri=True, timeout=10)
                con.execute("PRAGMA busy_timeout = 10000")
                con.execute("SELECT 1").fetchone()
            except sqlite3.Error:
                con = sqlite3.connect(str(self.path), timeout=10)
                con.execute("PRAGMA busy_timeout = 10000")
        else:
            con = sqlite3.connect(str(self.path), timeout=10)
            con.execute("PRAGMA busy_timeout = 10000")
        con.row_factory = sqlite3.Row
        return con

    def query(self, sql: str, params: Iterable = ()) -> list:
        con = self._connect(True)
        try:
            return [dict(r) for r in con.execute(sql, tuple(params)).fetchall()]
        finally:
            con.close()

    def one(self, sql: str, params: Iterable = ()) -> Optional[dict]:
        rows = self.query(sql, params)
        return rows[0] if rows else None

    def scalar(self, sql: str, params: Iterable = ()) -> Any:
        con = self._connect(True)
        try:
            row = con.execute(sql, tuple(params)).fetchone()
            return None if row is None else row[0]
        finally:
            con.close()

    def execute(self, sql: str, params: Iterable = ()) -> int:
        con = self._connect(False)
        try:
            for attempt in range(20):
                try:
                    cur = con.execute(sql, tuple(params))
                    con.commit()
                    return cur.rowcount
                except sqlite3.OperationalError as e:
                    if "locked" in str(e) or "busy" in str(e):
                        time.sleep(0.2)
                        continue
                    raise
            raise AssertionError(f"database stayed locked: {sql}")
        finally:
            con.close()

    def tables(self) -> list:
        return [r["name"] for r in self.query("SELECT name FROM sqlite_master WHERE type='table'")]

    def columns(self, table: str) -> list:
        return [r["name"] for r in self.query(f"PRAGMA table_info('{table}')")]

    def table_info(self, table: str) -> list:
        return self.query(f"PRAGMA table_info('{table}')")

    # -- domain helpers -------------------------------------------------

    def turn_row(self, chat_id: str, request_id: str) -> Optional[dict]:
        return self.one("SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", (ub(chat_id), ub(request_id)))

    def turns(self, chat_id: str) -> list:
        return self.query("SELECT * FROM chat_turns WHERE chat_id = ?", (ub(chat_id),))

    def message_rows(self, chat_id: str) -> list:
        return self.query("SELECT * FROM messages WHERE chat_id = ? ORDER BY created_at, id", (ub(chat_id),))

    def attachment_row(self, att_id: str) -> Optional[dict]:
        return self.one("SELECT * FROM attachments WHERE id = ?", (ub(att_id),))

    def attachments_of_chat(self, chat_id: str) -> list:
        return self.query("SELECT * FROM attachments WHERE chat_id = ? ORDER BY created_at", (ub(chat_id),))

    def quota_rows(self, token: str) -> list:
        return self.query(
            "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ?",
            (ub(tenant_id(token)), ub(user_id(token))),
        )

    def quota_row(self, token: str, bucket: str, period_type: str) -> Optional[dict]:
        ps = period_starts()[period_type]
        rows = self.query(
            "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? AND bucket = ? AND period_type = ?",
            (ub(tenant_id(token)), ub(user_id(token)), bucket, period_type),
        )
        rows = [r for r in rows if str(r.get("period_start", "")).startswith(ps)]
        return rows[0] if rows else None

    def quota_snapshot(self, token: str) -> dict:
        """{(bucket, period_type): {spent, reserved, calls, input_tokens, output_tokens, web_search_calls}}"""
        out = {}
        ps = period_starts()
        for r in self.quota_rows(token):
            pt = r.get("period_type")
            if not str(r.get("period_start", "")).startswith(ps.get(pt, "????")):
                continue
            out[(r["bucket"], pt)] = {
                k: int(r.get(k) or 0)
                for k in ("spent_credits_micro", "reserved_credits_micro", "calls", "input_tokens", "output_tokens", "web_search_calls", "code_interpreter_calls")
                if k in r
            }
        return out

    def seed_quota(self, token: str, bucket: str, period_type: str, **values) -> None:
        """UPDATE (or INSERT, modelled on an existing row) a quota_usage row of the current period."""
        ps = period_starts()[period_type]
        row = self.quota_row(token, bucket, period_type)
        if row is not None:
            sets = ", ".join(f"{k} = ?" for k in values)
            n = self.execute(f"UPDATE quota_usage SET {sets} WHERE id = ?", list(values.values()) + [row["id"]])
            assert n == 1
            return
        cols = self.table_info("quota_usage")
        template = self.one("SELECT * FROM quota_usage LIMIT 1")
        rec: dict = {}
        now_s = None
        if template is not None:
            now_s = template.get("updated_at")
        for c in cols:
            name = c["name"]
            if name == "id":
                rec[name] = uuid.uuid4().bytes
            elif name == "tenant_id":
                rec[name] = ub(tenant_id(token))
            elif name == "user_id":
                rec[name] = ub(user_id(token))
            elif name == "period_type":
                rec[name] = period_type
            elif name == "period_start":
                rec[name] = ps if template is None else _same_shape_date(template.get("period_start"), ps)
            elif name == "bucket":
                rec[name] = bucket
            elif name in values:
                rec[name] = values[name]
            elif name in ("updated_at", "created_at"):
                rec[name] = now_s or dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d %H:%M:%S.%f+00:00")
            elif c.get("notnull") and c.get("dflt_value") is None:
                rec[name] = 0
        for k, v in values.items():
            rec[k] = v
        names = ", ".join(rec.keys())
        qs = ", ".join("?" for _ in rec)
        self.execute(f"INSERT INTO quota_usage ({names}) VALUES ({qs})", list(rec.values()))

    # -- outbox (tolerant) --------------------------------------------

    def outbox_tables(self) -> list:
        return [t for t in self.tables() if t.startswith("toolkit_outbox")]

    def outbox_payload_tables(self) -> list:
        out = []
        for t in self.outbox_tables():
            if "dead" in t:
                continue
            if "payload" in self.columns(t):
                out.append(t)
        return out

    def outbox_payloads(self, *needles: str) -> list:
        """Decoded payload strings of outbox body rows containing all needles."""
        res = []
        for t in self.outbox_payload_tables():
            for r in self.query(f"SELECT payload FROM {t}"):
                p = r["payload"]
                s = p.decode("utf-8", "replace") if isinstance(p, (bytes, bytearray, memoryview)) else str(p)
                if all(n in s for n in needles):
                    res.append(s)
        return res

    def outbox_count(self, *needles: str) -> int:
        return len(self.outbox_payloads(*needles))


def _same_shape_date(template: Any, iso_date: str) -> Any:
    if isinstance(template, str) and len(template) > 10:
        return iso_date + template[10:]
    return iso_date


def shift_timestamp_old(value: Any, seconds: int = 3600 * 24 * 365) -> Any:
    """Return ``value`` moved far into the past, keeping its textual format."""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        # unix seconds or milliseconds
        return value - (seconds * 1000 if value > 10_000_000_000 else seconds)
    s = value.decode() if isinstance(value, (bytes, bytearray)) else str(value)
    m = re.match(r"^(\d{4})(-\d{2}-\d{2})", s)
    assert m, f"unrecognised timestamp format: {s!r}"
    return f"{int(m.group(1)) - max(1, seconds // (3600 * 24 * 365))}{s[4:]}"


# ---------------------------------------------------------------------------
# Test file content factories
# ---------------------------------------------------------------------------


def make_png(width: int = 32, height: int = 24, rgb: tuple = (200, 30, 60)) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def make_pdf(text: str = "hello") -> bytes:
    return (
        b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n"
        b"2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\n"
        + f"% {text}\n".encode()
        + b"trailer << /Root 1 0 R >>\n%%EOF\n"
    )


def make_xlsx() -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr(
            "[Content_Types].xml",
            '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>',
        )
        z.writestr("xl/workbook.xml", "<workbook/>")
    return buf.getvalue()


def text_blob(n_bytes: int, marker: str = "") -> str:
    base = (marker + " ") if marker else ""
    filler = "lorem ipsum dolor sit amet "
    s = base + filler * (n_bytes // len(filler) + 1)
    return s[:n_bytes]


def all_event_payload_text(res: StreamResult) -> str:
    return json.dumps(res.events)


def request_input_roles_texts(req: dict) -> list:
    from mock_llm import input_items, item_text

    out = []
    for it in input_items(req.get("json")):
        if isinstance(it, dict):
            out.append((it.get("role") or it.get("type"), item_text(it)))
    return out


def find_tool(req: dict, type_prefix: str) -> Optional[dict]:
    for t in (req.get("json") or {}).get("tools") or []:
        if isinstance(t, dict) and str(t.get("type", "")).startswith(type_prefix):
            return t
    return None


def norm_uuid_text(s: Any) -> str:
    return str(s).replace("-", "").lower()
