"""Black-box server harness for the mini-chat E2E suite.

Fixtures
--------
``mock``
    Session-scoped :class:`mock_provider.MockProvider` (OpenAI-compatible LLM
    and RAG upstream). Its scenario queue, failures, recorded requests and
    stored objects are reset before every test.
``mc``
    Session-scoped default :class:`MiniChatServer` (the example server with
    the mini-chat gear, wired to ``mock`` through OAGW).
``start_server``
    Factory that launches an ADDITIONAL server with a deep-merged config
    patch: ``start_server({"gears": {...}})`` returns a ready
    :class:`MiniChatServer`; ``start_server(patch, expect_fail=True)`` waits
    for the process to exit and returns a :class:`StartupOutcome`. Extra
    servers are stopped when the test ends.
``users``
    The static-authn identities (``A1``, ``A2`` in tenant A, ``B1`` in tenant B).

Every server instance gets its own API port, ``server.home_dir``, SQLite
files, grpc-hub UDS path and log file. Servers are stopped only through their
own ``Popen`` handle. The binary is ``<repo>/target/debug/cf-gears-example-server``
(override with ``MC_SERVER_BIN``); it is never built here. Build it with::

    cargo build --bin cf-gears-example-server --no-default-features \\
        --features mini-chat,static-authn,static-authz,single-tenant,static-credstore

Environment knobs: ``MC_SERVER_BIN``, ``MC_READY_TIMEOUT`` (seconds, default
90), ``MC_KEEP_ARTIFACTS=1`` (keep instance directories and logs).
"""

from __future__ import annotations

import atexit
import copy
import json
import os
import re
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import IO, Any, Callable, Iterator

import httpx
import pytest

from mc_helpers import SseResult, db_query, deep_merge, stream_sse
from mock_provider import MockProvider

try:
    import yaml
except ImportError:  # pragma: no cover - JSON is valid YAML, used as fallback
    yaml = None

REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_SERVER_BIN = REPO_ROOT / "target" / "debug" / "cf-gears-example-server"
READY_TIMEOUT = float(os.environ.get("MC_READY_TIMEOUT", "90"))
KEEP_ARTIFACTS = os.environ.get("MC_KEEP_ARTIFACTS", "") not in ("", "0", "false")
#: Seconds a server expected to fail startup may serve before it counts as started.
STARTUP_FAILURE_READY_GRACE = 5.0

VENDOR = "constructorfabric"
DB_SERVER = "sqlite_mc"
URL_PREFIX = "/mini-chat"
#: api-gateway ``prefix_path``; ``MiniChatServer.base_url`` includes it.
GATEWAY_PREFIX = ""
S2S_CLIENT_ID = "mini-chat"
S2S_CLIENT_SECRET = "mini-chat-dev-secret"
OPENAI_SECRET_KEY = "openai-key"
OPENAI_SECRET_VALUE = "sk-test-key"
APIKEY_AUTH_PLUGIN = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
SUBJECT_USER_TYPE = "gts.cf.core.security.subject_user.v1~"
PROVIDER_ID = "openai"


@dataclass(frozen=True)
class Identity:
    """A static-authn token and the identity it resolves to."""

    name: str
    token: str
    user_id: str
    tenant_id: str


TENANT_A = "a0000000-0000-4000-8000-00000000000a"
TENANT_B = "b0000000-0000-4000-8000-00000000000b"
USERS: dict[str, Identity] = {
    "A1": Identity("A1", "mc-e2e-token-a1", "a1000000-0000-4000-8000-0000000000a1", TENANT_A),
    "A2": Identity("A2", "mc-e2e-token-a2", "a2000000-0000-4000-8000-0000000000a2", TENANT_A),
    "B1": Identity("B1", "mc-e2e-token-b1", "b1000000-0000-4000-8000-0000000000b1", TENANT_B),
}
#: The mini-chat S2S identity (``client_credentials`` exchange) and a static
#: token for it. OAGW upstreams belong to its tenant, and the credstore
#: gateway only serves secrets that have metadata in that tenant, so the
#: harness creates the provider secrets through the credstore REST API with
#: this token (see :meth:`MiniChatServer.provision_provider_secrets`).
S2S_IDENTITY = Identity(
    "S2S", "mc-e2e-token-s2s", "5e5e0000-0000-4000-8000-000000000525", "5e5e0000-0000-4000-8000-0000000005e5"
)
#: Seconds to wait for the mini-chat OAGW reconcile to register the providers.
PROVISION_TIMEOUT = float(os.environ.get("MC_PROVISION_TIMEOUT", "40"))
_PROVISIONED_AT_START = re.compile(r"OAGW provisioning done.* pending=0\b")

# --------------------------------------------------------------------------
# Config
# --------------------------------------------------------------------------


def _catalog_entry(
    model_id: str,
    *,
    provider_model_id: str,
    display_name: str,
    description: str | None,
    tier: str,
    enabled: bool,
    multiplier_display: str,
    capabilities: list[str],
    tools: dict[str, bool],
    context_window: int,
    max_output_tokens: int,
    input_multiplier_micro: int,
    output_multiplier_micro: int,
    is_default: bool,
    sort_order: int,
) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "id": model_id,
        "provider_model_id": provider_model_id,
        "display_name": display_name,
        "provider_id": PROVIDER_ID,
        "provider_display_name": "OpenAI",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": "You are a helpful, concise assistant. Answer clearly and directly.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": capabilities,
        "context_window": context_window,
        "max_output_tokens": max_output_tokens,
        "max_input_tokens": context_window,
        "input_tokens_credit_multiplier_micro": input_multiplier_micro,
        "output_tokens_credit_multiplier_micro": output_multiplier_micro,
        "multiplier_display": multiplier_display,
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
            "api_params": {
                "temperature": 0.7,
                "top_p": 1.0,
                "frequency_penalty": 0.0,
                "presence_penalty": 0.0,
                "stop": [],
            },
            "features": {"streaming": True, "structured_output": True},
            "tool_support": {
                "web_search": tools.get("web_search", False),
                "file_search": tools.get("file_search", False),
                "image_generation": False,
                "code_interpreter": tools.get("code_interpreter", False),
                "mcp": False,
            },
            "supported_endpoints": {
                "chat_completions": True,
                "responses": True,
                "embeddings": False,
                "image_generation": False,
                "audio_speech_generation": False,
                "audio_transcription": False,
                "audio_translation": False,
            },
        },
        "preference": {"is_default": is_default, "sort_order": sort_order},
    }
    if description is not None:
        entry["description"] = description
    return entry


def default_model_catalog() -> list[dict[str, Any]]:
    """Static policy plugin catalog used by every server instance.

    * ``gpt-4.1``: premium, default, vision, web/file search + code interpreter;
    * ``gpt-4.1-mini``: standard, vision, code interpreter only;
    * ``gpt-disabled``: premium, ``enabled: false``;
    * ``gpt-text-only``: standard, no description, no ``VISION_INPUT``, no tools.
    """
    return [
        _catalog_entry(
            "gpt-4.1",
            provider_model_id="gpt-4.1-2025-04-14",
            display_name="GPT-4.1",
            description="Most capable model",
            tier="Premium",
            enabled=True,
            multiplier_display="3x",
            capabilities=["VISION_INPUT"],
            tools={"web_search": True, "file_search": True, "code_interpreter": True},
            context_window=1_047_576,
            max_output_tokens=32_768,
            input_multiplier_micro=3_000_000,
            output_multiplier_micro=15_000_000,
            is_default=True,
            sort_order=0,
        ),
        _catalog_entry(
            "gpt-4.1-mini",
            provider_model_id="gpt-4.1-mini-2025-04-14",
            display_name="GPT-4.1 Mini",
            description="Fast and efficient model",
            tier="Standard",
            enabled=True,
            multiplier_display="1x",
            capabilities=["VISION_INPUT"],
            tools={"code_interpreter": True},
            context_window=1_047_576,
            max_output_tokens=32_768,
            input_multiplier_micro=1_000_000,
            output_multiplier_micro=3_000_000,
            is_default=False,
            sort_order=1,
        ),
        _catalog_entry(
            "gpt-disabled",
            provider_model_id="gpt-disabled-2025-01-01",
            display_name="GPT Disabled",
            description="Temporarily unavailable model",
            tier="Premium",
            enabled=False,
            multiplier_display="5x",
            capabilities=["VISION_INPUT"],
            tools={"web_search": True, "file_search": True, "code_interpreter": True},
            context_window=200_000,
            max_output_tokens=16_384,
            input_multiplier_micro=5_000_000,
            output_multiplier_micro=20_000_000,
            is_default=False,
            sort_order=2,
        ),
        _catalog_entry(
            "gpt-text-only",
            provider_model_id="gpt-4o-mini-text-2024-07-18",
            display_name="GPT Text Only",
            description=None,
            tier="Standard",
            enabled=True,
            multiplier_display="1x",
            capabilities=[],
            tools={},
            context_window=128_000,
            max_output_tokens=16_384,
            input_multiplier_micro=500_000,
            output_multiplier_micro=1_500_000,
            is_default=False,
            sort_order=3,
        ),
    ]


def _token_entry(identity: Identity) -> dict[str, Any]:
    return {
        "token": identity.token,
        "identity": {
            "subject_id": identity.user_id,
            "subject_tenant_id": identity.tenant_id,
            "subject_type": SUBJECT_USER_TYPE,
            "token_scopes": ["*"],
        },
    }


def build_server_config(
    *, api_port: int, mock_port: int, home_dir: Path, log_dir: Path, grpc_socket: Path
) -> dict[str, Any]:
    """Complete server config: platform gears, OAGW, credstore and mini-chat."""
    return {
        "server": {"home_dir": str(home_dir)},
        "database": {
            "servers": {
                DB_SERVER: {
                    "engine": "sqlite",
                    "params": {"WAL": "true", "synchronous": "NORMAL", "busy_timeout": "5000"},
                    "pool": {"max_conns": 5, "acquire_timeout": "30s"},
                }
            }
        },
        "logging": {
            "default": {
                "console_level": "info",
                "file": str(log_dir / "server-file.log"),
                "file_level": "debug",
            },
            "mini_chat": {
                "console_level": "debug",
                "file": str(log_dir / "mini-chat.log"),
                "file_level": "debug",
            },
        },
        "gears": {
            "api-gateway": {
                "config": {
                    "bind_addr": f"127.0.0.1:{api_port}",
                    "prefix_path": GATEWAY_PREFIX,
                    "enable_docs": False,
                    "cors_enabled": False,
                    "openapi": {
                        "title": "Mini-Chat E2E API",
                        "version": "0.1.0",
                        "description": "Mini-chat black-box E2E server",
                    },
                    # >= 25 MiB + 64 KiB so 25 MiB documents reach mini-chat.
                    "defaults": {"body_limit_bytes": 64_000_000},
                    "auth_disabled": False,
                    "require_auth_by_default": True,
                    # Zones referenced by mini-chat routes; generous for tests.
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
                            "in_flight_limit": 1000,
                            "backlog_limit": 0,
                            "backlog_timeout": "0s",
                            "response_status_code": 429,
                            "key": {"type": "ip"},
                            "max_keys": 10000,
                        }
                    },
                }
            },
            "gear-orchestrator": {"config": {}},
            "grpc-hub": {"config": {"listen_addr": f"uds://{grpc_socket}"}},
            "types-registry": {
                "database": {"server": DB_SERVER, "file": "types_registry.db"},
                "config": {},
            },
            "authn-resolver": {"config": {"vendor": VENDOR}},
            "authz-resolver": {"config": {"vendor": VENDOR}},
            "static-authn-plugin": {
                "config": {
                    "vendor": VENDOR,
                    "priority": 100,
                    "mode": "static_tokens",
                    "tokens": [
                        _token_entry(identity) for identity in [*USERS.values(), S2S_IDENTITY]
                    ],
                    "s2s_credentials": [
                        {
                            "client_id": S2S_CLIENT_ID,
                            "client_secret": S2S_CLIENT_SECRET,
                            "identity": _token_entry(S2S_IDENTITY)["identity"],
                        }
                    ],
                }
            },
            "static-authz-plugin": {"config": {"vendor": VENDOR, "priority": 100}},
            "tenant-resolver": {"config": {"vendor": VENDOR}},
            "single-tenant-tr-plugin": {"config": {"vendor": VENDOR}},
            "credstore": {
                "database": {"server": DB_SERVER, "file": "credstore.db"},
                "config": {"vendor": VENDOR},
            },
            "static-credstore-plugin": {
                "config": {
                    "vendor": VENDOR,
                    "priority": 100,
                    # No tenant_id: a global secret readable from every tenant.
                    "secrets": [{"key": OPENAI_SECRET_KEY, "value": OPENAI_SECRET_VALUE}],
                }
            },
            "oagw": {
                "config": {
                    "proxy_timeout_secs": 30,
                    "allow_http_upstream": True,
                    "ssrf_policy": {"enabled": False},
                }
            },
            "mini-chat": {
                "database": {"server": DB_SERVER, "file": "mini_chat.db"},
                "config": {
                    "vendor": VENDOR,
                    "client_credentials": {
                        "client_id": S2S_CLIENT_ID,
                        "client_secret": S2S_CLIENT_SECRET,
                    },
                    "orphan_watchdog": {"enabled": True, "scan_interval_secs": 1},
                    "thread_summary_worker": {"enabled": True, "summary_model_id": "gpt-4.1-mini"},
                    "providers": {
                        PROVIDER_ID: {
                            "kind": "openai_responses",
                            "storage_kind": "openai",
                            "host": "127.0.0.1",
                            "port": mock_port,
                            "use_http": True,
                            "api_path": "/v1/responses",
                            "auth_plugin_type": APIKEY_AUTH_PLUGIN,
                            "auth_config": {
                                "header": "Authorization",
                                "prefix": "Bearer ",
                                "secret_ref": f"cred://{OPENAI_SECRET_KEY}",
                            },
                        }
                    },
                },
            },
            "static-mini-chat-model-policy-plugin": {
                "config": {
                    "vendor": VENDOR,
                    "priority": 100,
                    "model_catalog": default_model_catalog(),
                    "kill_switches": {
                        "disable_premium_tier": False,
                        "force_standard_tier": False,
                        "disable_web_search": False,
                        "disable_file_search": False,
                        "disable_images": False,
                        "disable_code_interpreter": False,
                    },
                    "default_standard_limits": {
                        "limit_daily_credits_micro": 1_000_000_000_000,
                        "limit_monthly_credits_micro": 10_000_000_000_000,
                    },
                    "default_premium_limits": {
                        "limit_daily_credits_micro": 500_000_000_000,
                        "limit_monthly_credits_micro": 5_000_000_000_000,
                    },
                }
            },
            "static-mini-chat-audit-plugin": {
                "config": {"vendor": VENDOR, "priority": 100, "enabled": True}
            },
        },
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
    }


# --------------------------------------------------------------------------
# Server process
# --------------------------------------------------------------------------


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def server_binary() -> Path:
    override = os.environ.get("MC_SERVER_BIN")
    binary = Path(override) if override else DEFAULT_SERVER_BIN
    if not binary.is_file():
        pytest.fail(
            f"server binary not found: {binary}\n"
            "Build it with: cargo build --bin cf-gears-example-server --no-default-features "
            "--features mini-chat,static-authn,static-authz,single-tenant,static-credstore "
            "(or set MC_SERVER_BIN)",
            pytrace=False,
        )
    return binary


_LIVE_SERVERS: set["MiniChatServer"] = set()


def _stop_live_servers() -> None:
    for server in list(_LIVE_SERVERS):
        server.stop()


atexit.register(_stop_live_servers)


class MiniChatServer:
    """One running (or starting) example-server process with the mini-chat gear."""

    def __init__(self, name: str, config: dict[str, Any], work_dir: Path, binary: Path) -> None:
        self.name = name
        self.config = config
        self.work_dir = work_dir
        self.binary = binary
        self.config_path = work_dir / "server-config.yaml"
        self.log_path = work_dir / "logs" / "server.log"
        bind_addr = config["gears"]["api-gateway"]["config"]["bind_addr"]
        self.host, port = bind_addr.rsplit(":", 1)
        self.port = int(port)
        self.prefix_path = config["gears"]["api-gateway"]["config"].get("prefix_path", "")
        self.home_dir = Path(config["server"]["home_dir"])
        db_file = config["gears"]["mini-chat"]["database"]["file"]
        #: SQLite file of the mini-chat gear: ``<home_dir>/<gear name>/<file>``.
        self.db_path = self.home_dir / "mini-chat" / db_file
        self.users = USERS
        self.proc: subprocess.Popen[bytes] | None = None
        self._log_handle: IO[bytes] | None = None
        self._client: httpx.Client | None = None

    # ---------------------------------------------------------------- URLs

    @property
    def base_url(self) -> str:
        """Server root including the api-gateway ``prefix_path``."""
        return f"http://{self.host}:{self.port}{self.prefix_path}"

    def url(self, path: str) -> str:
        """Absolute URL of a gear path, e.g. ``/mini-chat/v1/models``.

        Paths that do not start with the gear prefix (``/v1/models``) get it
        prepended.
        """
        if not path.startswith("/"):
            path = "/" + path
        if not path.startswith(URL_PREFIX + "/"):
            path = URL_PREFIX + path
        return self.base_url + path

    @property
    def gear_config(self) -> dict[str, Any]:
        """The ``gears.mini-chat.config`` section in effect."""
        return self.config["gears"]["mini-chat"]["config"]

    @property
    def catalog(self) -> list[dict[str, Any]]:
        """Static policy plugin model catalog in effect."""
        return self.config["gears"]["static-mini-chat-model-policy-plugin"]["config"]["model_catalog"]

    # ---------------------------------------------------------- identities

    def identity(self, user: str = "A1") -> Identity:
        return USERS[user]

    def headers(self, user: str | None = "A1", **extra: str) -> dict[str, str]:
        """Request headers for ``user`` (``None`` = no Authorization header)."""
        headers: dict[str, str] = {}
        if user is not None:
            headers["Authorization"] = f"Bearer {USERS[user].token}"
        headers.update(extra)
        return headers

    # ---------------------------------------------------------------- HTTP

    @property
    def client(self) -> httpx.Client:
        if self._client is None:
            self._client = httpx.Client(timeout=httpx.Timeout(60.0, connect=5.0))
        return self._client

    def request(
        self,
        method: str,
        path: str,
        user: str | None = "A1",
        *,
        headers: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> httpx.Response:
        """Send a request to a gear path as ``user``."""
        merged = self.headers(user)
        merged.update(headers or {})
        return self.client.request(method, self.url(path), headers=merged, **kwargs)

    def get(self, path: str, user: str | None = "A1", **kwargs: Any) -> httpx.Response:
        return self.request("GET", path, user, **kwargs)

    def post(self, path: str, user: str | None = "A1", **kwargs: Any) -> httpx.Response:
        return self.request("POST", path, user, **kwargs)

    def patch(self, path: str, user: str | None = "A1", **kwargs: Any) -> httpx.Response:
        return self.request("PATCH", path, user, **kwargs)

    def put(self, path: str, user: str | None = "A1", **kwargs: Any) -> httpx.Response:
        return self.request("PUT", path, user, **kwargs)

    def delete(self, path: str, user: str | None = "A1", **kwargs: Any) -> httpx.Response:
        return self.request("DELETE", path, user, **kwargs)

    def stream(
        self,
        method: str,
        path: str,
        user: str | None = "A1",
        *,
        headers: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> SseResult:
        """Send a request and read its SSE stream (see :func:`mc_helpers.stream_sse`)."""
        merged = self.headers(user)
        merged.update(headers or {})
        return stream_sse(self.client, method, self.url(path), headers=merged, **kwargs)

    # -------------------------------------------------------------- SQLite

    def sqlite(self, readonly: bool = True) -> sqlite3.Connection:
        """Connection to the mini-chat SQLite database."""
        if not self.db_path.exists():
            raise AssertionError(f"mini-chat database does not exist (yet): {self.db_path}")
        if readonly:
            conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=10)
        else:
            conn = sqlite3.connect(str(self.db_path), timeout=10)
        conn.execute("PRAGMA busy_timeout = 10000")
        return conn

    def query(self, sql: str, params: tuple[Any, ...] | list[Any] = ()) -> list[dict[str, Any]]:
        """Run a read query on the mini-chat database (see :func:`mc_helpers.db_query`)."""
        conn = self.sqlite()
        try:
            return db_query(conn, sql, params)
        finally:
            conn.close()

    # ------------------------------------------------------------- process

    @property
    def is_running(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    @property
    def returncode(self) -> int | None:
        return None if self.proc is None else self.proc.poll()

    def start(self) -> "MiniChatServer":
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        text = (
            yaml.safe_dump(self.config, sort_keys=False)
            if yaml is not None
            else json.dumps(self.config, indent=2)
        )
        self.config_path.write_text(text)
        self._log_handle = open(self.log_path, "wb")
        env = {**os.environ, "RUST_BACKTRACE": "1", "NO_COLOR": "1"}
        self.proc = subprocess.Popen(
            [str(self.binary), "--config", str(self.config_path), "run"],
            cwd=str(REPO_ROOT),
            stdin=subprocess.DEVNULL,
            stdout=self._log_handle,
            stderr=subprocess.STDOUT,
            env=env,
        )
        _LIVE_SERVERS.add(self)
        return self

    def log_text(self) -> str:
        if self._log_handle is not None:
            self._log_handle.flush()
        try:
            return self.log_path.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def log_tail(self, lines: int = 200) -> str:
        return "\n".join(self.log_text().splitlines()[-lines:])

    def _answers(self) -> bool:
        """``/readyz`` is 200 and the gear route answers with any HTTP status."""
        try:
            ready = self.client.get(f"{self.base_url}/readyz", timeout=3)
            if ready.status_code != 200:
                return False
            self.client.get(self.url("/v1/models"), headers=self.headers("A1"), timeout=5)
        except httpx.TransportError:
            return False
        return True

    def wait_ready(self, timeout: float = READY_TIMEOUT) -> "MiniChatServer":
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not self.is_running:
                code = self.returncode
                self.stop()
                pytest.fail(
                    f"[{self.name}] server exited during startup (code {code}).\n"
                    f"config: {self.config_path}\nlog: {self.log_path}\n"
                    f"--- last 200 log lines ---\n{self.log_tail()}",
                    pytrace=False,
                )
            if self._answers():
                self.provision_provider_secrets()
                return self
            time.sleep(0.25)
        self.stop()
        pytest.fail(
            f"[{self.name}] server not ready within {timeout:.0f}s.\n"
            f"config: {self.config_path}\nlog: {self.log_path}\n"
            f"--- last 200 log lines ---\n{self.log_tail()}",
            pytrace=False,
        )

    # ----------------------------------------------------------- provisioning

    def _credstore_secrets(self) -> list[dict[str, Any]]:
        plugin = self.config["gears"].get("static-credstore-plugin", {}).get("config", {})
        return list(plugin.get("secrets", []))

    def provision_provider_secrets(self, timeout: float = PROVISION_TIMEOUT) -> None:
        """Create the provider secrets in credstore and wait for OAGW provisioning.

        The static credstore plugin only holds secret values; the credstore
        gateway resolves a ``secret_ref`` through its own metadata, which only
        the credstore API creates. Until the secret exists, mini-chat leaves
        its providers pending and its reconcile task registers them later
        (DESIGN §3.2 "OAGW provisioning"). This creates every secret of the
        static plugin config in the S2S tenant (an existing one is written
        again) and waits until mini-chat reports every provider upstream
        registered.
        """
        secrets = self._credstore_secrets()
        if not secrets:
            return
        headers = {"Authorization": f"Bearer {S2S_IDENTITY.token}"}
        for secret in secrets:
            resp = self.client.post(
                f"{self.base_url}/credstore/v1/secrets",
                headers=headers,
                json={"reference": secret["key"], "value": secret["value"], "sharing": "tenant"},
                timeout=10,
            )
            if resp.status_code == 409:
                # A restart on the same home_dir keeps the credstore metadata,
                # but the static plugin lost the value written through the
                # API: write it again.
                resp = self.client.put(
                    f"{self.base_url}/credstore/v1/secrets/{secret['key']}",
                    headers={**headers, "If-Match": "*"},
                    json={"value": secret["value"], "sharing": "tenant"},
                    timeout=10,
                )
            if resp.status_code not in (200, 201, 204):
                self.stop()
                pytest.fail(
                    f"[{self.name}] credstore secret {secret['key']!r} not created: "
                    f"HTTP {resp.status_code} {resp.text[:500]}",
                    pytrace=False,
                )
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            log = self.log_text()
            if "all provider upstreams registered" in log or _PROVISIONED_AT_START.search(log):
                return
            if not self.is_running:
                break
            time.sleep(0.25)
        code = self.returncode
        self.stop()
        pytest.fail(
            f"[{self.name}] mini-chat providers not registered in OAGW within {timeout:.0f}s "
            f"(code {code}).\nlog: {self.log_path}\n--- last 200 log lines ---\n{self.log_tail()}",
            pytrace=False,
        )

    def wait_startup_failure(
        self, timeout: float, ready_grace: float = STARTUP_FAILURE_READY_GRACE
    ) -> tuple[int | None, bool]:
        """Wait for a startup that is expected to fail.

        Returns ``(returncode, became_ready)``. Config validation runs at gear
        and plugin init, before the REST listener serves requests, so a server
        that has been answering for ``ready_grace`` seconds is not going to
        fail: the wait ends early with ``(None, True)``.
        """
        deadline = time.monotonic() + timeout
        ready_since: float | None = None
        while time.monotonic() < deadline:
            code = self.returncode
            if code is not None:
                return code, False
            if self._answers():
                ready_since = ready_since or time.monotonic()
                if time.monotonic() - ready_since >= ready_grace:
                    return None, True
            time.sleep(0.25)
        return self.returncode, ready_since is not None

    def wait_exit(self, timeout: float) -> int | None:
        """Wait for the process to exit; return its code or ``None`` on timeout."""
        assert self.proc is not None, "server was not started"
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None

    def stop(self, timeout: float = 15.0) -> None:
        """Stop the process through its own handle (SIGTERM, then SIGKILL)."""
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        if self._client is not None:
            self._client.close()
            self._client = None
        if self._log_handle is not None:
            self._log_handle.close()
            self._log_handle = None
        _LIVE_SERVERS.discard(self)

    def cleanup(self) -> None:
        """Remove the instance directory unless ``MC_KEEP_ARTIFACTS`` is set."""
        if not KEEP_ARTIFACTS and not self.is_running:
            shutil.rmtree(self.work_dir, ignore_errors=True)

    def __repr__(self) -> str:
        return f"MiniChatServer(name={self.name!r}, base_url={self.base_url!r}, pid={getattr(self.proc, 'pid', None)})"


@dataclass
class StartupOutcome:
    """Result of ``start_server(..., expect_fail=True)``."""

    returncode: int | None
    log: str
    became_ready: bool
    config: dict[str, Any]

    @property
    def exited_nonzero(self) -> bool:
        return self.returncode is not None and self.returncode != 0

    def describe(self) -> str:
        tail = "\n".join(self.log.splitlines()[-200:])
        return (
            f"returncode={self.returncode} became_ready={self.became_ready}\n"
            f"--- last 200 log lines ---\n{tail}"
        )


def create_server(mock: MockProvider, overrides: dict[str, Any] | None = None, name: str = "server") -> MiniChatServer:
    """Prepare (but do not start) a server instance with ``overrides`` deep-merged."""
    work_dir = Path(tempfile.mkdtemp(prefix=f"mc-e2e-{name}-"))
    config = build_server_config(
        api_port=_free_port(),
        mock_port=mock.port,
        home_dir=work_dir / "home",
        log_dir=work_dir / "logs",
        grpc_socket=work_dir / "grpc.sock",
    )
    if overrides:
        config = deep_merge(config, overrides)
    return MiniChatServer(name, config, work_dir, server_binary())


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


@pytest.fixture(scope="session")
def mock() -> Iterator[MockProvider]:
    provider = MockProvider().start()
    try:
        yield provider
    finally:
        provider.stop()


@pytest.fixture(autouse=True)
def _fresh_mock(request: pytest.FixtureRequest) -> None:
    """Reset the shared mock provider before every test that uses it."""
    if "mock" in request.fixturenames:
        request.getfixturevalue("mock").reset()


@pytest.fixture(scope="session")
def users() -> dict[str, Identity]:
    return USERS


@pytest.fixture
def model_catalog() -> list[dict[str, Any]]:
    """A fresh copy of the default static policy plugin catalog (safe to modify)."""
    return default_model_catalog()


@pytest.fixture(scope="session")
def mc(mock: MockProvider) -> Iterator[MiniChatServer]:
    server = create_server(mock, name="default").start()
    try:
        server.wait_ready()
        yield server
    finally:
        server.stop()
        server.cleanup()


StartServer = Callable[..., Any]


@pytest.fixture
def start_server(mock: MockProvider) -> Iterator[StartServer]:
    """Factory for additional server instances; all are stopped at test end."""
    started: list[MiniChatServer] = []

    def _start(
        config_overrides: dict[str, Any] | None = None,
        expect_fail: bool = False,
        *,
        name: str = "extra",
        ready_timeout: float = READY_TIMEOUT,
        exit_timeout: float = 60.0,
    ) -> MiniChatServer | StartupOutcome:
        server = create_server(mock, copy.deepcopy(config_overrides or {}), name=name)
        started.append(server)
        server.start()
        if not expect_fail:
            return server.wait_ready(ready_timeout)
        code, became_ready = server.wait_startup_failure(exit_timeout)
        server.stop()
        return StartupOutcome(
            returncode=code, log=server.log_text(), became_ready=became_ready, config=server.config
        )

    yield _start
    for server in started:
        server.stop()
        server.cleanup()
