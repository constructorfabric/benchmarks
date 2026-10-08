"""Fixtures of the mini-chat black-box E2E suite.

The suite starts the debug example server (built with
``--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore``)
against the scriptable mock provider in ``mock_llm.py`` and checks the gear
through HTTP/SSE, its SQLite database and the requests the mock received.

Run: ``python3 -m pytest gears/mini-chat/mini-chat/tests/e2e -q``
"""

from __future__ import annotations

import itertools
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Callable, Iterator

import httpx
import pytest

import config_template as ct
from mock_llm import MockLlmServer, free_port

REPO_ROOT = Path(__file__).resolve().parents[5]
BINARY = Path(os.environ.get("MINI_CHAT_SERVER_BIN", REPO_ROOT / "target" / "debug" / "cf-gears-example-server"))
PREFIX = "/mini-chat/v1"

TENANT_A = ct.TENANT_A
TENANT_B = ct.TENANT_B


# ───────────────────────────── helpers ─────────────────────────────


def ub(value: str | uuid.UUID) -> bytes:
    """UUID as stored by the gear in SQLite (16-byte blob)."""
    return uuid.UUID(str(value)).bytes


def wait_until(pred: Callable[[], Any], timeout: float = 15.0, interval: float = 0.2, msg: str = "condition") -> Any:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {msg} (last={last!r})")


class SseResult:
    def __init__(self, status: int, headers: httpx.Headers, events: list[tuple[str, Any]], body: Any = None):
        self.status = status
        self.headers = headers
        self.events = events
        self.body = body

    @property
    def names(self) -> list[str]:
        return [e for e, _ in self.events]

    def first(self, name: str) -> Any:
        for e, d in self.events:
            if e == name:
                return d
        raise AssertionError(f"no {name} event in {self.names}")

    def all(self, name: str) -> list[Any]:
        return [d for e, d in self.events if e == name]

    @property
    def text(self) -> str:
        return "".join(d["content"] for e, d in self.events if e == "delta" and d.get("type") == "text")

    @property
    def terminal(self) -> tuple[str, Any]:
        return self.events[-1]

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return f"SseResult(status={self.status}, events={self.events!r}, body={self.body!r})"


def parse_sse_lines(lines: Iterator[str], on_event: Callable[[str, Any], bool] | None = None) -> list[tuple[str, Any]]:
    events: list[tuple[str, Any]] = []
    name: str | None = None
    data: list[str] = []
    for line in lines:
        if line == "":
            if name is not None or data:
                raw = "\n".join(data)
                try:
                    payload: Any = json.loads(raw) if raw else None
                except json.JSONDecodeError:
                    payload = raw
                ev = (name or "message", payload)
                events.append(ev)
                if on_event is not None and on_event(*ev):
                    break
            name, data = None, []
            continue
        if line.startswith(":"):
            continue
        field, _, value = line.partition(":")
        value = value[1:] if value.startswith(" ") else value
        if field == "event":
            name = value
        elif field == "data":
            data.append(value)
    return events


class Api:
    """HTTP client for one caller (bearer token)."""

    def __init__(self, base: str, token: str | None, user_id: str | None = None, tenant_id: str | None = None):
        self.base = base
        self.token = token
        self.user_id = user_id
        self.tenant_id = tenant_id
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        self.http = httpx.Client(base_url=base, headers=headers, timeout=60.0)

    def req(self, method: str, path: str, **kw: Any) -> httpx.Response:
        return self.http.request(method, PREFIX + path, **kw)

    def get(self, path: str, **kw: Any) -> httpx.Response:
        return self.req("GET", path, **kw)

    def post(self, path: str, **kw: Any) -> httpx.Response:
        return self.req("POST", path, **kw)

    def put(self, path: str, **kw: Any) -> httpx.Response:
        return self.req("PUT", path, **kw)

    def patch(self, path: str, **kw: Any) -> httpx.Response:
        return self.req("PATCH", path, **kw)

    def delete(self, path: str, **kw: Any) -> httpx.Response:
        return self.req("DELETE", path, **kw)

    # ── domain shortcuts ──

    def create_chat(self, **body: Any) -> dict[str, Any]:
        r = self.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def stream_path(
        self,
        path: str,
        body: dict[str, Any] | None = None,
        *,
        method: str = "POST",
        stop: Callable[[str, Any], bool] | None = None,
    ) -> SseResult:
        with self.http.stream(method, PREFIX + path, json=body if body is not None else {}) as r:
            if r.headers.get("content-type", "").startswith("text/event-stream"):
                events = parse_sse_lines(r.iter_lines(), stop)
                return SseResult(r.status_code, r.headers, events)
            r.read()
            try:
                payload = r.json()
            except Exception:  # noqa: BLE001
                payload = r.text
            return SseResult(r.status_code, r.headers, [], payload)

    def send(self, chat_id: str, content: str = "Hello", **extra: Any) -> SseResult:
        body = {"content": content, **extra}
        return self.stream_path(f"/chats/{chat_id}/messages:stream", body)

    def retry(self, chat_id: str, request_id: str) -> SseResult:
        return self.stream_path(f"/chats/{chat_id}/turns/{request_id}/retry", {})

    def edit(self, chat_id: str, request_id: str, content: str) -> SseResult:
        return self.stream_path(f"/chats/{chat_id}/turns/{request_id}", {"content": content}, method="PATCH")

    def upload(
        self,
        chat_id: str,
        data: bytes,
        filename: str | None = "doc.txt",
        content_type: str | None = "text/plain",
    ) -> httpx.Response:
        if content_type is None:
            # Hand-built part without a Content-Type header.
            boundary = "----mcboundary"
            disp = f'form-data; name="file"; filename="{filename}"' if filename else 'form-data; name="file"'
            body = (
                f"--{boundary}\r\nContent-Disposition: {disp}\r\n\r\n".encode()
                + data
                + f"\r\n--{boundary}--\r\n".encode()
            )
            return self.post(
                f"/chats/{chat_id}/attachments",
                content=body,
                headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
            )
        if filename is None:
            boundary = "----mcboundary"
            body = (
                f'--{boundary}\r\nContent-Disposition: form-data; name="file"\r\nContent-Type: {content_type}\r\n\r\n'.encode()
                + data
                + f"\r\n--{boundary}--\r\n".encode()
            )
            return self.post(
                f"/chats/{chat_id}/attachments",
                content=body,
                headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
            )
        return self.post(f"/chats/{chat_id}/attachments", files={"file": (filename, data, content_type)})

    def messages(self, chat_id: str, **params: Any) -> list[dict[str, Any]]:
        r = self.get(f"/chats/{chat_id}/messages", params=params)
        assert r.status_code == 200, r.text
        return r.json()["items"]

    def turn(self, chat_id: str, request_id: str) -> httpx.Response:
        return self.get(f"/chats/{chat_id}/turns/{request_id}")


class Mock:
    def __init__(self, server: MockLlmServer):
        self.server = server
        self.base = f"http://127.0.0.1:{server.port}"
        self.http = httpx.Client(base_url=self.base, timeout=10)

    def reset(self) -> None:
        self.http.post("/__mock/reset")

    def script(self, *items: dict[str, Any]) -> None:
        self.http.post("/__mock/responses", json=list(items))

    def summary_script(self, *items: dict[str, Any], chat_id: str | None = None) -> None:
        if chat_id:
            items = tuple({**i, "for_chat": chat_id} for i in items)
        self.http.post("/__mock/summary_responses", json=list(items))

    def config(self, **kw: Any) -> None:
        self.http.post("/__mock/config", json=kw)

    def requests(self, method: str | None = None, contains: str | None = None) -> list[dict[str, Any]]:
        out = self.http.get("/__mock/requests").json()
        if method:
            out = [r for r in out if r["method"] == method]
        if contains:
            out = [r for r in out if contains in r["path"]]
        return out

    def chat_requests(self, chat_id: str | None = None) -> list[dict[str, Any]]:
        out = []
        for r in self.requests("POST", "/responses"):
            meta = (r.get("json") or {}).get("metadata") or {}
            if meta.get("request_type") == "summary":
                continue
            if chat_id and meta.get("chat_id") != chat_id:
                continue
            out.append(r)
        return out

    def summary_requests(self, chat_id: str | None = None) -> list[dict[str, Any]]:
        out = []
        for r in self.requests("POST", "/responses"):
            meta = (r.get("json") or {}).get("metadata") or {}
            if meta.get("request_type") == "summary" and (chat_id is None or meta.get("chat_id") == chat_id):
                out.append(r)
        return out

    def stats(self) -> dict[str, Any]:
        return self.http.get("/__mock/stats").json()


class Server:
    """One example-server process with a rendered config."""

    def __init__(self, name: str, mock_port: int, **render_kw: Any):
        self.name = name
        self.port = free_port()
        self.dir = Path(tempfile.mkdtemp(prefix=f"mc-e2e-{name}-"))
        self.home = self.dir / "home"
        self.home.mkdir()
        self.config_path = self.dir / "config.yaml"
        self.config_path.write_text(ct.render(port=self.port, mock_port=mock_port, home_dir=str(self.home), **render_kw))
        self.log_path = self.dir / "server.log"
        self.pid: int | None = None
        self.base = f"http://127.0.0.1:{self.port}"

    def start(self) -> None:
        if not BINARY.exists():
            raise RuntimeError(f"server binary not found at {BINARY}; build it first")
        log = open(self.log_path, "wb")  # noqa: SIM115
        proc = subprocess.Popen(  # noqa: S603
            [str(BINARY), "--config", str(self.config_path), "run"],
            stdout=log,
            stderr=subprocess.STDOUT,
            cwd=str(self.dir),
        )
        self.proc = proc
        self.pid = proc.pid
        (self.dir / "server.pid").write_text(str(proc.pid))
        deadline = time.time() + 90
        while time.time() < deadline:
            if proc.poll() is not None:
                raise RuntimeError(f"server {self.name} exited early:\n{self.log()[-4000:]}")
            try:
                r = httpx.get(
                    f"{self.base}{PREFIX}/models",
                    headers={"Authorization": f"Bearer {ct.token(0, 0)}"},
                    timeout=2,
                )
                if r.status_code == 200:
                    return
            except httpx.HTTPError:
                pass
            time.sleep(0.3)
        raise RuntimeError(f"server {self.name} did not become ready:\n{self.log()[-4000:]}")

    def stop(self) -> None:
        if self.pid is None:
            return
        try:
            os.kill(self.pid, signal.SIGTERM)
            self.proc.wait(timeout=30)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.pid = None

    def log(self) -> str:
        try:
            return self.log_path.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    @property
    def db_path(self) -> Path:
        return self.home / "mini-chat" / "mini_chat.db"

    def db(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.db_path), timeout=30)
        conn.row_factory = sqlite3.Row
        return conn

    def query(self, sql: str, *args: Any) -> list[sqlite3.Row]:
        conn = self.db()
        try:
            return list(conn.execute(sql, args))
        finally:
            conn.close()

    def execute(self, sql: str, *args: Any) -> int:
        conn = self.db()
        try:
            cur = conn.execute(sql, args)
            conn.commit()
            return cur.rowcount
        finally:
            conn.close()

    def outbox_payloads(self, queue: str) -> list[dict[str, Any]]:
        """Payloads enqueued to an outbox queue (body table)."""
        conn = self.db()
        try:
            cols = [r[1] for r in conn.execute("PRAGMA table_info(toolkit_outbox_body)")]
            rows = list(conn.execute("SELECT * FROM toolkit_outbox_body"))
        finally:
            conn.close()
        out = []
        for row in rows:
            rec = dict(zip(cols, row))
            payload = rec.get("payload")
            if isinstance(payload, (bytes, bytearray)):
                try:
                    payload = json.loads(payload)
                except (ValueError, UnicodeDecodeError):
                    continue
            elif isinstance(payload, str):
                try:
                    payload = json.loads(payload)
                except ValueError:
                    continue
            else:
                continue
            ptype = rec.get("payload_type") or ""
            if queue in ptype or queue == "*":
                out.append(payload)
        return out

    def json_log(self) -> list[dict[str, Any]]:
        out = []
        text = ""
        for name in ("server.log", "mini-chat.log"):
            try:
                text += (self.home / "logs" / name).read_text(errors="replace") + "\n"
            except FileNotFoundError:
                pass
        for line in text.splitlines():
            if not line.startswith("{"):
                continue
            try:
                out.append(json.loads(line))
            except ValueError:
                continue
        return out

    def plugin_events(self, message: str) -> list[dict[str, Any]]:
        out = []
        for rec in self.json_log():
            fields = rec.get("fields") or {}
            if fields.get("message") == message and "event" in fields:
                try:
                    out.append(json.loads(fields["event"]))
                except ValueError:
                    pass
        return out

    def usage_events(self, **match: Any) -> list[dict[str, Any]]:
        evs = self.plugin_events("mini-chat usage event")
        return [e for e in evs if all(e.get(k) == v for k, v in match.items())]

    def audit_events(self, **match: Any) -> list[dict[str, Any]]:
        evs = self.plugin_events("mini-chat audit event")
        return [e for e in evs if all(e.get(k) == v for k, v in match.items())]

    # ── callers ──

    def api(self, tenant_idx: int, n: int) -> Api:
        tenant = TENANT_A if tenant_idx == 0 else TENANT_B
        return Api(self.base, ct.token(tenant_idx, n), ct.user_id(tenant_idx, n), tenant)

    def anonymous(self) -> Api:
        return Api(self.base, None)


# ───────────────────────────── fixtures ─────────────────────────────

_user_counter = itertools.count(1)

LIMITS_EXTRA = """      rag:
        max_documents_per_chat: 2
        max_total_upload_mb_per_chat: 1
        uploaded_file_max_size_kb: 600
        uploaded_image_max_size_kb: 64
        max_images_per_message: 2
        max_concurrent_uploads: 4
"""

def _knowledge_extra(mock_port_placeholder: str = "{mock_port}") -> str:
    return """        kb:
          kind: openai_responses
          storage_kind: azure
          api_version: "2025-04-01-preview"
          host: "localhost"
          port: {mock_port}
          use_http: true
          upstream_alias: "mock-kb"
          api_path: "/openai/v1/responses"
      knowledge_search:
        enabled: true
        vector_store_id: "vs_kb_1"
        provider_id: "kb"
        max_calls_per_message: 3
        top_k: 4
        max_chunk_chars: 20
"""


SERVER_PRESETS: dict[str, dict[str, Any]] = {
    "default": {},
    "limits": {"extra_mini_chat": LIMITS_EXTRA},
    "kill": {
        "kill_switches": {
            "disable_web_search": True,
            "disable_images": True,
            "disable_code_interpreter": True,
            "disable_file_search": True,
            "force_standard_tier": True,
        }
    },
    "tight_quota": {"premium_daily": 50_000, "standard_daily": 20_000},
    "azure": {"storage_kind": "azure", "api_version": "2025-04-01-preview"},
    "knowledge": {"extra_mini_chat": "__KNOWLEDGE__"},
}


@pytest.fixture(scope="session")
def mock_server() -> Iterator[MockLlmServer]:
    srv = MockLlmServer()
    srv.start()
    yield srv
    srv.stop()


@pytest.fixture(scope="session")
def mock(mock_server: MockLlmServer) -> Mock:
    return Mock(mock_server)


@pytest.fixture(scope="session")
def servers(mock_server: MockLlmServer) -> Iterator[Callable[[str], Server]]:
    started: dict[str, Server] = {}

    def get(name: str) -> Server:
        if name not in started:
            preset = dict(SERVER_PRESETS[name])
            if preset.get("extra_mini_chat") == "__KNOWLEDGE__":
                preset["extra_mini_chat"] = _knowledge_extra().replace("{mock_port}", str(mock_server.port))
            srv = Server(name, mock_server.port, **preset)
            srv.start()
            started[name] = srv
        return started[name]

    yield get
    for srv in started.values():
        srv.stop()
        if os.environ.get("MINI_CHAT_E2E_KEEP") != "1":
            shutil.rmtree(srv.dir, ignore_errors=True)


@pytest.fixture(scope="session")
def server(servers: Callable[[str], Server]) -> Server:
    return servers("default")


@pytest.fixture(autouse=True)
def _reset_mock(mock: Mock) -> None:
    mock.reset()


def _fresh(srv: Server, tenant_idx: int = 0) -> Api:
    n = next(_user_counter)
    assert n < ct.NUM_USERS, "out of test users; raise NUM_USERS in config_template.py"
    return srv.api(tenant_idx, n)


@pytest.fixture
def new_user(server: Server) -> Callable[..., Api]:
    def make(tenant_idx: int = 0, srv: Server | None = None) -> Api:
        return _fresh(srv or server, tenant_idx)

    return make


@pytest.fixture
def api(new_user: Callable[..., Api]) -> Api:
    return new_user()


@pytest.fixture
def other_user(new_user: Callable[..., Api]) -> Api:
    return new_user()


@pytest.fixture
def tenant_b_user(new_user: Callable[..., Api]) -> Api:
    return new_user(1)


# ───────────────────────────── assertions ─────────────────────────────


def assert_problem(
    r: httpx.Response | SseResult,
    status: int,
    *,
    category: str | None = None,
    reason: str | None = None,
    field: str | None = None,
    resource_type: str | None = None,
) -> dict[str, Any]:
    if isinstance(r, SseResult):
        code, body, headers = r.status, r.body, r.headers
    else:
        code, headers = r.status_code, r.headers
        try:
            body = r.json()
        except Exception:  # noqa: BLE001
            body = r.text
    assert code == status, f"expected {status}, got {code}: {body}"
    assert isinstance(body, dict), body
    assert headers.get("content-type", "").startswith("application/problem+json"), headers.get("content-type")
    assert body.get("status") == status
    if category:
        assert category in body.get("type", ""), body
    ctx = body.get("context") or {}
    if reason:
        found = [ctx.get("reason")]
        for fv in ctx.get("field_violations") or []:
            found.append(fv.get("reason"))
        for v in ctx.get("violations") or []:
            found.append(v.get("type"))
            found.append(v.get("subject"))
        for v in ctx.get("quota_violations") or ctx.get("violations") or []:
            found.append(v.get("subject"))
        found.append(ctx.get("resource_name"))
        assert reason in found, f"reason {reason} not in {body}"
    if field:
        fields = [fv.get("field") for fv in ctx.get("field_violations") or []]
        assert field in fields, body
    if resource_type:
        assert ctx.get("resource_type") == resource_type, body
    return body


def ok_stream(r: SseResult) -> SseResult:
    assert r.status == 200, r
    assert r.names[0] == "stream_started", r.names
    assert r.names[-1] == "done", r
    return r
