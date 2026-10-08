"""Black-box harness for the mini-chat E2E tests.

Starts the OpenAI-compatible mock provider and the example server with a
generated configuration, and offers small HTTP/SSE helpers. Processes are
stopped by the pid recorded at start (never by command-line matching).
"""

from __future__ import annotations

import copy
import json
import os
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Iterator

import httpx
import yaml

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3].parent
SERVER_BIN = Path(os.environ.get("MINI_CHAT_SERVER_BIN", REPO / "target" / "debug" / "cf-gears-example-server"))

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
TENANT_B = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
USER_A = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_A2 = "44444444-6a88-4768-9dfc-6bcd5187d9ed"
USER_B = "22222222-6a88-4768-9dfc-6bcd5187d9ed"
TOKEN_A = "token-user-a"
TOKEN_A2 = "token-user-a2"
TOKEN_B = "token-user-b"

PREFIX = "/mini-chat/v1"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def model_entry(mid: str, **over: Any) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "id": mid,
        "provider_model_id": f"prov-{mid}",
        "display_name": mid.upper(),
        "description": f"{mid} model",
        "provider_id": "mock",
        "provider_display_name": "Mock",
        "icon": "",
        "tier": "standard",
        "enabled": True,
        "system_prompt": f"SYSTEM PROMPT {mid}",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50,
        },
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.5, "stop": []},
            "features": {"streaming": True, "structured_output": False},
            "tool_support": {
                "web_search": True,
                "file_search": True,
                "image_generation": False,
                "code_interpreter": True,
                "mcp": False,
            },
            "supported_endpoints": {"responses": True},
        },
        "preference": {"is_default": False, "sort_order": 0},
    }
    for k, v in over.items():
        if isinstance(v, dict) and isinstance(entry.get(k), dict):
            entry[k] = deep_merge(entry[k], v)
        else:
            entry[k] = v
    return entry


def deep_merge(a: dict[str, Any], b: dict[str, Any]) -> dict[str, Any]:
    out = copy.deepcopy(a)
    for k, v in b.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = copy.deepcopy(v)
    return out


def default_catalog() -> list[dict[str, Any]]:
    return [
        model_entry(
            "premium-1",
            tier="premium",
            multiplier_display="3x",
            input_tokens_credit_multiplier_micro=3_000_000,
            output_tokens_credit_multiplier_micro=15_000_000,
            preference={"is_default": True, "sort_order": 0},
        ),
        model_entry("gpt-4.1-mini", description=""),
        model_entry(
            "std-novision",
            multimodal_capabilities=[],
            general_config={"tool_support": {"web_search": False, "file_search": False, "code_interpreter": False}},
        ),
        model_entry(
            "tiny-ctx",
            context_window=3000,
            max_input_tokens=2500,
            max_output_tokens=500,
            estimation_budgets={"fixed_overhead_tokens": 10, "safety_margin_pct": 0},
        ),
        model_entry("disabled-1", enabled=False),
    ]


def build_config(home: Path, api_port: int, mock_port: int, overrides: dict[str, Any] | None = None) -> dict[str, Any]:
    cfg: dict[str, Any] = {
        "server": {"home_dir": str(home)},
        "database": {
            "servers": {
                "sqlite_mc": {
                    "engine": "sqlite",
                    "params": {"WAL": "true", "synchronous": "NORMAL", "busy_timeout": "5000"},
                    "pool": {"max_conns": 5, "acquire_timeout": "30s"},
                }
            }
        },
        "logging": {
            "default": {"console_level": "info", "file": "logs/server.log", "file_level": "info"},
            "mini_chat": {"console_level": "debug", "file": "logs/mini-chat.log", "file_level": "debug"},
        },
        "gears": {
            "api-gateway": {
                "config": {
                    "bind_addr": f"127.0.0.1:{api_port}",
                    "enable_docs": True,
                    "cors_enabled": False,
                    "openapi": {"title": "mini-chat e2e", "version": "0.1.0", "description": "e2e"},
                    "defaults": {"body_limit_bytes": 64000000},
                    "auth_disabled": False,
                    "require_auth_by_default": True,
                    "rate_limit_zones": {
                        "rl_mini_chat_chat": {
                            "rate_limit": "1000/s",
                            "burst_limit": 1000,
                            "response_status_code": 429,
                            "response_retry_after": "auto",
                            "key": {"type": "ip"},
                            "max_keys": 10000,
                        }
                    },
                    "in_flight_limit_zones": {
                        "ifl_mini_chat_chat": {
                            "in_flight_limit": 100,
                            "backlog_limit": 0,
                            "backlog_timeout": "0s",
                            "response_status_code": 429,
                            "key": {"type": "ip"},
                            "max_keys": 10000,
                        }
                    },
                }
            },
            "grpc-hub": {"config": {"listen_addr": f"uds://{home}/grpc.sock"}},
            "authn-resolver": {"config": {"vendor": "constructorfabric"}},
            "authz-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authn-plugin": {
                "config": {
                    "vendor": "constructorfabric",
                    "priority": 100,
                    "mode": "static_tokens",
                    "tokens": [
                        {"token": TOKEN_A, "identity": {"subject_id": USER_A, "subject_tenant_id": TENANT_A, "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
                        {"token": TOKEN_A2, "identity": {"subject_id": USER_A2, "subject_tenant_id": TENANT_A, "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
                        {"token": TOKEN_B, "identity": {"subject_id": USER_B, "subject_tenant_id": TENANT_B, "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
                    ],
                    "s2s_credentials": [{"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"}],
                }
            },
            "static-authz-plugin": {"config": {"vendor": "constructorfabric", "priority": 100}},
            "tenant-resolver": {"config": {"vendor": "constructorfabric"}},
            "single-tenant-tr-plugin": {"config": {"vendor": "constructorfabric"}},
            "credstore": {"database": {"server": "sqlite_mc", "file": "credstore.db"}, "config": {"vendor": "constructorfabric"}},
            "static-credstore-plugin": {"config": {"secrets": []}},
            "mini-chat": {
                "database": {"server": "sqlite_mc", "file": "mini_chat.db"},
                "config": {
                    "vendor": "constructorfabric",
                    "client_credentials": {"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"},
                    "streaming": {"sse_ping_interval_seconds": 5},
                    "orphan_watchdog": {"enabled": True, "scan_interval_secs": 1, "timeout_secs": 90},
                    "upload_reaper": {"enabled": True, "scan_interval_secs": 1, "stale_after_secs": 60},
                    "thread_summary_worker": {"enabled": True, "summary_model_id": "gpt-4.1-mini"},
                    "providers": {
                        "mock": {
                            "kind": "openai_responses",
                            "host": "127.0.0.1",
                            "port": mock_port,
                            "use_http": True,
                            "storage_kind": "openai",
                        }
                    },
                },
            },
            "static-mini-chat-audit-plugin": {"config": {"vendor": "constructorfabric", "priority": 100, "enabled": True}},
            "static-mini-chat-model-policy-plugin": {
                "config": {"vendor": "constructorfabric", "priority": 100, "model_catalog": default_catalog()}
            },
            "oagw": {
                "config": {
                    "proxy_timeout_secs": 30,
                    "allow_http_upstream": True,
                    "ssrf_policy": {"enabled": False},
                }
            },
        },
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
    }
    if overrides:
        cfg = deep_merge(cfg, overrides)
    return cfg


def wait_http(url: str, timeout: float, proc: subprocess.Popen[bytes] | None = None) -> None:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"process exited early with {proc.returncode}")
        try:
            r = httpx.get(url, timeout=1.0)
            if r.status_code < 500:
                return
        except Exception as e:  # noqa: BLE001
            last = e
        time.sleep(0.2)
    raise RuntimeError(f"{url} not ready: {last}")


class Mock:
    def __init__(self) -> None:
        self.port = free_port()
        self.pid_file = Path(tempfile.mkstemp(prefix="mc-mock-", suffix=".pid")[1])
        self.log = Path(tempfile.mkstemp(prefix="mc-mock-", suffix=".log")[1])
        self.proc: subprocess.Popen[bytes] | None = None
        self.base = f"http://127.0.0.1:{self.port}"

    def start(self) -> None:
        self.proc = subprocess.Popen(
            [sys.executable, str(HERE / "mock_provider.py"), "--port", str(self.port)],
            stdout=self.log.open("wb"),
            stderr=subprocess.STDOUT,
        )
        self.pid_file.write_text(str(self.proc.pid))
        wait_http(self.base + "/__mock/requests", 20, self.proc)

    def stop(self) -> None:
        stop_pid(self.pid_file, self.proc)

    # control API
    def reset(self) -> None:
        httpx.post(self.base + "/__mock/reset").raise_for_status()

    def script(self, items: list[dict[str, Any]], path_suffix: str = "responses") -> None:
        httpx.post(self.base + "/__mock/script", json={"path_suffix": path_suffix, "items": items}).raise_for_status()

    def config(self, **kw: Any) -> None:
        httpx.post(self.base + "/__mock/config", json=kw).raise_for_status()

    def requests(self, path_suffix: str | None = None, method: str | None = None) -> list[dict[str, Any]]:
        reqs = httpx.get(self.base + "/__mock/requests").json()
        if path_suffix is not None:
            reqs = [r for r in reqs if r["path"].endswith(path_suffix)]
        if method is not None:
            reqs = [r for r in reqs if r["method"] == method]
        return reqs

    def chat_requests(self) -> list[dict[str, Any]]:
        return [r for r in self.requests("/responses") if r.get("json", {}).get("stream")]

    def state(self) -> dict[str, Any]:
        return httpx.get(self.base + "/__mock/state").json()


def stop_pid(pid_file: Path, proc: subprocess.Popen[bytes] | None) -> None:
    try:
        pid = int(pid_file.read_text().strip())
    except Exception:  # noqa: BLE001
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    if proc is not None:
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            os.kill(pid, signal.SIGKILL)
            proc.wait(timeout=10)


class Server:
    def __init__(self, mock: Mock, overrides: dict[str, Any] | None = None) -> None:
        self.mock = mock
        self.home = Path(tempfile.mkdtemp(prefix="mc-e2e-"))
        self.api_port = free_port()
        self.base = f"http://127.0.0.1:{self.api_port}"
        self.cfg = build_config(self.home, self.api_port, mock.port, overrides)
        self.cfg_path = self.home / "config.yaml"
        self.cfg_path.write_text(yaml.safe_dump(self.cfg, sort_keys=False))
        self.pid_file = self.home / "server.pid"
        self.log = self.home / "server.log"
        self.proc: subprocess.Popen[bytes] | None = None

    def start(self) -> None:
        if not SERVER_BIN.exists():
            raise RuntimeError(f"server binary missing: {SERVER_BIN}")
        self.proc = subprocess.Popen(
            [str(SERVER_BIN), "--config", str(self.cfg_path), "run"],
            stdout=self.log.open("wb"),
            stderr=subprocess.STDOUT,
            cwd=str(self.home),
        )
        self.pid_file.write_text(str(self.proc.pid))
        try:
            wait_http(self.base + "/health", 120, self.proc)
            # Wait until the gear routes answer (gateway may come up before start completes).
            deadline = time.time() + 60
            while time.time() < deadline:
                r = httpx.get(self.base + PREFIX + "/models", headers=auth(TOKEN_A), timeout=5)
                if r.status_code == 200:
                    break
                time.sleep(0.3)
            # Wait for OAGW provisioning: a chat route must resolve.
            self._wait_provisioned()
        except Exception:
            self.stop()
            raise

    def _wait_provisioned(self) -> None:
        deadline = time.time() + 30
        while time.time() < deadline:
            if "OAGW upstream provisioned" in self.log_text():
                return
            time.sleep(0.2)

    def stop(self) -> None:
        stop_pid(self.pid_file, self.proc)

    def cleanup(self) -> None:
        shutil.rmtree(self.home, ignore_errors=True)

    # DB access (read-only inspection of the gear database)
    def db_path(self) -> Path:
        for p in self.home.rglob("mini_chat.db"):
            return p
        raise FileNotFoundError("mini_chat.db")

    def query(self, sql: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:
        con = sqlite3.connect(f"file:{self.db_path()}?mode=ro", uri=True, timeout=10)
        con.row_factory = sqlite3.Row
        try:
            return [dict(r) for r in con.execute(sql, params).fetchall()]
        finally:
            con.close()

    def execute(self, sql: str, params: tuple[Any, ...] = ()) -> None:
        con = sqlite3.connect(str(self.db_path()), timeout=10)
        try:
            con.execute(sql, params)
            con.commit()
        finally:
            con.close()

    def log_text(self) -> str:
        try:
            return self.log.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def client(self, token: str = TOKEN_A) -> "Client":
        return Client(self, token)


def auth(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


def ub(u: str | uuid.UUID) -> bytes:
    """UUID as the 16-byte BLOB stored by the gear."""
    return uuid.UUID(str(u)).bytes


def from_blob(b: bytes | str | None) -> str | None:
    if b is None:
        return None
    if isinstance(b, (bytes, bytearray)):
        return str(uuid.UUID(bytes=bytes(b)))
    return b


class SseEvent:
    def __init__(self, event: str, data: Any) -> None:
        self.event = event
        self.data = data

    def __repr__(self) -> str:
        return f"SseEvent({self.event}, {self.data})"


def parse_sse(lines: Iterator[str]) -> Iterator[SseEvent]:
    event = None
    data: list[str] = []
    for line in lines:
        if line == "":
            if event is not None or data:
                raw = "\n".join(data)
                try:
                    payload = json.loads(raw) if raw else None
                except json.JSONDecodeError:
                    payload = raw
                yield SseEvent(event or "message", payload)
            event, data = None, []
            continue
        if line.startswith(":"):
            continue
        if line.startswith("event:"):
            event = line[6:].strip()
        elif line.startswith("data:"):
            data.append(line[5:].lstrip(" "))


class StreamResult:
    def __init__(self, status: int, headers: dict[str, str], events: list[SseEvent], body: Any = None) -> None:
        self.status = status
        self.headers = headers
        self.events = events
        self.body = body

    @property
    def names(self) -> list[str]:
        return [e.event for e in self.events]

    def first(self, name: str) -> SseEvent:
        for e in self.events:
            if e.event == name:
                return e
        raise AssertionError(f"no {name} event in {self.names}")

    def all(self, name: str) -> list[SseEvent]:
        return [e for e in self.events if e.event == name]

    @property
    def text(self) -> str:
        return "".join(e.data["content"] for e in self.events if e.event == "delta" and e.data.get("type") == "text")

    @property
    def started(self) -> dict[str, Any]:
        return self.first("stream_started").data

    @property
    def done(self) -> dict[str, Any]:
        return self.first("done").data


class Client:
    def __init__(self, server: Server, token: str) -> None:
        self.server = server
        self.token = token
        self.http = httpx.Client(base_url=server.base, headers=auth(token), timeout=60)

    def url(self, path: str) -> str:
        return PREFIX + path

    def get(self, path: str, **kw: Any) -> httpx.Response:
        return self.http.get(self.url(path), **kw)

    def post(self, path: str, **kw: Any) -> httpx.Response:
        return self.http.post(self.url(path), **kw)

    def patch(self, path: str, **kw: Any) -> httpx.Response:
        return self.http.patch(self.url(path), **kw)

    def put(self, path: str, **kw: Any) -> httpx.Response:
        return self.http.put(self.url(path), **kw)

    def delete(self, path: str, **kw: Any) -> httpx.Response:
        return self.http.delete(self.url(path), **kw)

    # helpers
    def create_chat(self, **body: Any) -> dict[str, Any]:
        r = self.post("/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def stream_raw(self, method: str, path: str, json_body: Any = None, max_events: int | None = None,
                   stop_after: str | None = None, timeout: float = 60) -> StreamResult:
        with self.http.stream(method, self.url(path), json=json_body, timeout=timeout) as r:
            headers = dict(r.headers)
            if not r.headers.get("content-type", "").startswith("text/event-stream"):
                r.read()
                try:
                    body = r.json()
                except Exception:  # noqa: BLE001
                    body = r.text
                return StreamResult(r.status_code, headers, [], body)
            events: list[SseEvent] = []
            for ev in parse_sse(r.iter_lines()):
                events.append(ev)
                if stop_after is not None and ev.event == stop_after:
                    break
                if max_events is not None and len(events) >= max_events:
                    break
            return StreamResult(r.status_code, headers, events)

    def send(self, chat_id: str, content: str, **extra: Any) -> StreamResult:
        body = {"content": content, **extra}
        return self.stream_raw("POST", f"/chats/{chat_id}/messages:stream", body)

    def retry(self, chat_id: str, request_id: str) -> StreamResult:
        return self.stream_raw("POST", f"/chats/{chat_id}/turns/{request_id}/retry")

    def edit(self, chat_id: str, request_id: str, content: str) -> StreamResult:
        return self.stream_raw("PATCH", f"/chats/{chat_id}/turns/{request_id}", {"content": content})

    def messages(self, chat_id: str, **params: Any) -> list[dict[str, Any]]:
        r = self.get(f"/chats/{chat_id}/messages", params={"limit": 100, **params})
        assert r.status_code == 200, r.text
        return r.json()["items"]

    def turn(self, chat_id: str, request_id: str) -> httpx.Response:
        return self.get(f"/chats/{chat_id}/turns/{request_id}")

    def upload(self, chat_id: str, filename: str, data: bytes, content_type: str) -> httpx.Response:
        return self.post(f"/chats/{chat_id}/attachments", files={"file": (filename, data, content_type)})

    def quota(self) -> dict[str, Any]:
        r = self.get("/quota/status")
        assert r.status_code == 200, r.text
        return r.json()


def wait_until(fn: Any, timeout: float = 15, interval: float = 0.2, msg: str = "condition") -> Any:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {msg}; last={last!r}")


def problem_reason(body: dict[str, Any]) -> str | None:
    ctx = body.get("context") or {}
    if "field_violations" in ctx:
        return ctx["field_violations"][0].get("reason")
    if "violations" in ctx:
        v = ctx["violations"][0]
        return v.get("type") or v.get("description")
    return ctx.get("reason") or ctx.get("resource_name")


class BackgroundStream:
    """Runs a streaming request on a raw socket in a thread; exposes events
    as they arrive. `disconnect()` closes the TCP connection (client drop)."""

    def __init__(self, client: "Client", method: str, path: str, body: Any = None, disconnect_after: str | None = None) -> None:
        import threading

        self.client = client
        self.events: list[SseEvent] = []
        self.status: int | None = None
        self.body: Any = None
        self.error: Exception | None = None
        self.finished = False
        self._disconnect_after = disconnect_after
        self._sock: socket.socket | None = None
        self._thread = threading.Thread(target=self._run, args=(method, path, body), daemon=True)
        self._thread.start()

    def _run(self, method: str, path: str, body: Any) -> None:
        try:
            payload = json.dumps(body).encode() if body is not None else b""
            sock = socket.create_connection(("127.0.0.1", self.client.server.api_port))
            self._sock = sock
            head = (
                f"{method} {self.client.url(path)} HTTP/1.1\r\nHost: 127.0.0.1\r\n"
                f"Authorization: Bearer {self.client.token}\r\nAccept: text/event-stream\r\n"
                f"Content-Type: application/json\r\nContent-Length: {len(payload)}\r\n\r\n"
            ).encode()
            sock.sendall(head + payload)
            buf = b""
            while b"\r\n\r\n" not in buf:
                chunk = sock.recv(65536)
                if not chunk:
                    raise RuntimeError("connection closed before headers")
                buf += chunk
            header_blob, rest = buf.split(b"\r\n\r\n", 1)
            lines = header_blob.decode().split("\r\n")
            self.status = int(lines[0].split()[1])
            headers = {k.strip().lower(): v.strip() for k, v in (l.split(":", 1) for l in lines[1:] if ":" in l)}
            chunked = headers.get("transfer-encoding", "").lower() == "chunked"
            raw = rest
            data = b""
            text = ""
            while True:
                # de-chunk what we have
                if chunked:
                    while True:
                        if b"\r\n" not in raw:
                            break
                        size_line, after = raw.split(b"\r\n", 1)
                        size = int(size_line.split(b";")[0] or b"0", 16)
                        if size == 0:
                            self._consume(text + data.decode(errors="replace"), final=True)
                            self.finished = True
                            return
                        if len(after) < size + 2:
                            break
                        data += after[:size]
                        raw = after[size + 2 :]
                else:
                    data += raw
                    raw = b""
                if not headers.get("content-type", "").startswith("text/event-stream"):
                    if not chunked and len(data) >= int(headers.get("content-length", "0")):
                        self.body = json.loads(data or b"null")
                        self.finished = True
                        return
                else:
                    text += data.decode(errors="replace")
                    data = b""
                    text = self._consume(text)
                    if self._disconnect_after is not None and any(e.event == self._disconnect_after for e in self.events):
                        self.disconnect()
                        return
                chunk = sock.recv(65536)
                if not chunk:
                    self.finished = True
                    if not headers.get("content-type", "").startswith("text/event-stream") and data:
                        self.body = json.loads(data)
                    return
                raw += chunk
        except OSError:
            return
        except Exception as e:  # noqa: BLE001
            self.error = e

    def _consume(self, text: str, final: bool = False) -> str:
        while "\n\n" in text:
            block, text = text.split("\n\n", 1)
            for ev in parse_sse(iter(block.split("\n") + [""])):
                self.events.append(ev)
        return text

    def wait_event(self, name: str, timeout: float = 20) -> SseEvent:
        def find() -> SseEvent | None:
            for e in self.events:
                if e.event == name:
                    return e
            return None

        return wait_until(find, timeout, msg=f"event {name}")

    def disconnect(self) -> None:
        if self._sock is not None:
            try:
                self._sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self._sock.close()

    def join(self, timeout: float = 60) -> None:
        self._thread.join(timeout)

    @property
    def names(self) -> list[str]:
        return [e.event for e in self.events]
