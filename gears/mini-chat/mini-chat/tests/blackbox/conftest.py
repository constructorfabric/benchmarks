"""Shared fixtures for the mini-chat black-box suite.

The suite drives the real debug server (``cf-gears-example-server``) over
HTTP/SSE against an in-process OpenAI-compatible mock provider and inspects
the gear's SQLite database.

* ``mock``    — session-wide :class:`MockProvider` (reset before every test).
* ``server``  — one server per test module. A module customises it with the
  module-level dicts ``MINI_CHAT_PATCH`` / ``POLICY_PATCH`` (deep-merged into
  the ``mini-chat`` / ``static-mini-chat-model-policy-plugin`` config).
  The server is started and stopped by pid (see ``server.py``).
* ``api`` / ``api_a2`` / ``api_b`` — HTTP clients for ``tok-a`` (user A),
  ``tok-a2`` (same tenant, other user) and ``tok-b`` (other tenant).
* ``db``      — read-only SQLite helper for the server's ``mini_chat.db``.

Build the binary first::

    cargo build --offline --bin cf-gears-example-server --no-default-features \\
        --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
"""

import json
import os
import shutil
import sqlite3
import threading
import time
import uuid

import httpx
import pytest

from mock_provider import MockProvider
from server import BINARY, Server

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
USER_A = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_A2 = "44444444-6a88-4768-9dfc-6bcd5187d9ed"
USER_B = "22222222-6a88-4768-9dfc-6bcd5187d9ed"
TENANT_B = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"

_RUNNING = []


# --------------------------------------------------------------------- helpers
def wait_until(fn, timeout=10.0, interval=0.1, desc="condition"):
    """Poll ``fn`` until it returns a truthy value; return that value."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {desc} (last={last!r})")


def parse_sse(text):
    """Parse an SSE body into ``[(event, data)]`` (``data`` JSON-decoded when possible)."""
    events = []
    for block in text.replace("\r\n", "\n").split("\n\n"):
        if not block.strip():
            continue
        name, data_lines = "message", []
        for line in block.split("\n"):
            if line.startswith(":"):
                continue
            if line.startswith("event:"):
                name = line[len("event:"):].strip()
            elif line.startswith("data:"):
                data_lines.append(line[len("data:"):].lstrip())
        raw = "\n".join(data_lines)
        try:
            data = json.loads(raw) if raw else None
        except ValueError:
            data = raw
        events.append((name, data))
    return events


class SseResult:
    def __init__(self, status, headers, text):
        self.status = status
        self.headers = headers
        self.text = text
        self.is_sse = headers.get("content-type", "").startswith("text/event-stream")
        self.events = parse_sse(text) if self.is_sse else []
        self.problem = None
        if not self.is_sse and text:
            try:
                self.problem = json.loads(text)
            except ValueError:
                self.problem = None

    @property
    def names(self):
        return [e for e, _ in self.events]

    def first(self, name):
        for e, d in self.events:
            if e == name:
                return d
        return None

    def all(self, name):
        return [d for e, d in self.events if e == name]

    @property
    def started(self):
        return self.first("stream_started")

    @property
    def request_id(self):
        return (self.started or {}).get("request_id")

    @property
    def message_id(self):
        return (self.started or {}).get("message_id")

    @property
    def done(self):
        return self.first("done")

    @property
    def error(self):
        return self.first("error")

    @property
    def text_content(self):
        return "".join(d["content"] for d in self.all("delta") if d.get("type") == "text")

    def __repr__(self):
        return f"SseResult(status={self.status}, names={self.names}, problem={self.problem})"


def reason(problem):
    """``context.reason`` of a canonical Problem response."""
    return ((problem or {}).get("context") or {}).get("reason")


def field_reasons(problem):
    ctx = (problem or {}).get("context") or {}
    return [v.get("reason") for v in ctx.get("field_violations", [])]


class Api:
    """Thin client over the mini-chat REST surface for one bearer token."""

    def __init__(self, server, token):
        self.server = server
        self.base = server.base
        self.token = token
        self.h = {"Authorization": f"Bearer {token}"} if token else {}
        self.http = httpx.Client(base_url=self.base, headers=self.h, timeout=30)

    def close(self):
        self.http.close()

    # plain JSON
    def get(self, path, **kw):
        return self.http.get(path, **kw)

    def post(self, path, **kw):
        return self.http.post(path, **kw)

    def patch(self, path, **kw):
        return self.http.patch(path, **kw)

    def put(self, path, **kw):
        return self.http.put(path, **kw)

    def delete(self, path, **kw):
        return self.http.delete(path, **kw)

    # chats
    def create_chat(self, **body):
        r = self.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def messages(self, chat_id, **params):
        r = self.get(f"/chats/{chat_id}/messages", params=params)
        assert r.status_code == 200, r.text
        return r.json()["items"]

    def turn_status(self, chat_id, request_id):
        return self.get(f"/chats/{chat_id}/turns/{request_id}")

    # streaming
    def sse(self, method, path, json_body=None, timeout=60):
        with self.http.stream(method, path, json=json_body, timeout=timeout) as resp:
            body = resp.read().decode()
            return SseResult(resp.status_code, {k.lower(): v for k, v in resp.headers.items()}, body)

    def send(self, chat_id, content="hello", **extra):
        return self.sse("POST", f"/chats/{chat_id}/messages:stream", {"content": content, **extra})

    def retry(self, chat_id, request_id):
        return self.sse("POST", f"/chats/{chat_id}/turns/{request_id}/retry")

    def edit(self, chat_id, request_id, content):
        return self.sse("PATCH", f"/chats/{chat_id}/turns/{request_id}", {"content": content})

    def delete_turn(self, chat_id, request_id):
        return self.delete(f"/chats/{chat_id}/turns/{request_id}")

    def send_in_background(self, chat_id, content="hello", **extra):
        """Start ``messages:stream`` in a thread; ``join()`` returns the :class:`SseResult`."""
        return Background(lambda: self.send(chat_id, content, **extra))

    def open_and_drop(self, chat_id, content, until, **extra):
        """Open a stream, read events until ``until(events)`` is true, then drop the connection.

        Returns the events seen before the disconnect."""
        seen = []
        with httpx.Client(base_url=self.base, headers=self.h, timeout=30) as c:
            with c.stream("POST", f"/chats/{chat_id}/messages:stream", json={"content": content, **extra}) as resp:
                assert resp.status_code == 200
                buf = ""
                for chunk in resp.iter_text():
                    buf += chunk
                    if "\n\n" in buf:
                        done_part, buf = buf.rsplit("\n\n", 1)
                        seen.extend(parse_sse(done_part + "\n\n"))
                        if until(seen):
                            break
        return seen

    # attachments
    def upload(self, chat_id, filename, data, content_type):
        return self.post(f"/chats/{chat_id}/attachments", files={"file": (filename, data, content_type)})

    def quota(self):
        r = self.get("/quota/status")
        assert r.status_code == 200, r.text
        return r.json()


class Background:
    def __init__(self, fn):
        self.result = None
        self.exc = None

        def run():
            try:
                self.result = fn()
            except BaseException as e:  # noqa: BLE001
                self.exc = e

        self.thread = threading.Thread(target=run, daemon=True)
        self.thread.start()

    def join(self, timeout=60):
        self.thread.join(timeout)
        assert not self.thread.is_alive(), "background request did not finish"
        if self.exc:
            raise self.exc
        return self.result


def ub(u):
    """UUID string -> 16-byte BLOB (SQLite storage format of the gear)."""
    return uuid.UUID(str(u)).bytes


def us(b):
    """16-byte BLOB -> canonical UUID string (``None`` passes through)."""
    return None if b is None else str(uuid.UUID(bytes=bytes(b)))


class Db:
    """Read-only access to the gear's SQLite database."""

    def __init__(self, path):
        self.path = path

    def rows(self, sql, *params):
        con = sqlite3.connect(f"file:{self.path}?mode=ro", uri=True, timeout=10)
        con.row_factory = sqlite3.Row
        try:
            return [dict(r) for r in con.execute(sql, params).fetchall()]
        finally:
            con.close()

    def one(self, sql, *params):
        rows = self.rows(sql, *params)
        assert len(rows) == 1, f"expected one row, got {len(rows)}: {rows}"
        return rows[0]

    def scalar(self, sql, *params):
        rows = self.rows(sql, *params)
        return next(iter(rows[0].values())) if rows else None

    # convenience
    def turns(self, chat_id, include_deleted=True):
        sql = "SELECT * FROM chat_turns WHERE chat_id = ?"
        if not include_deleted:
            sql += " AND deleted_at IS NULL"
        return self.rows(sql + " ORDER BY started_at, id", ub(chat_id))

    def turn(self, chat_id, request_id):
        return self.one("SELECT * FROM chat_turns WHERE chat_id = ? AND request_id = ?", ub(chat_id), ub(request_id))

    def messages(self, chat_id, include_deleted=True):
        sql = "SELECT * FROM messages WHERE chat_id = ?"
        if not include_deleted:
            sql += " AND deleted_at IS NULL"
        return self.rows(sql + " ORDER BY created_at, id", ub(chat_id))

    def quota_usage(self, user_id=USER_A, tenant_id=TENANT_A):
        rows = self.rows(
            "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? ORDER BY bucket, period_type",
            ub(tenant_id),
            ub(user_id),
        )
        return {(r["bucket"], r["period_type"]): r for r in rows}

    def outbox_tables(self):
        return [r["name"] for r in self.rows("SELECT name FROM sqlite_master WHERE type='table'")]


# --------------------------------------------------------------------- fixtures
def pytest_configure(config):
    config.addinivalue_line("markers", "slow: test takes several seconds")


@pytest.fixture(scope="session")
def mock():
    if not os.path.exists(BINARY):
        pytest.exit(f"server binary not found at {BINARY}; build it first (see conftest docstring)", returncode=2)
    m = MockProvider()
    m.start()
    yield m
    m.stop()


@pytest.fixture(autouse=True)
def _reset_mock(mock):
    mock.reset()
    yield


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    rep = outcome.get_result()
    if rep.failed:
        item.module._bb_failed = True
        srv = getattr(item.module, "_bb_server", None)
        if srv is not None:
            rep.sections.append(("server log (tail)", f"home: {srv.home}\n" + srv.log()[-6000:]))


@pytest.fixture(scope="module")
def server(mock, request):
    mc_patch = getattr(request.module, "MINI_CHAT_PATCH", None)
    pol_patch = getattr(request.module, "POLICY_PATCH", None)
    s = Server(mock.port, mini_chat_patch=mc_patch, policy_patch=pol_patch)
    _RUNNING.append(s)
    request.module._bb_server = s
    try:
        s.start()
    except Exception:
        s.stop()
        raise
    yield s
    s.stop()
    _RUNNING.remove(s)
    # keep the home dir (config, logs, DB) only when something failed or on request
    if not getattr(request.module, "_bb_failed", False) and not os.environ.get("MINI_CHAT_BB_KEEP"):
        shutil.rmtree(s.home, ignore_errors=True)


def pytest_sessionfinish(session, exitstatus):
    # safety net: never leave a server behind (stopped by pid)
    for s in list(_RUNNING):
        s.stop()


@pytest.fixture(scope="module")
def api(server):
    c = Api(server, "tok-a")
    yield c
    c.close()


@pytest.fixture(scope="module")
def api_a2(server):
    c = Api(server, "tok-a2")
    yield c
    c.close()


@pytest.fixture(scope="module")
def api_b(server):
    c = Api(server, "tok-b")
    yield c
    c.close()


@pytest.fixture(scope="module")
def anon(server):
    c = Api(server, None)
    yield c
    c.close()


@pytest.fixture(scope="module")
def db(server):
    return Db(server.db_path)


@pytest.fixture
def chat(api):
    """A fresh chat of user A with the default model."""
    return api.create_chat(title="bb")
