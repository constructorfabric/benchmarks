"""Fixtures of the mini-chat end-to-end suite.

The suite starts its own mock provider (``mock_llm.py``) and one or more
``cf-gears-example-server`` processes built from ``base_config.yaml`` with
per-fixture overrides. Every process is stopped by its own pid.

Build the server first:

    cargo build --bin cf-gears-example-server --no-default-features \
        --features mini-chat,static-authn,static-authz,single-tenant,static-credstore
"""

import copy
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
import pytest
import yaml

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
SERVER_BIN = Path(os.environ.get("MINI_CHAT_SERVER_BIN", REPO / "target/debug/cf-gears-example-server"))
MOCK_PORT = int(os.environ.get("MINI_CHAT_MOCK_PORT", "18090"))
MOCK_URL = f"http://127.0.0.1:{MOCK_PORT}"

TENANT = "00000000-df51-5b42-9538-d2b56b7ee953"
USER_A = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_B = "22222222-6a88-4768-9dfc-6bcd5187d9ed"
USER_C = "33333333-6a88-4768-9dfc-6bcd5187d9ed"
OTHER_TENANT = "44444444-df51-5b42-9538-d2b56b7ee953"
USER_X = "55555555-6a88-4768-9dfc-6bcd5187d9ed"

_port_counter = [18200]


def _next_port():
    _port_counter[0] += 1
    return _port_counter[0]


def _wait_http(url, timeout=30.0, proc=None):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            return False
        try:
            httpx.get(url, timeout=1.0)
            return True
        except httpx.HTTPError:
            time.sleep(0.2)
    return False


# ── mock provider ──────────────────────────────────────────────────────────


class Mock:
    def __init__(self, url):
        self.url = url

    def reset(self):
        httpx.post(f"{self.url}/_mock/reset", timeout=5)

    def config(self, **kw):
        httpx.post(f"{self.url}/_mock/config", json=kw, timeout=5)

    def requests(self):
        return httpx.get(f"{self.url}/_mock/requests", timeout=5).json()

    def chat_requests(self, chat_id):
        out = []
        for r in self.requests():
            body = r.get("json") or {}
            meta = body.get("metadata") or {}
            if r["path"].endswith("/responses") and meta.get("chat_id") == str(chat_id) and meta.get("request_type", "chat") == "chat":
                out.append(r)
        return out

    def summary_requests(self, chat_id):
        out = []
        for r in self.requests():
            meta = (r.get("json") or {}).get("metadata") or {}
            if r["path"].endswith("/responses") and meta.get("chat_id") == str(chat_id) and meta.get("request_type") != "chat":
                out.append(r)
        return out

    def paths(self, method=None, contains=""):
        return [r for r in self.requests() if (method is None or r["method"] == method) and contains in r["path"]]


@pytest.fixture(scope="session")
def mock():
    proc = None
    if not _wait_http(f"{MOCK_URL}/health", timeout=0.5):
        log = open(tempfile.gettempdir() + "/mini-chat-e2e-mock.log", "w")
        proc = subprocess.Popen([sys.executable, str(HERE / "mock_llm.py"), str(MOCK_PORT)], stdout=log, stderr=subprocess.STDOUT)
        assert _wait_http(f"{MOCK_URL}/health", timeout=10, proc=proc), "mock provider did not start"
    m = Mock(MOCK_URL)
    yield m
    if proc is not None:
        proc.terminate()
        proc.wait(timeout=10)


# ── server ─────────────────────────────────────────────────────────────────


class Server:
    def __init__(self, name, overrides=None):
        self.name = name
        self.port = _next_port()
        self.home = Path(tempfile.mkdtemp(prefix=f"mini-chat-e2e-{name}-"))
        cfg = yaml.safe_load((HERE / "base_config.yaml").read_text())
        cfg["server"]["home_dir"] = str(self.home)
        gears = cfg["gears"]
        gears["api-gateway"]["config"]["bind_addr"] = f"127.0.0.1:{self.port}"
        gears["grpc-hub"]["config"]["listen_addr"] = f"uds://{self.home}/grpc.sock"
        gears["mini-chat"]["config"]["providers"]["openai"]["port"] = MOCK_PORT
        if overrides:
            overrides(cfg)
        self.cfg = cfg
        self.config_path = self.home / "config.yaml"
        self.config_path.write_text(yaml.safe_dump(cfg))
        self.log_path = self.home / "server.log"
        self.proc = None
        self.base = f"http://127.0.0.1:{self.port}/mini-chat/v1"

    @property
    def db_path(self):
        return self.home / "mini-chat" / "mini_chat.db"

    def start(self, wait_ready=True):
        assert SERVER_BIN.exists(), f"server binary not found: {SERVER_BIN}"
        offset = self.log_path.stat().st_size if self.log_path.exists() else 0
        log = open(self.log_path, "a")
        self.proc = subprocess.Popen(
            [str(SERVER_BIN), "--config", str(self.config_path), "run"],
            stdout=log,
            stderr=subprocess.STDOUT,
            cwd=str(self.home),
        )
        if wait_ready:
            deadline = time.time() + 60
            while time.time() < deadline:
                assert self.proc.poll() is None, f"server exited: {self.log_tail()}"
                if "mini-chat started" in self.log_path.read_text(errors="replace")[offset:]:
                    time.sleep(0.3)
                    return self
                time.sleep(0.2)
            raise AssertionError(f"server not ready: {self.log_tail()}")
        return self

    def stop(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=40)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        self.proc = None

    def restart(self):
        self.stop()
        return self.start()

    def log_text(self):
        return self.log_path.read_text(errors="replace")

    def log_tail(self, n=40):
        try:
            return "\n".join(self.log_text().splitlines()[-n:])
        except OSError:
            return ""

    def client(self, token="user-a"):
        return Api(self.base, token)

    def db(self):
        import sqlite3

        conn = sqlite3.connect(str(self.db_path), timeout=10)
        conn.row_factory = sqlite3.Row
        return conn

    def query(self, sql, *args):
        conn = self.db()
        try:
            return [dict(r) for r in conn.execute(sql, args).fetchall()]
        finally:
            conn.close()

    def cleanup(self):
        self.stop()
        if not os.environ.get("MINI_CHAT_KEEP_HOME"):
            shutil.rmtree(self.home, ignore_errors=True)


def start_server(name, overrides=None):
    return Server(name, overrides).start()


@pytest.fixture(scope="session")
def server(mock):
    s = start_server("default")
    yield s
    s.cleanup()


# ── HTTP / SSE helpers ─────────────────────────────────────────────────────


def parse_sse(text):
    events = []
    name, data = None, []
    for line in text.splitlines():
        if line.startswith(":"):
            continue
        if line == "":
            if name is not None or data:
                payload = "\n".join(data)
                try:
                    payload = json.loads(payload)
                except ValueError:
                    pass
                events.append((name or "message", payload))
            name, data = None, []
            continue
        if line.startswith("event:"):
            name = line[6:].strip()
        elif line.startswith("data:"):
            data.append(line[5:].lstrip())
    if name is not None or data:
        payload = "\n".join(data)
        try:
            payload = json.loads(payload)
        except ValueError:
            pass
        events.append((name or "message", payload))
    return events


class Stream:
    """Result of a streaming call: HTTP status, headers, JSON error or events."""

    def __init__(self, resp, text):
        self.status = resp.status_code
        self.headers = resp.headers
        self.text = text
        self.is_sse = resp.headers.get("content-type", "").startswith("text/event-stream")
        self.events = parse_sse(text) if self.is_sse else []
        self.json = None
        if not self.is_sse:
            try:
                self.json = json.loads(text)
            except ValueError:
                pass

    def names(self):
        return [e[0] for e in self.events]

    def first(self, name):
        for n, d in self.events:
            if n == name:
                return d
        return None

    def all(self, name):
        return [d for n, d in self.events if n == name]

    @property
    def terminal(self):
        return self.events[-1] if self.events else None

    def text_content(self):
        return "".join(d.get("content", "") for n, d in self.events if n == "delta" and d.get("type") == "text")


class Api:
    def __init__(self, base, token):
        self.base = base
        self.token = token
        self.http = httpx.Client(timeout=60.0)

    def headers(self, extra=None):
        h = {"Authorization": f"Bearer {self.token}"} if self.token else {}
        h.update(extra or {})
        return h

    def req(self, method, path, **kw):
        headers = self.headers(kw.pop("headers", None))
        return self.http.request(method, self.base + path, headers=headers, **kw)

    def get(self, path, **kw):
        return self.req("GET", path, **kw)

    def post(self, path, **kw):
        return self.req("POST", path, **kw)

    def patch(self, path, **kw):
        return self.req("PATCH", path, **kw)

    def put(self, path, **kw):
        return self.req("PUT", path, **kw)

    def delete(self, path, **kw):
        return self.req("DELETE", path, **kw)

    # domain helpers
    def create_chat(self, **body):
        r = self.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def stream(self, path, body=None, method="POST", timeout=60.0):
        kw = {"headers": self.headers({"Accept": "text/event-stream"}), "timeout": timeout}
        if body is not None:
            kw["json"] = body
        with self.http.stream(method, self.base + path, **kw) as resp:
            text = resp.read().decode()
        return Stream(resp, text)

    def send(self, chat_id, content="hello", **extra):
        body = {"content": content}
        body.update(extra)
        return self.stream(f"/chats/{chat_id}/messages:stream", body)

    def retry(self, chat_id, request_id):
        return self.stream(f"/chats/{chat_id}/turns/{request_id}/retry")

    def edit(self, chat_id, request_id, content):
        return self.stream(f"/chats/{chat_id}/turns/{request_id}", {"content": content}, method="PATCH")

    def messages(self, chat_id, query=""):
        r = self.get(f"/chats/{chat_id}/messages{query}")
        assert r.status_code == 200, r.text
        return r.json()["items"]

    def all_messages(self, chat_id):
        items, cursor = [], None
        while True:
            q = "?limit=100" + (f"&cursor={cursor}" if cursor else "")
            r = self.get(f"/chats/{chat_id}/messages{q}")
            assert r.status_code == 200, r.text
            page = r.json()
            items += page["items"]
            cursor = page["page_info"].get("next_cursor")
            if not cursor:
                return items

    def turn(self, chat_id, request_id):
        return self.get(f"/chats/{chat_id}/turns/{request_id}")

    def upload(self, chat_id, filename, data, content_type):
        return self.post(f"/chats/{chat_id}/attachments", files={"file": (filename, data, content_type)})

    def quota(self):
        r = self.get("/quota/status")
        assert r.status_code == 200, r.text
        return r.json()


def wait_for(pred, timeout=20.0, interval=0.2, msg="condition"):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {msg}; last={last!r}")


def violations(resp_json):
    ctx = (resp_json or {}).get("context") or {}
    return ctx.get("field_violations") or ctx.get("violations") or []


def reason(resp_json):
    v = violations(resp_json)
    if v:
        return v[0].get("reason") or v[0].get("type") or v[0].get("description")
    return ((resp_json or {}).get("context") or {}).get("reason")


@pytest.fixture
def api(server):
    return server.client("user-a")


@pytest.fixture
def api_b(server):
    return server.client("user-b")


@pytest.fixture
def chat(api):
    return api.create_chat()


@pytest.fixture(autouse=True)
def _reset_mock_config(mock):
    yield
    mock.config(index_status="completed", index_delay_secs=0, upload_status=200, vs_create_status=200, delete_fail_count=0, summary_fail_count=0)
