"""Test harness: config generation, process management and HTTP / SSE helpers
for the mini-chat e2e tests (mock provider + example server)."""

import json
import os
import signal
import socket
import sqlite3
import subprocess
import sys
import time
import uuid
from pathlib import Path

import httpx

ROOT = Path(__file__).resolve().parents[5]
SERVER_BIN = Path(os.environ.get("MC_SERVER_BIN", ROOT / "target" / "debug" / "cf-gears-example-server"))
HERE = Path(__file__).resolve().parent

TENANT_A = "00000000-0000-0000-0000-00000000000a"
TENANT_B = "00000000-0000-0000-0000-00000000000b"
USER_A1 = "00000000-0000-0000-0000-0000000000a1"
USER_A2 = "00000000-0000-0000-0000-0000000000a2"
USER_B1 = "00000000-0000-0000-0000-0000000000b1"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def model(mid, tier="Standard", enabled=True, vision=True, web=True, files=True, ci=True,
          default=False, context_window=128000, max_output=4096, max_input=0,
          in_mult=1_000_000, out_mult=3_000_000, provider="openai", provider_model_id=None):
    return {
        "id": mid,
        "provider_model_id": provider_model_id or mid,
        "display_name": mid.upper(),
        "description": f"{mid} description" if mid != "gpt-nodesc" else "",
        "provider_id": provider,
        "provider_display_name": "Mock",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": f"You are {mid}.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"] if vision else [],
        "context_window": context_window,
        "max_output_tokens": max_output,
        "max_input_tokens": max_input,
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
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "max_file_size_mb": 25,
            "api_params": {},
            "features": {"streaming": True, "structured_output": False},
            "tool_support": {
                "web_search": web,
                "file_search": files,
                "image_generation": False,
                "code_interpreter": ci,
                "mcp": False,
            },
            "supported_endpoints": {"chat_completions": True, "responses": True},
        },
        "preference": {"is_default": default, "sort_order": 0},
    }


def default_catalog():
    return [
        model("gpt-premium", tier="Premium", default=True, in_mult=3_000_000, out_mult=15_000_000),
        model("gpt-standard"),
        model("gpt-novision", vision=False, web=False, files=False, ci=False),
        model("gpt-disabled", enabled=False),
        model("gpt-4.1-mini"),
        model("gpt-tiny", context_window=2600, max_output=1024, max_input=1500),
        model("gpt-chat", provider="chatprov", web=False, files=False, ci=False),
    ]


def build_config(home, api_port, mock_port, *, catalog=None, kill_switches=None,
                 standard_limits=None, premium_limits=None, mini_chat_overrides=None):
    users = [
        ("token-a1", USER_A1, TENANT_A),
        ("token-a2", USER_A2, TENANT_A),
        ("token-b1", USER_B1, TENANT_B),
    ]
    mini_chat = {
        "vendor": "constructorfabric",
        "client_credentials": {"client_id": "mini-chat", "client_secret": "secret"},
        "streaming": {"sse_ping_interval_seconds": 5},
        "orphan_watchdog": {"enabled": True, "scan_interval_secs": 1, "timeout_secs": 90},
        "upload_reaper": {"enabled": True, "scan_interval_secs": 1, "stale_after_secs": 60},
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "host": "127.0.0.1",
                "port": mock_port,
                "use_http": True,
                "api_path": "/v1/responses",
                "storage_kind": "openai",
            },
            "chatprov": {
                "kind": "openai_chat_completions",
                "host": "127.0.0.1",
                "port": mock_port,
                "use_http": True,
                "api_path": "/v1/chat/completions",
                "rag_provider": "openai",
            },
        },
    }
    for k, v in (mini_chat_overrides or {}).items():
        mini_chat[k] = v
    policy = {
        "vendor": "constructorfabric",
        "priority": 100,
        "model_catalog": catalog if catalog is not None else default_catalog(),
    }
    if kill_switches:
        policy["kill_switches"] = kill_switches
    if standard_limits:
        policy["default_standard_limits"] = standard_limits
    if premium_limits:
        policy["default_premium_limits"] = premium_limits
    cfg = {
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
                    "bind_addr": f"127.0.0.1:{api_port}",
                    "enable_docs": False,
                    "cors_enabled": False,
                    "defaults": {"body_limit_bytes": 64000000},
                    "auth_disabled": False,
                    "rate_limit_zones": {
                        "rl_mini_chat_chat": {
                            "rate_limit": "1000/s", "burst_limit": 1000, "response_status_code": 429,
                            "response_retry_after": "auto", "key": {"type": "ip"}, "max_keys": 10000,
                        }
                    },
                    "in_flight_limit_zones": {
                        "ifl_mini_chat_chat": {
                            "in_flight_limit": 100, "backlog_limit": 0, "backlog_timeout": "0s",
                            "response_status_code": 429, "key": {"type": "ip"}, "max_keys": 10000,
                        }
                    },
                }
            },
            "gear-orchestrator": {"config": {}},
            "authn-resolver": {"config": {"vendor": "constructorfabric"}},
            "authz-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authn-plugin": {
                "config": {
                    "vendor": "constructorfabric",
                    "priority": 100,
                    "mode": "static_tokens",
                    "tokens": [
                        {"token": t, "identity": {"subject_id": u, "subject_tenant_id": tn, "token_scopes": ["*"]}}
                        for (t, u, tn) in users
                    ],
                    "s2s_credentials": [{"client_id": "mini-chat", "client_secret": "secret"}],
                }
            },
            "static-authz-plugin": {"config": {"vendor": "constructorfabric", "priority": 100}},
            "credstore": {
                "database": {"server": "sqlite_mc", "file": "credstore.db"},
                "config": {"vendor": "constructorfabric"},
            },
            "static-credstore-plugin": {"config": {"secrets": [{"key": "openai-key", "value": "sk-test"}]}},
            "tenant-resolver": {"config": {"vendor": "constructorfabric"}},
            "single-tenant-tr-plugin": {"config": {"vendor": "constructorfabric"}},
            "mini-chat": {
                "database": {"server": "sqlite_mc", "file": "mini_chat.db"},
                "config": mini_chat,
            },
            "static-mini-chat-audit-plugin": {"config": {"vendor": "constructorfabric", "priority": 100, "enabled": True}},
            "static-mini-chat-model-policy-plugin": {"config": policy},
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
    return cfg


class Stack:
    """A running mock provider + server."""

    def __init__(self, workdir, **cfg_kwargs):
        self.workdir = Path(workdir)
        self.workdir.mkdir(parents=True, exist_ok=True)
        self.home = self.workdir / "home"
        self.home.mkdir(exist_ok=True)
        self.api_port = free_port()
        self.mock_port = free_port()
        self.cfg_kwargs = cfg_kwargs
        self.mock = None
        self.server = None

    @property
    def base(self):
        return f"http://127.0.0.1:{self.api_port}"

    @property
    def mock_base(self):
        return f"http://127.0.0.1:{self.mock_port}"

    def start(self):
        self.mock = subprocess.Popen(
            [sys.executable, str(HERE / "mock_provider.py"), str(self.mock_port)],
            stdout=subprocess.DEVNULL,
            stderr=open(self.workdir / "mock.log", "w"),
        )
        cfg = build_config(self.home, self.api_port, self.mock_port, **self.cfg_kwargs)
        cfg_path = self.workdir / "config.json"
        cfg_path.write_text(json.dumps(cfg, indent=2))
        # The server reads YAML; JSON is valid YAML.
        yaml_path = self.workdir / "config.yaml"
        yaml_path.write_text(json.dumps(cfg, indent=2))
        self.server = subprocess.Popen(
            [str(SERVER_BIN), "--config", str(yaml_path), "run"],
            stdout=open(self.workdir / "server.log", "w"),
            stderr=subprocess.STDOUT,
            cwd=str(self.workdir),
        )
        deadline = time.time() + 120
        while time.time() < deadline:
            if self.server.poll() is not None:
                raise RuntimeError("server exited: " + (self.workdir / "server.log").read_text()[-4000:])
            try:
                r = httpx.get(self.base + "/mini-chat/v1/models", headers=auth("token-a1"), timeout=2)
                if r.status_code == 200:
                    break
            except httpx.HTTPError:
                pass
            time.sleep(0.5)
        else:
            raise RuntimeError("server did not become ready")
        # wait for the outbox / S2S start
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                r = httpx.get(self.base + "/health", timeout=2)
                if r.status_code == 200 and '"mini-chat"' in r.text and "unhealthy" not in r.text:
                    break
            except httpx.HTTPError:
                pass
            time.sleep(0.5)
        return self

    def stop(self):
        for p in (self.server, self.mock):
            if p is not None and p.poll() is None:
                p.send_signal(signal.SIGTERM)
                try:
                    p.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    p.kill()

    # ---- helpers ----------------------------------------------------------

    def db(self):
        path = next(self.home.rglob("mini_chat.db"))
        conn = sqlite3.connect(str(path), timeout=10)
        conn.row_factory = sqlite3.Row
        return conn

    def query(self, sql, params=()):
        conn = self.db()
        try:
            return [dict(r) for r in conn.execute(sql, params).fetchall()]
        finally:
            conn.close()

    def execute(self, sql, params=()):
        conn = self.db()
        try:
            conn.execute(sql, params)
            conn.commit()
        finally:
            conn.close()

    def mock_requests(self, path_contains=None, method=None):
        reqs = httpx.get(self.mock_base + "/_admin/requests", timeout=5).json()
        out = []
        for r in reqs:
            if path_contains and path_contains not in r["path"]:
                continue
            if method and r["method"] != method:
                continue
            out.append(r)
        return out

    def mock_reset(self):
        httpx.post(self.mock_base + "/_admin/reset", timeout=5)

    def mock_script(self, items):
        httpx.post(self.mock_base + "/_admin/script", json=items, timeout=5)

    def mock_set(self, **kw):
        httpx.post(self.mock_base + "/_admin/set", json=kw, timeout=5)


def auth(token="token-a1"):
    return {"Authorization": f"Bearer {token}"}


def ubytes(u):
    return uuid.UUID(str(u)).bytes


def parse_sse(text):
    events = []
    name = None
    data = []
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
                events.append((name, payload))
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
        events.append((name, payload))
    return events


class Client:
    def __init__(self, stack, token="token-a1"):
        self.s = stack
        self.h = auth(token)
        self.c = httpx.Client(base_url=stack.base + "/mini-chat", timeout=60)

    def req(self, method, path, **kw):
        headers = dict(self.h)
        headers.update(kw.pop("headers", {}))
        return self.c.request(method, path, headers=headers, **kw)

    def create_chat(self, **body):
        r = self.req("POST", "/v1/chats", json=body)
        assert r.status_code == 201, r.text
        return r.json()

    def stream(self, chat_id, content="hello", **body):
        payload = {"content": content}
        payload.update(body)
        r = self.req("POST", f"/v1/chats/{chat_id}/messages:stream", json=payload)
        return r

    def send(self, chat_id, content="hello", **body):
        r = self.stream(chat_id, content, **body)
        assert r.status_code == 200, r.text
        return parse_sse(r.text)

    def messages(self, chat_id, **params):
        r = self.req("GET", f"/v1/chats/{chat_id}/messages", params=params)
        assert r.status_code == 200, r.text
        return r.json()

    def upload(self, chat_id, filename, data, content_type):
        files = {"file": (filename, data, content_type)}
        return self.req("POST", f"/v1/chats/{chat_id}/attachments", files=files)


def wait_for(fn, timeout=20, interval=0.2):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    return last
