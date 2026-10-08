"""Black-box harness for the mini-chat gear.

Starts an OpenAI-compatible mock provider and the real ``cf-gears-example-server``
debug binary with a generated configuration (JSON is valid YAML), and provides an
HTTP/SSE client plus direct access to the gear's SQLite database.

The server process is tracked by pid (never by command-line matching).
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

import httpx

from mock_provider import MockProviderServer

REPO_ROOT = Path(__file__).resolve().parents[3]
BINARY = REPO_ROOT / "target" / "debug" / "cf-gears-example-server"

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
TENANT_B = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
USER_A1 = "11111111-6a88-4768-9dfc-6bcd5187d9ed"
USER_A2 = "44444444-6a88-4768-9dfc-6bcd5187d9ed"
USER_B = "22222222-6a88-4768-9dfc-6bcd5187d9ed"

TOKENS = {
    "a1": ("tok-a1", USER_A1, TENANT_A),
    "a2": ("tok-a2", USER_A2, TENANT_A),
    "b": ("tok-b", USER_B, TENANT_B),
}


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def catalog_entry(
    model_id: str,
    tier: str = "Standard",
    *,
    enabled: bool = True,
    vision: bool = True,
    web_search: bool = True,
    file_search: bool = True,
    code_interpreter: bool = True,
    context_window: int = 1047576,
    max_output_tokens: int = 32768,
    max_input_tokens: int = 1047576,
    in_mult: int = 1_000_000,
    out_mult: int = 3_000_000,
    is_default: bool = False,
    provider_model_id: str | None = None,
    system_prompt: str = "You are a helpful assistant.",
) -> dict:
    return {
        "id": model_id,
        "provider_model_id": provider_model_id or model_id,
        "display_name": model_id.upper(),
        "description": f"{model_id} description",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": system_prompt,
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"] if vision else [],
        "context_window": context_window,
        "max_output_tokens": max_output_tokens,
        "max_input_tokens": max_input_tokens,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult,
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
        "max_tool_calls": 10,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "stop": []},
            "features": {"streaming": True, "structured_output": True},
            "tool_support": {
                "web_search": web_search,
                "file_search": file_search,
                "image_generation": False,
                "code_interpreter": code_interpreter,
                "mcp": False,
            },
            "supported_endpoints": {"responses": True},
        },
        "preference": {"is_default": is_default, "sort_order": 0},
    }


def default_catalog() -> list[dict]:
    return [
        catalog_entry("gpt-4.1", "Premium", is_default=True, in_mult=3_000_000, out_mult=15_000_000),
        catalog_entry("gpt-4.1-mini", "Standard"),
        catalog_entry("std-novision", "Standard", vision=False, web_search=False, file_search=False, code_interpreter=False),
        catalog_entry("disabled-model", "Standard", enabled=False),
        catalog_entry(
            "tiny-ctx", "Standard", context_window=4096, max_output_tokens=1024, max_input_tokens=3072,
            web_search=False, file_search=False, code_interpreter=False,
        ),
    ]


@dataclass
class ServerOptions:
    catalog: list[dict] = field(default_factory=default_catalog)
    kill_switches: dict = field(default_factory=dict)
    gear_overrides: dict = field(default_factory=dict)
    standard_limits: dict | None = None
    premium_limits: dict | None = None


def deep_merge(base: dict, extra: dict) -> dict:
    out = dict(base)
    for k, v in extra.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = v
    return out


def build_config(home: Path, port: int, mock_port: int, opts: ServerOptions) -> dict:
    gear_cfg: dict[str, Any] = {
        "vendor": "constructorfabric",
        "client_credentials": {"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"},
        "orphan_watchdog": {"enabled": True, "scan_interval_secs": 1, "timeout_secs": 90},
        "upload_reaper": {"enabled": True, "scan_interval_secs": 1, "stale_after_secs": 60},
        "thread_summary_worker": {"enabled": True, "summary_model_id": "gpt-4.1-mini"},
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "storage_kind": "openai",
                "host": "127.0.0.1",
                "port": mock_port,
                "use_http": True,
                "api_path": "/v1/responses",
            }
        },
    }
    gear_cfg = deep_merge(gear_cfg, opts.gear_overrides)
    policy: dict[str, Any] = {
        "vendor": "constructorfabric",
        "priority": 100,
        "model_catalog": opts.catalog,
        "kill_switches": opts.kill_switches,
    }
    if opts.standard_limits:
        policy["default_standard_limits"] = opts.standard_limits
    if opts.premium_limits:
        policy["default_premium_limits"] = opts.premium_limits
    return {
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
        "logging": {"default": {"console_level": "info", "file": "logs/server.log", "file_level": "debug"}},
        "gears": {
            "api-gateway": {
                "config": {
                    "bind_addr": f"127.0.0.1:{port}",
                    "enable_docs": False,
                    "cors_enabled": False,
                    "defaults": {"body_limit_bytes": 64000000},
                    "auth_disabled": False,
                }
            },
            "gear-orchestrator": {"config": {}},
            "grpc-hub": {"config": {"listen_addr": f"uds://{home}/grpc.sock"}},
            "authn-resolver": {"config": {"vendor": "constructorfabric"}},
            "authz-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authn-plugin": {
                "config": {
                    "vendor": "constructorfabric",
                    "priority": 100,
                    "mode": "static_tokens",
                    "tokens": [
                        {
                            "token": tok,
                            "identity": {
                                "subject_id": sub,
                                "subject_tenant_id": ten,
                                "subject_type": "gts.cf.core.security.subject_user.v1~",
                                "token_scopes": ["*"],
                            },
                        }
                        for tok, sub, ten in TOKENS.values()
                    ],
                    "s2s_credentials": [{"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"}],
                }
            },
            "static-authz-plugin": {"config": {"vendor": "constructorfabric", "priority": 100}},
            "tenant-resolver": {"config": {"vendor": "constructorfabric"}},
            "single-tenant-tr-plugin": {"config": {"vendor": "constructorfabric"}},
            "credstore": {
                "database": {"server": "sqlite_mc", "file": "credstore.db"},
                "config": {"vendor": "constructorfabric"},
            },
            "static-credstore-plugin": {"config": {"secrets": []}},
            "oagw": {
                "config": {
                    "proxy_timeout_secs": 10,
                    "allow_http_upstream": True,
                    "ssrf_policy": {"enabled": False},
                }
            },
            "mini-chat": {
                "database": {"server": "sqlite_mc", "file": "mini_chat.db"},
                "config": gear_cfg,
            },
            "static-mini-chat-audit-plugin": {"config": {"vendor": "constructorfabric", "priority": 100, "enabled": True}},
            "static-mini-chat-model-policy-plugin": {"config": policy},
        },
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
    }


class SseEvent:
    def __init__(self, event: str, data: Any) -> None:
        self.event = event
        self.data = data

    def __repr__(self) -> str:
        return f"SseEvent({self.event!r}, {self.data!r})"


def parse_sse(text: str) -> list[SseEvent]:
    events: list[SseEvent] = []
    for block in text.replace("\r\n", "\n").split("\n\n"):
        name = None
        data_lines: list[str] = []
        for line in block.split("\n"):
            if line.startswith(":") or not line:
                continue
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                data_lines.append(line[5:].lstrip())
        if name is None and not data_lines:
            continue
        raw = "\n".join(data_lines)
        try:
            data = json.loads(raw) if raw else None
        except json.JSONDecodeError:
            data = raw
        events.append(SseEvent(name or "message", data))
    return events


class MiniChat:
    """Running server + mock provider."""

    def __init__(self, opts: ServerOptions | None = None) -> None:
        self.opts = opts or ServerOptions()
        self.home = Path(tempfile.mkdtemp(prefix="mini-chat-e2e-"))
        self.port = free_port()
        self.mock = MockProviderServer()
        self.proc: subprocess.Popen | None = None
        self.base = f"http://127.0.0.1:{self.port}/mini-chat/v1"
        self.log_path = self.home / "stdout.log"

    # ── lifecycle ──────────────────────────────────────────────────────
    def start(self) -> "MiniChat":
        self.mock.start()
        cfg = build_config(self.home, self.port, self.mock.port, self.opts)
        cfg_path = self.home / "config.yaml"
        cfg_path.write_text(json.dumps(cfg, indent=2))
        log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [str(BINARY), "--config", str(cfg_path), "run"],
            cwd=str(REPO_ROOT),
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited early:\n{self.log_path.read_text()[-5000:]}")
            try:
                r = httpx.get(f"http://127.0.0.1:{self.port}/healthz", timeout=2)
                if r.status_code == 200:
                    r2 = httpx.get(f"{self.base}/models", headers=self.headers("a1"), timeout=2)
                    if r2.status_code != 404:
                        self._install_outbox_capture()
                        return self
            except httpx.HTTPError:
                pass
            time.sleep(0.5)
        raise RuntimeError(f"server not ready:\n{self.log_path.read_text()[-5000:]}")

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        self.mock.stop()

    # ── HTTP helpers ───────────────────────────────────────────────────
    @staticmethod
    def headers(user: str = "a1", extra: dict | None = None) -> dict:
        h = {"Authorization": f"Bearer {TOKENS[user][0]}"}
        if extra:
            h.update(extra)
        return h

    def req(self, method: str, path: str, user: str = "a1", **kw: Any) -> httpx.Response:
        headers = self.headers(user, kw.pop("headers", None))
        return httpx.request(method, f"{self.base}{path}", headers=headers, timeout=kw.pop("timeout", 60), **kw)

    def create_chat(self, user: str = "a1", **body: Any) -> dict:
        r = self.req("POST", "/chats", user, json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def stream(self, chat_id: str, content: str = "Hello", user: str = "a1", **body: Any) -> tuple[httpx.Response, list[SseEvent]]:
        payload = {"content": content, **body}
        r = self.req("POST", f"/chats/{chat_id}/messages:stream", user, json=payload, timeout=120)
        events = parse_sse(r.text) if r.headers.get("content-type", "").startswith("text/event-stream") else []
        return r, events

    def sse(self, method: str, path: str, user: str = "a1", **kw: Any) -> tuple[httpx.Response, list[SseEvent]]:
        r = self.req(method, path, user, timeout=120, **kw)
        events = parse_sse(r.text) if r.headers.get("content-type", "").startswith("text/event-stream") else []
        return r, events

    def stream_iter(self, chat_id: str, content: str = "Hello", user: str = "a1", **body: Any) -> Iterator[tuple[str, str]]:
        """Yields raw (kind, line) pairs while streaming; caller may break to disconnect."""
        payload = {"content": content, **body}
        with httpx.stream(
            "POST", f"{self.base}/chats/{chat_id}/messages:stream", headers=self.headers(user), json=payload, timeout=120
        ) as r:
            yield ("status", str(r.status_code))
            for line in r.iter_lines():
                yield ("line", line)

    # ── mock control ───────────────────────────────────────────────────
    def mock_url(self, path: str) -> str:
        return f"http://127.0.0.1:{self.mock.port}{path}"

    def mock_reset(self) -> None:
        httpx.post(self.mock_url("/__control/reset"))

    def mock_script(self, *scripts: dict) -> None:
        httpx.post(self.mock_url("/__control/responses"), json=list(scripts))

    def mock_config(self, **cfg: Any) -> None:
        httpx.post(self.mock_url("/__control/config"), json=cfg)

    def mock_requests(self, path_suffix: str | None = None, method: str | None = None) -> list[dict]:
        reqs = httpx.get(self.mock_url("/__control/requests")).json()
        out = []
        for r in reqs:
            if path_suffix and not r["path"].endswith(path_suffix):
                continue
            if method and r["method"] != method:
                continue
            out.append(r)
        return out

    def responses_requests(self) -> list[dict]:
        return [r for r in self.mock_requests("/responses", "POST")]

    def mock_stats(self) -> dict:
        return httpx.get(self.mock_url("/__control/stats")).json()

    def chat_requests(self) -> list[dict]:
        """Bodies of streaming (chat) Responses API requests received by the mock."""
        return [r["body"] for r in self.responses_requests() if isinstance(r["body"], dict) and r["body"].get("stream")]

    def summary_requests(self) -> list[dict]:
        """Bodies of non-streaming Responses API requests (thread summary) received by the mock."""
        return [r["body"] for r in self.responses_requests() if isinstance(r["body"], dict) and not r["body"].get("stream")]

    # ── incremental SSE ────────────────────────────────────────────────
    def sse_iter(
        self, method: str, path: str, user: str = "a1", json_body: Any = None, timeout: float = 60
    ) -> Iterator[tuple[float, Any]]:
        """Streams a request and yields ``(elapsed_secs, item)`` incrementally.

        The first item is the ``httpx.Response`` (status/headers available); following items are
        ``SseEvent`` objects as soon as each SSE block is complete. For non-SSE responses the body is
        read and the response is yielded once. Closing the generator closes the connection.
        """
        t0 = time.monotonic()
        kw: dict[str, Any] = {}
        if json_body is not None:
            kw["json"] = json_body
        with httpx.Client(timeout=timeout) as client:
            with client.stream(method, f"{self.base}{path}", headers=self.headers(user), **kw) as r:
                if not r.headers.get("content-type", "").startswith("text/event-stream"):
                    r.read()
                    yield (time.monotonic() - t0, r)
                    return
                yield (time.monotonic() - t0, r)
                block: list[str] = []
                for line in r.iter_lines():
                    if line == "":
                        if block:
                            evs = parse_sse("\n".join(block) + "\n\n")
                            block = []
                            for ev in evs:
                                yield (time.monotonic() - t0, ev)
                        continue
                    block.append(line)
                if block:
                    for ev in parse_sse("\n".join(block) + "\n\n"):
                        yield (time.monotonic() - t0, ev)

    def stream_events(self, chat_id: str, content: str = "Hello", user: str = "a1", **body: Any) -> Iterator[tuple[float, Any]]:
        return self.sse_iter("POST", f"/chats/{chat_id}/messages:stream", user, {"content": content, **body})

    # ── outbox capture ─────────────────────────────────────────────────
    def _install_outbox_capture(self) -> None:
        """Copies every outbox body (and its queue) into ``e2e_outbox_capture`` via SQLite triggers.

        The shared outbox vacuums processed bodies, so tests read the captured copy instead.
        """
        try:
            conn = self.db()
        except FileNotFoundError:
            return
        try:
            names = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if "toolkit_outbox_body" not in names:
                return
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS e2e_outbox_capture (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    body_id INTEGER,
                    queue TEXT,
                    payload BLOB,
                    payload_type TEXT
                );
                CREATE TRIGGER IF NOT EXISTS e2e_outbox_capture_body AFTER INSERT ON toolkit_outbox_body
                BEGIN
                    INSERT INTO e2e_outbox_capture(body_id, payload, payload_type)
                    VALUES (NEW.id, NEW.payload, NEW.payload_type);
                END;
                """
            )
            if "toolkit_outbox_incoming" in names and "toolkit_outbox_partitions" in names:
                conn.executescript(
                    """
                    CREATE TRIGGER IF NOT EXISTS e2e_outbox_capture_queue AFTER INSERT ON toolkit_outbox_incoming
                    BEGIN
                        UPDATE e2e_outbox_capture
                           SET queue = (SELECT queue FROM toolkit_outbox_partitions WHERE id = NEW.partition_id)
                         WHERE body_id = NEW.body_id AND queue IS NULL;
                    END;
                    """
                )
            conn.commit()
        finally:
            conn.close()

    def outbox_events(self, queue_contains: str | None = None) -> list[dict]:
        """Captured outbox messages: ``{id, queue, payload, payload_type}`` (payload JSON-decoded when possible)."""
        try:
            rows = self.query("SELECT id, queue, payload, payload_type FROM e2e_outbox_capture ORDER BY id")
        except sqlite3.OperationalError:
            return []
        out = []
        for r in rows:
            raw = r["payload"]
            if isinstance(raw, (bytes, bytearray)):
                try:
                    raw = raw.decode()
                except UnicodeDecodeError:
                    pass
            try:
                payload = json.loads(raw) if isinstance(raw, str) else raw
            except json.JSONDecodeError:
                payload = raw
            q = r["queue"] or ""
            if queue_contains and queue_contains not in q:
                continue
            out.append({"id": r["id"], "queue": q, "payload": payload, "payload_type": r["payload_type"]})
        return out

    def server_log(self) -> str:
        parts = []
        for p in [self.log_path, *self.home.rglob("server.log")]:
            try:
                parts.append(Path(p).read_text(errors="replace"))
            except OSError:
                pass
        return "\n".join(parts)

    # ── database ───────────────────────────────────────────────────────
    def db_path(self) -> Path:
        cands = list(self.home.rglob("mini_chat.db"))
        if not cands:
            raise FileNotFoundError(f"mini_chat.db not found under {self.home}")
        return cands[0]

    def db(self) -> sqlite3.Connection:
        conn = sqlite3.connect(str(self.db_path()), timeout=10)
        conn.row_factory = sqlite3.Row
        return conn

    def query(self, sql: str, params: tuple = ()) -> list[sqlite3.Row]:
        with self.db() as conn:
            return list(conn.execute(sql, params))

    def execute(self, sql: str, params: tuple = ()) -> None:
        conn = self.db()
        try:
            conn.execute(sql, params)
            conn.commit()
        finally:
            conn.close()


def ub(u: str) -> bytes:
    """UUID string → 16-byte BLOB as stored by the gear."""
    return uuid.UUID(u).bytes


def wait_until(pred, timeout: float = 15.0, interval: float = 0.2):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = pred()
        if last:
            return last
        time.sleep(interval)
    return last
