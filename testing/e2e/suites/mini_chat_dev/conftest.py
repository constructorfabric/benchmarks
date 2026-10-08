"""Self-managed black-box suite for the mini-chat gear (``launcher: pytest``).

The suite builds nothing: it runs the debug server binary
(``target/debug/cf-gears-example-server`` or ``$E2E_BINARY``) built with
``--no-default-features --features
mini-chat,static-authn,static-authz,single-tenant,static-credstore`` against an
in-process OpenAI-compatible mock (``mock_llm.py``), a temp ``home_dir`` and
unique ports. It is collected only when its directory is named on the pytest
command line (or ``MINI_CHAT_DEV_E2E=1``), so whole-tree runs of
``testing/e2e`` are not affected.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator, Optional

import httpx
import pytest

from .mock_llm import MockLLM

SUITE_DIR = Path(__file__).resolve().parent
REPO_ROOT = SUITE_DIR.parents[3]
TEMPLATE = SUITE_DIR / "server_config.yaml.tmpl"
TEST_TIMEOUT_SECS = 180

TENANT_1 = "10000000-0000-4000-8000-000000000001"
TENANT_2 = "20000000-0000-4000-8000-000000000002"
USERS = {
    "A": {"token": "mc-token-user-a", "id": "a0000000-0000-4000-8000-00000000000a", "tenant": TENANT_1},
    "B": {"token": "mc-token-user-b", "id": "b0000000-0000-4000-8000-00000000000b", "tenant": TENANT_1},
    "C": {"token": "mc-token-user-c", "id": "c0000000-0000-4000-8000-00000000000c", "tenant": TENANT_2},
}
PREFIX = "/mini-chat/v1"
RT_CHAT = "gts.cf.core.mini_chat.chat.v1~"
RT_MESSAGE = "gts.cf.core.mini_chat.message.v1~"
RT_TURN = "gts.cf.core.mini_chat.turn.v1~"
RT_ATTACHMENT = "gts.cf.core.mini_chat.attachment.v1~"
RT_MODEL = "gts.cf.core.mini_chat.model.v1~"
RT_ODATA = "gts.cf.core.odata.query.v1~"

# The two configured provider entries (both `openai_responses`, one mock listener each).
PROVIDERS = {
    "openai": {"model": "gpt-4.1", "chat_path": "/v1/responses", "storage_prefix": "/v1", "query": ""},
    "azure": {
        "model": "azure-gpt-4.1",
        "chat_path": "/openai/v1/responses",
        "storage_prefix": "/openai",
        "query": "api-version=2025-03-01-preview",
    },
}

PROVIDER_AUTH_UNREADABLE = """          auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
          auth_config:
            header: "authorization"
            prefix: "Bearer "
            secret_ref: "openai-key"
"""


# ── collection gate + per-test timeout ─────────────────────────────────────


def _suite_requested(config: pytest.Config) -> bool:
    if os.getenv("MINI_CHAT_DEV_E2E") == "1":
        return True
    return any("mini_chat_dev" in str(a) for a in config.args)


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    requested = _suite_requested(config)
    skip = pytest.mark.skip(reason="mini_chat_dev runs only when its path is given (or MINI_CHAT_DEV_E2E=1)")
    for item in items:
        if SUITE_DIR not in Path(str(item.fspath)).parents:
            continue
        if not requested:
            item.add_marker(skip)
        elif item.get_closest_marker("timeout") is None:
            item.add_marker(pytest.mark.timeout(TEST_TIMEOUT_SECS))


# ── server lifecycle ───────────────────────────────────────────────────────


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def binary_path() -> Path:
    env = os.getenv("E2E_BINARY")
    return Path(env) if env else REPO_ROOT / "target" / "debug" / "cf-gears-example-server"


def render_config(home: Path, api_port: int, mock_port: int, azure_port: int, provider_auth: str = "") -> str:
    text = TEMPLATE.read_text()
    return (
        text.replace("__HOME__", str(home))
        .replace("__API_PORT__", str(api_port))
        .replace("__MOCK_AZURE_PORT__", str(azure_port))
        .replace("__MOCK_PORT__", str(mock_port))
        .replace("__PROVIDER_AUTH__", provider_auth.rstrip("\n"))
    )


@dataclass
class Server:
    home: Path
    api_port: int
    pid_file: Path
    log_path: Path
    proc: Optional[subprocess.Popen] = None

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.api_port}"

    @property
    def db_path(self) -> Path:
        return self.home / "mini-chat" / "mini_chat.db"

    def log_text(self) -> str:
        return self.log_path.read_text(errors="replace") if self.log_path.exists() else ""

    def start(self, config_text: str, ready_timeout: float = 120.0) -> "Server":
        binary = binary_path()
        if not binary.exists():
            pytest.skip(f"server binary not found: {binary} (build it first)")
        cfg = self.home / "server.yaml"
        cfg.write_text(config_text)
        log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            [str(binary), "--config", str(cfg), "run"],
            stdout=log,
            stderr=subprocess.STDOUT,
            cwd=str(self.home),
            env={**os.environ, "RUST_BACKTRACE": "1"},
        )
        self.pid_file.write_text(str(self.proc.pid))
        deadline = time.monotonic() + ready_timeout
        headers = {"Authorization": f"Bearer {USERS['A']['token']}"}
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited early ({self.proc.returncode}):\n{self.log_text()[-4000:]}")
            try:
                r = httpx.get(f"{self.base_url}{PREFIX}/models", headers=headers, timeout=2)
                if r.status_code == 200:
                    return self
            except httpx.HTTPError:
                pass
            time.sleep(0.3)
        self.stop()
        raise RuntimeError(f"server not ready in {ready_timeout}s:\n{self.log_text()[-4000:]}")

    def stop(self) -> None:
        # Signal only the child this fixture spawned and only while it is still
        # running (never a pid read back from a file, never by process name).
        proc = self.proc
        if proc is not None and proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)
        self.pid_file.unlink(missing_ok=True)


def deep_merge(base: dict, patch: dict) -> dict:
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(base.get(k), dict):
            deep_merge(base[k], v)
        else:
            base[k] = v
    return base


def new_server(
    mock: MockLLM, name: str = "main", provider_auth: str = "", overrides: Optional[dict] = None
) -> Server:
    """Start a server; ``name`` labels its diagnostic pid file
    (``/tmp/mini_chat_dev_<pytest pid>_<name>.pid``, unique per run)."""
    pid_file = Path(f"/tmp/mini_chat_dev_{os.getpid()}_{name}.pid")
    home = Path(tempfile.mkdtemp(prefix="mini_chat_dev_"))
    (home / "logs").mkdir()
    srv = Server(home=home, api_port=free_port(), pid_file=pid_file, log_path=home / "server.out")
    try:
        text = render_config(home, srv.api_port, mock.port, mock.azure_port, provider_auth)
        if overrides:
            # PyYAML is not in testing/e2e/requirements.txt: import it only here,
            # so collecting the suite never needs it.
            yaml = pytest.importorskip("yaml")
            text = yaml.safe_dump(deep_merge(yaml.safe_load(text), overrides), sort_keys=False)
        return srv.start(text)
    except BaseException:
        dispose_server(srv)
        raise


def dispose_server(srv: Server) -> None:
    srv.stop()
    if os.getenv("MINI_CHAT_DEV_KEEP") == "1":
        print(f"\nmini_chat_dev: kept server home {srv.home}")
    else:
        shutil.rmtree(srv.home, ignore_errors=True)


@pytest.fixture(params=sorted(PROVIDERS))
def provider(request: pytest.FixtureRequest) -> dict:
    """Provider-parametrized scenarios: ``{"name", "model", "chat_path", "storage_prefix", "query"}``."""
    return {"name": request.param, **PROVIDERS[request.param]}


@pytest.fixture(scope="session")
def mock_llm() -> Iterator[MockLLM]:
    mock = MockLLM().start()
    yield mock
    mock.stop()


@pytest.fixture(autouse=True)
def _reset_mock_scripts(request: pytest.FixtureRequest) -> Iterator[None]:
    yield
    if "mock_llm" in request.fixturenames:
        request.getfixturevalue("mock_llm").reset_scripts()


@pytest.fixture(scope="session")
def server(mock_llm: MockLLM) -> Iterator[Server]:
    srv = new_server(mock_llm)
    yield srv
    dispose_server(srv)


# ── HTTP helpers ───────────────────────────────────────────────────────────


def parse_sse(text: str) -> list[tuple[str, Any]]:
    """Parse an SSE body into ``(event, data)`` pairs (``data`` JSON-decoded)."""
    events: list[tuple[str, Any]] = []
    for block in text.replace("\r\n", "\n").split("\n\n"):
        name, data_lines = None, []
        for line in block.split("\n"):
            if not line or line.startswith(":"):
                continue
            key, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if key == "event":
                name = value
            elif key == "data":
                data_lines.append(value)
        if name is None and not data_lines:
            continue
        raw = "\n".join(data_lines)
        try:
            data = json.loads(raw) if raw else None
        except ValueError:
            data = raw
        events.append((name or "message", data))
    return events


@dataclass
class StreamResult:
    status: int
    headers: httpx.Headers
    body: str
    events: list[tuple[str, Any]] = field(default_factory=list)

    @property
    def names(self) -> list[str]:
        return [n for n, _ in self.events]

    def all(self, name: str) -> list[Any]:
        return [d for n, d in self.events if n == name]

    def first(self, name: str) -> Any:
        found = self.all(name)
        return found[0] if found else None

    @property
    def problem(self) -> dict:
        return json.loads(self.body)

    @property
    def started(self) -> dict:
        return self.first("stream_started")

    @property
    def request_id(self) -> str:
        return self.started["request_id"]

    @property
    def text(self) -> str:
        return "".join(d["content"] for d in self.all("delta") if d.get("type") == "text")

    @property
    def done(self) -> Optional[dict]:
        return self.first("done")

    @property
    def error(self) -> Optional[dict]:
        return self.first("error")


class LiveStream:
    """An open SSE response read event by event (for running-turn scenarios)."""

    def __init__(self, client: httpx.Client, method: str, path: str, body: Optional[dict]):
        self._cm = client.stream(method, path, json=body, timeout=httpx.Timeout(60, read=60))
        self.response = self._cm.__enter__()
        self.status = self.response.status_code
        self._lines = self.response.iter_lines()
        self.events: list[tuple[str, Any]] = []

    def next_event(self) -> Optional[tuple[str, Any]]:
        block: list[str] = []
        for line in self._lines:
            if line == "":
                if block:
                    parsed = parse_sse("\n".join(block))
                    block = []
                    if parsed:
                        self.events.append(parsed[0])
                        return parsed[0]
                continue
            block.append(line)
        if block:
            parsed = parse_sse("\n".join(block))
            if parsed:
                self.events.append(parsed[0])
                return parsed[0]
        return None

    def read_until(self, name: str) -> list[tuple[str, Any]]:
        out = []
        while True:
            ev = self.next_event()
            if ev is None:
                raise AssertionError(f"stream ended before {name}: {self.events}")
            out.append(ev)
            if ev[0] == name:
                return out

    def read_all(self) -> list[tuple[str, Any]]:
        while self.next_event() is not None:
            pass
        return self.events

    def close(self) -> None:
        self._cm.__exit__(None, None, None)


class Api:
    def __init__(self, base_url: str, token: Optional[str]):
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        self.client = httpx.Client(base_url=base_url + PREFIX, headers=headers, timeout=60)

    def __getattr__(self, name: str) -> Any:  # get/post/patch/put/delete/request/stream
        return getattr(self.client, name)

    def create_chat(self, model: Optional[str] = None, title: Optional[str] = None) -> dict:
        body: dict[str, Any] = {}
        if model is not None:
            body["model"] = model
        if title is not None:
            body["title"] = title
        r = self.client.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def _sse(self, method: str, path: str, body: Optional[dict]) -> StreamResult:
        kwargs: dict[str, Any] = {}
        if body is not None:
            kwargs["json"] = body
        r = self.client.request(method, path, **kwargs)
        res = StreamResult(r.status_code, r.headers, r.text)
        if r.status_code == 200 and r.headers.get("content-type", "").startswith("text/event-stream"):
            res.events = parse_sse(r.text)
        return res

    def stream(self, chat_id: str, content: str = "Hello there", **extra: Any) -> StreamResult:
        return self._sse("POST", f"/chats/{chat_id}/messages:stream", {"content": content, **extra})

    def send(self, chat_id: str, content: str = "Hello there", **extra: Any) -> StreamResult:
        res = self.stream(chat_id, content, **extra)
        assert res.status == 200, res.body
        assert res.names[-1] == "done", res.events
        return res

    def open_stream(self, chat_id: str, content: str = "Hello there", **extra: Any) -> LiveStream:
        return LiveStream(self.client, "POST", f"/chats/{chat_id}/messages:stream", {"content": content, **extra})

    def retry(self, chat_id: str, request_id: str) -> StreamResult:
        return self._sse("POST", f"/chats/{chat_id}/turns/{request_id}/retry", None)

    def edit(self, chat_id: str, request_id: str, content: str) -> StreamResult:
        return self._sse("PATCH", f"/chats/{chat_id}/turns/{request_id}", {"content": content})

    def messages(self, chat_id: str, **params: Any) -> list[dict]:
        r = self.client.get(f"/chats/{chat_id}/messages", params={"limit": 100, **params})
        assert r.status_code == 200, r.text
        return r.json()["items"]

    def turn(self, chat_id: str, request_id: str) -> httpx.Response:
        return self.client.get(f"/chats/{chat_id}/turns/{request_id}")

    def wait_turn(self, chat_id: str, request_id: str, states: tuple[str, ...], timeout: float = 15.0) -> dict:
        deadline = time.monotonic() + timeout
        body: dict = {}
        while time.monotonic() < deadline:
            r = self.turn(chat_id, request_id)
            if r.status_code == 200:
                body = r.json()
                if body["state"] in states:
                    return body
            time.sleep(0.1)
        raise AssertionError(f"turn {request_id} not in {states}: {body}")

    def upload(self, chat_id: str, filename: str, data: bytes, content_type: str) -> httpx.Response:
        return self.client.post(
            f"/chats/{chat_id}/attachments", files={"file": (filename, data, content_type)}
        )


@pytest.fixture
def api(server: Server) -> Iterator[Any]:
    clients: list[Api] = []

    def make(user: Optional[str] = "A") -> Api:
        token = USERS[user]["token"] if user else None
        a = Api(server.base_url, token)
        clients.append(a)
        return a

    yield make
    for c in clients:
        c.client.close()


# ── DB helpers ─────────────────────────────────────────────────────────────


def ub(value: str) -> bytes:
    """UUID text -> 16-byte BLOB as stored by the gear."""
    return uuid.UUID(str(value)).bytes


def us(blob: bytes) -> str:
    return str(uuid.UUID(bytes=bytes(blob)))


@pytest.fixture
def db(server: Server) -> Iterator[sqlite3.Connection]:
    conn = sqlite3.connect(str(server.db_path), timeout=30, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA busy_timeout = 30000")
    yield conn
    conn.close()


def wait_until(pred, timeout: float = 15.0, interval: float = 0.2):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        v = pred()
        if v:
            return v
        time.sleep(interval)
    return pred()


def outbox_payloads(conn: sqlite3.Connection, queue: Optional[str] = None) -> list[dict]:
    """Outbox messages as ``{"queue", "payload"}`` (payload JSON-decoded), oldest first."""
    rows = conn.execute(
        """
        SELECT b.id AS id, b.payload AS payload, p.queue AS queue
        FROM toolkit_outbox_body b
        LEFT JOIN toolkit_outbox_incoming i ON i.body_id = b.id
        LEFT JOIN toolkit_outbox_outgoing o ON o.body_id = b.id
        LEFT JOIN toolkit_outbox_partitions p
               ON p.id = COALESCE(i.partition_id, o.partition_id)
        ORDER BY b.id
        """
    ).fetchall()
    out = []
    for r in rows:
        try:
            payload = json.loads(bytes(r["payload"]))
        except ValueError:
            continue
        if queue is None or r["queue"] == queue:
            out.append({"id": r["id"], "queue": r["queue"], "payload": payload})
    return out


def quota_rows(conn: sqlite3.Connection, user: str) -> dict[tuple[str, str], dict]:
    """``quota_usage`` rows of a user keyed by ``(period_type, bucket)`` (current periods)."""
    u = USERS[user]
    rows = conn.execute(
        "SELECT * FROM quota_usage WHERE tenant_id = ? AND user_id = ? ORDER BY period_start",
        (ub(u["tenant"]), ub(u["id"])),
    ).fetchall()
    return {(r["period_type"], r["bucket"]): dict(r) for r in rows}


def assert_problem(r: httpx.Response, status: int, **expect: Any) -> dict:
    """Assert a canonical RFC 9457 Problem with the given status."""
    assert r.status_code == status, f"{r.status_code} {r.text}"
    assert r.headers.get("content-type", "").startswith("application/problem+json"), r.headers
    p = r.json()
    for k in ("type", "title", "status", "detail", "context"):
        assert k in p, p
    assert p["status"] == status
    assert "code" not in p
    for k, v in expect.items():
        assert p.get(k) == v, (k, p)
    return p


def field_reasons(problem: dict) -> list[str]:
    return [v.get("reason") for v in problem["context"].get("field_violations", [])]
