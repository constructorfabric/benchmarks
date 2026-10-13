"""Session fixtures for the mini-chat black-box E2E suite.

* ``mock_llm``   -- in-process OpenAI-compatible mock (``mock_llm.py``)
* ``server``     -- the primary ``cf-gears-example-server`` (default config)
* ``ks_server``  -- a lazily started secondary server whose static model policy
                    plugin turns on kill switches (``disable_web_search``,
                    ``disable_images``, ``disable_code_interpreter``,
                    ``force_standard_tier``)
* ``api`` / ``api_for`` -- httpx clients bound to a static-authn token
* ``db``         -- sqlite3 helper on the gear database of ``server``

The servers are started with ``subprocess.Popen`` and stopped ONLY through that
Popen handle (never by process name).
"""

from __future__ import annotations

import atexit
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Callable, Optional

import httpx
import pytest
import yaml

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from mchelpers import DB, USERS, Api  # noqa: E402
from mock_llm import MockLLM, free_port  # noqa: E402

REPO_ROOT = HERE.parents[2]
DEFAULT_BINARY = REPO_ROOT / "target" / "debug" / "cf-gears-example-server"
STARTUP_TIMEOUT = float(os.environ.get("MC_E2E_STARTUP_TIMEOUT", "120"))


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


def _catalog_entry(
    model_id: str,
    tier: str,
    *,
    enabled: bool = True,
    vision: bool = True,
    web_search: bool = True,
    file_search: bool = True,
    code_interpreter: bool = True,
    context_window: int = 128000,
    max_output_tokens: int = 4096,
    max_input_tokens: int = 100000,
    in_mult: int = 1_000_000,
    out_mult: int = 3_000_000,
    multiplier_display: str = "1x",
    is_default: bool = False,
    sort_order: int = 10,
    description: Optional[str] = None,
) -> dict:
    return {
        "id": model_id,
        "provider_model_id": f"mock-{model_id}",
        "display_name": f"E2E {model_id}",
        "description": description if description is not None else f"E2E test model {model_id}",
        "provider_id": "openai",
        "provider_display_name": "Mock OpenAI",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": f"You are the E2E test assistant. E2E-SYSPROMPT model={model_id}.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"] if vision else [],
        "context_window": context_window,
        "max_output_tokens": max_output_tokens,
        "max_input_tokens": max_input_tokens,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult,
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
        "max_tool_calls": 4,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0, "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": True, "structured_output": True},
            "tool_support": {
                "web_search": web_search,
                "file_search": file_search,
                "image_generation": False,
                "code_interpreter": code_interpreter,
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


def model_catalog() -> list:
    return [
        _catalog_entry("prem", "Premium", in_mult=3_000_000, out_mult=15_000_000, multiplier_display="3x", is_default=True, sort_order=0),
        # `std` is the first enabled Standard entry: it is the downgrade target.
        _catalog_entry("std", "Standard", sort_order=1),
        _catalog_entry("std-novision", "Standard", vision=False, web_search=False, file_search=False, code_interpreter=False, sort_order=2),
        _catalog_entry(
            "std-tiny",
            "Standard",
            web_search=False,
            file_search=False,
            code_interpreter=False,
            context_window=4096,
            max_output_tokens=1024,
            max_input_tokens=3072,
            sort_order=3,
        ),
        # Input budget min(3000, 4096-3000) = 1096 while INPUT_TOO_LONG triggers only above 3000:
        # a ~6 KB message passes INPUT_TOO_LONG but not the context budget.
        _catalog_entry(
            "std-budget",
            "Standard",
            web_search=False,
            file_search=False,
            code_interpreter=False,
            context_window=4096,
            max_output_tokens=3000,
            max_input_tokens=3000,
            sort_order=4,
        ),
        _catalog_entry("off-model", "Standard", enabled=False, sort_order=5),
        _catalog_entry(
            "prem-novision", "Premium", vision=False, in_mult=3_000_000, out_mult=15_000_000, multiplier_display="3x", sort_order=6
        ),
    ]


def build_config(
    home: Path, port: int, mock_port: int, kill_switches: Optional[dict] = None, tweak: Optional[Callable[[dict], None]] = None
) -> dict:
    tokens = []
    for tok, (sub, ten) in USERS.items():
        tokens.append(
            {
                "token": tok,
                "identity": {
                    "subject_id": sub,
                    "subject_tenant_id": ten,
                    "subject_type": "gts.cf.core.security.subject_user.v1~",
                    "token_scopes": ["*"],
                },
            }
        )
    policy_cfg: dict = {
        "vendor": "constructorfabric",
        "priority": 100,
        "default_standard_limits": {"limit_daily_credits_micro": 100_000_000, "limit_monthly_credits_micro": 1_000_000_000},
        "default_premium_limits": {"limit_daily_credits_micro": 50_000_000, "limit_monthly_credits_micro": 500_000_000},
        "model_catalog": model_catalog(),
    }
    if kill_switches:
        policy_cfg["kill_switches"] = dict(kill_switches)

    log_level = os.environ.get("MC_E2E_LOG_LEVEL", "info")
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
        "logging": {
            "default": {"console_level": "warn", "file": "logs/cf-gears.log", "file_level": log_level},
            "mini_chat": {"console_level": "warn", "file": "logs/mini-chat.log", "file_level": "debug"},
            "api-gateway": {"console_level": "warn", "file": "logs/api.log", "file_level": log_level},
        },
        "gears": {
            "api-gateway": {
                "config": {
                    "bind_addr": f"127.0.0.1:{port}",
                    "enable_docs": False,
                    "cors_enabled": False,
                    "auth_disabled": False,
                    "require_auth_by_default": True,
                    "defaults": {"body_limit_bytes": 64_000_000},
                    # Zones referenced by the mini-chat create_chat route (generous: tests create many chats).
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
            # A database lets the registry admit GTS entities (plugin instances etc.).
            "types-registry": {"database": {"server": "sqlite_mc", "file": "types_registry.db"}, "config": {}},
            "tenant-resolver": {"config": {"vendor": "constructorfabric"}},
            "single-tenant-tr-plugin": {"config": {"vendor": "constructorfabric"}},
            "authn-resolver": {"config": {"vendor": "constructorfabric"}},
            "authz-resolver": {"config": {"vendor": "constructorfabric"}},
            "static-authz-plugin": {"config": {"vendor": "constructorfabric", "priority": 100}},
            "static-authn-plugin": {
                "config": {
                    "vendor": "constructorfabric",
                    "priority": 100,
                    "mode": "static_tokens",
                    "tokens": tokens,
                    "s2s_credentials": [{"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"}],
                }
            },
            "grpc-hub": {"config": {"listen_addr": f"uds:///tmp/cf-gears-grpc-mc-{uuid.uuid4().hex[:10]}"}},
            "gear-orchestrator": {"config": {}},
            "credstore": {
                "database": {"server": "sqlite_mc", "file": "credstore.db"},
                "config": {"vendor": "constructorfabric"},
            },
            "static-credstore-plugin": {"config": {"secrets": [{"key": "openai-key", "value": "sk-test-e2e-fake-key"}]}},
            "oagw": {"config": {"proxy_timeout_secs": 30, "allow_http_upstream": True, "ssrf_policy": {"enabled": False}}},
            "mini-chat": {
                "database": {"server": "sqlite_mc", "file": "mini_chat.db"},
                "config": {
                    "vendor": "constructorfabric",
                    "client_credentials": {"client_id": "mini-chat", "client_secret": "mini-chat-dev-secret"},
                    "providers": {
                        "openai": {
                            "kind": "openai_responses",
                            "host": "127.0.0.1",
                            "port": mock_port,
                            "use_http": True,
                            "upstream_alias": "mock-openai",
                            "api_path": "/v1/responses",
                            "storage_kind": "openai",
                            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                            "auth_config": {"header": "authorization", "prefix": "Bearer ", "secret_ref": "cred://openai-key"},
                        }
                    },
                    "orphan_watchdog": {"scan_interval_secs": 1, "timeout_secs": 90},
                    "upload_reaper": {"scan_interval_secs": 1, "stale_after_secs": 60},
                    "thread_summary_worker": {"enabled": True, "summary_model_id": "std-tiny"},
                    "streaming": {"sse_ping_interval_seconds": 5},
                    "quota": {"web_search_daily_quota": 75},
                    "rag": {
                        "max_documents_per_chat": 3,
                        "max_images_per_message": 2,
                        "uploaded_file_max_size_kb": 2048,
                        "uploaded_image_max_size_kb": 256,
                        "max_total_upload_mb_per_chat": 5,
                    },
                },
            },
            "static-mini-chat-audit-plugin": {"config": {"vendor": "constructorfabric", "priority": 100, "enabled": True}},
            "static-mini-chat-model-policy-plugin": {"config": policy_cfg},
        },
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
    }
    if tweak is not None:
        tweak(cfg)
    return cfg


# ---------------------------------------------------------------------------
# Server process handle
# ---------------------------------------------------------------------------


class ServerHandle:
    def __init__(
        self, name: str, mock: MockLLM, kill_switches: Optional[dict] = None, tweak: Optional[Callable[[dict], None]] = None
    ):
        self.name = name
        self.mock = mock
        self.kill_switches = kill_switches
        self.tweak = tweak
        self.port = free_port()
        self.home = Path(tempfile.mkdtemp(prefix=f"mc-e2e-{name}-"))
        self.config_path = self.home / "config.yaml"
        self.log_path = self.home / "server.out.log"
        self.proc: Optional[subprocess.Popen] = None
        self._log_fh = None
        self.binary = Path(os.environ.get("MC_E2E_BINARY", str(DEFAULT_BINARY)))
        self.db = DB(self._db_path)

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def _db_path(self) -> Path:
        direct = self.home / "mini-chat" / "mini_chat.db"
        if direct.exists():
            return direct
        found = sorted(self.home.glob("**/mini_chat.db"))
        assert found, f"mini_chat.db not found under {self.home}"
        return found[0]

    def write_config(self) -> None:
        cfg = build_config(self.home, self.port, self.mock.port, self.kill_switches, self.tweak)
        self.config_path.write_text(yaml.safe_dump(cfg, sort_keys=False))

    def log_tail(self, n: int = 40) -> str:
        chunks = [f"(full logs under {self.home}; set MC_E2E_KEEP_HOME=1 to keep them)"]
        noise = ('"target":"access_log"', "canonical_error_layer", "access_log:")
        for p in [self.log_path, *sorted((self.home / "logs").glob("*.log"))]:
            try:
                lines = [ln for ln in p.read_text(errors="replace").splitlines() if not any(x in ln for x in noise)][-n:]
                chunks.append(f"--- {p.name} ---\n" + "\n".join(lines))
            except FileNotFoundError:
                pass
        return "\n".join(chunks)

    def start(self) -> "ServerHandle":
        try:
            return self._start()
        except BaseException:
            self.stop()  # never leave a half-started server behind
            raise

    def _start(self) -> "ServerHandle":
        if not self.binary.exists():
            raise RuntimeError(
                f"server binary {self.binary} not found; build it with: cargo build --bin cf-gears-example-server "
                "--no-default-features --features mini-chat,static-authn,static-authz,single-tenant,static-credstore"
            )
        self.write_config()
        self._log_fh = open(self.log_path, "wb")
        env = dict(os.environ)
        env.setdefault("RUST_BACKTRACE", "1")
        self.proc = subprocess.Popen(
            [str(self.binary), "--config", str(self.config_path), "run"],
            stdout=self._log_fh,
            stderr=subprocess.STDOUT,
            cwd=str(self.home),
            env=env,
        )
        atexit.register(self.stop)  # safety net: stop by the Popen handle even on abnormal exit
        deadline = time.time() + STARTUP_TIMEOUT
        last = None
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"server '{self.name}' exited early with code {self.proc.returncode}\n{self.log_tail()}"
                )
            try:
                r = httpx.get(f"{self.base_url}/healthz", timeout=2)
                if r.status_code == 200:
                    break
                last = f"status {r.status_code}"
            except httpx.HTTPError as e:
                last = repr(e)
            time.sleep(0.5)
        else:
            raise RuntimeError(f"server '{self.name}' not healthy after {STARTUP_TIMEOUT}s ({last})\n{self.log_tail()}")
        self._wait_gear_ready()
        self._provision_secret()
        return self

    def _provision_secret(self) -> None:
        """credstore only resolves secrets that have a metadata row, so the
        provider key is created through its API after start (see
        config/quickstart.yaml); mini-chat retries the deferred provider."""
        body = {"reference": "openai-key", "value": "sk-test-e2e-fake-key", "sharing": "tenant"}
        r = httpx.post(
            f"{self.base_url}/credstore/v1/secrets",
            headers={"Authorization": "Bearer tok-a", "Content-Type": "application/json"},
            json=body,
            timeout=10,
        )
        if r.status_code not in (201, 409):
            raise RuntimeError(f"creating the provider secret failed: {r.status_code} {r.text}")

    def _wait_gear_ready(self) -> None:
        """/healthz may answer before the gear routes are mounted; probe one route."""
        deadline = time.time() + 60
        while time.time() < deadline:
            try:
                r = httpx.get(f"{self.base_url}/mini-chat/v1/models", headers={"Authorization": "Bearer tok-a"}, timeout=5)
                if r.status_code == 200:
                    return
            except httpx.HTTPError:
                pass
            if self.proc is not None and self.proc.poll() is not None:
                raise RuntimeError(f"server '{self.name}' exited\n{self.log_tail()}")
            time.sleep(0.5)
        raise RuntimeError(f"mini-chat routes of '{self.name}' never answered 200 on /v1/models\n{self.log_tail()}")

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        self.proc = None
        if self._log_fh:
            self._log_fh.close()
            self._log_fh = None

    def cleanup_home(self) -> None:
        if os.environ.get("MC_E2E_KEEP_HOME"):
            print(f"[mc-e2e] keeping {self.home}")
            return
        shutil.rmtree(self.home, ignore_errors=True)

    def api(self, token: Optional[str]) -> Api:
        return Api(self.base_url, token)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def pytest_configure(config):
    config.addinivalue_line("markers", "noserver: test does not need the gear server (mock self-tests)")
    config.addinivalue_line("markers", "slow: test takes more than ~20 s")


@pytest.fixture(scope="session")
def mock_llm():
    m = MockLLM().start()
    yield m
    m.stop()


@pytest.fixture(scope="session")
def server(mock_llm):
    h = ServerHandle("main", mock_llm)
    h.start()
    yield h
    h.stop()
    h.cleanup_home()


_KS = {"handle": None}


@pytest.fixture(scope="session")
def ks_server(mock_llm, server):
    """Secondary server with kill switches on (started on first use)."""
    if _KS["handle"] is None:
        h = ServerHandle(
            "killswitch",
            mock_llm,
            kill_switches={
                "disable_web_search": True,
                "disable_images": True,
                "disable_code_interpreter": True,
                "force_standard_tier": True,
            },
        )
        h.start()
        _KS["handle"] = h
    yield _KS["handle"]
    h = _KS["handle"]
    if h is not None:
        h.stop()
        h.cleanup_home()
        _KS["handle"] = None


_KB = {"handle": None}
KB_VECTOR_STORE = "vs_kb_e2e"


def _enable_knowledge_search(cfg: dict) -> None:
    mc = cfg["gears"]["mini-chat"]["config"]
    mc["providers"]["openai"]["api_version"] = "2025-04-01-preview"
    mc["knowledge_search"] = {
        "enabled": True,
        "provider_id": "openai",
        "vector_store_id": KB_VECTOR_STORE,
        "max_calls_per_message": 2,
        "top_k": 3,
    }


@pytest.fixture(scope="session")
def kb_server(mock_llm, server):
    """Secondary server with knowledge search enabled (started on first use)."""
    if _KB["handle"] is None:
        h = ServerHandle("knowledge", mock_llm, tweak=_enable_knowledge_search)
        h.start()
        _KB["handle"] = h
    yield _KB["handle"]
    h = _KB["handle"]
    if h is not None:
        h.stop()
        h.cleanup_home()
        _KB["handle"] = None


@pytest.fixture()
def kb_api_for(kb_server) -> Callable[[str], Api]:
    clients = []

    def make(token: Optional[str]) -> Api:
        c = kb_server.api(token)
        clients.append(c)
        return c

    yield make
    for c in clients:
        c.close()


@pytest.fixture(autouse=True)
def _mock_hygiene(request, mock_llm):
    """Each test starts with a clean mock config and no hanging streams."""
    mock_llm.configure(**{k: None for k in mock_llm.config()})
    yield
    mock_llm.release_hangs()
    mock_llm.configure(**{k: None for k in mock_llm.config()})


@pytest.fixture()
def api_for(server) -> Callable[[str], Api]:
    clients = []

    def make(token: Optional[str]) -> Api:
        c = server.api(token)
        clients.append(c)
        return c

    yield make
    for c in clients:
        c.close()


@pytest.fixture()
def api(api_for) -> Api:
    """Client for user A (tok-a)."""
    return api_for("tok-a")


@pytest.fixture()
def db(server) -> DB:
    return server.db


@pytest.fixture()
def ks_api_for(ks_server) -> Callable[[str], Api]:
    clients = []

    def make(token: Optional[str]) -> Api:
        c = ks_server.api(token)
        clients.append(c)
        return c

    yield make
    for c in clients:
        c.close()
