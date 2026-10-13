"""Fixtures of the mini-chat black-box suite.

One session: a scripted provider (``mock_provider.py``) and the example server
(``cf-gears-example-server``) configured against it, both started as child processes and
stopped by their recorded pid (never by name). Server state (``mini_chat.db``, logs) lives in a
temporary home directory.
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
import pytest

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
SERVER_BIN = Path(
    os.environ.get(
        "MINI_CHAT_SERVER_BIN", REPO / "target" / "debug" / "cf-gears-example-server"
    )
)
STARTUP_TIMEOUT_SECS = 90
VENDOR = "constructorfabric"
PREMIUM_MODEL = "bb-premium"
STANDARD_MODEL = "bb-standard"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def stop(proc: subprocess.Popen) -> None:
    """Stops a child process by its pid (SIGTERM, then SIGKILL)."""
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def wait_http_ok(url: str, proc: subprocess.Popen, log: Path, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(
                f"process exited with {proc.returncode} before {url} answered:\n{tail(log)}"
            )
        try:
            res = httpx.get(url, timeout=2)
            if res.status_code == 200:
                return
            last = f"{res.status_code} {res.text[:300]}"
        except httpx.HTTPError as err:
            last = repr(err)
        time.sleep(0.3)
    raise RuntimeError(f"{url} not ready after {timeout}s (last: {last}):\n{tail(log)}")


def tail(path: Path, lines: int = 60) -> str:
    try:
        return "\n".join(path.read_text(errors="replace").splitlines()[-lines:])
    except OSError:
        return "<no log>"


def catalog_entry(model_id: str, tier: str, premium: bool) -> dict:
    tools = {
        "web_search": premium,
        "file_search": True,
        "image_generation": False,
        "code_interpreter": False,
        "mcp": False,
    }
    return {
        "id": model_id,
        "provider_model_id": f"{model_id}-provider",
        "display_name": f"Black-box {tier}",
        "description": f"{tier} model of the black-box suite",
        "provider_id": "openai",
        "provider_display_name": "Mock OpenAI",
        "tier": tier,
        "enabled": True,
        "system_prompt": "You are a test assistant.",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120000,
        "input_tokens_credit_multiplier_micro": 3000000 if premium else 1000000,
        "output_tokens_credit_multiplier_micro": 15000000 if premium else 3000000,
        "multiplier_display": "3x" if premium else "1x",
        "max_num_results": 5,
        "max_tool_calls": 2,
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
            "features": {"streaming": True, "structured_output": False},
            "tool_support": tools,
            "supported_endpoints": {
                "chat_completions": False,
                "responses": True,
                "embeddings": False,
                "image_generation": False,
                "audio_speech_generation": False,
                "audio_transcription": False,
                "audio_translation": False,
            },
        },
        "preference": {"is_default": premium, "sort_order": 0 if premium else 1},
    }


def server_config(home: Path, api_port: int, mock_port: int) -> dict:
    return {
        "server": {"home_dir": str(home)},
        "database": {
            "servers": {
                "sqlite_bb": {
                    "engine": "sqlite",
                    "params": {"WAL": "true", "synchronous": "NORMAL", "busy_timeout": "5000"},
                    "pool": {"max_conns": 5, "acquire_timeout": "30s"},
                }
            }
        },
        "logging": {"default": {"console_level": "info", "file": "logs/server.log", "file_level": "info"}},
        "opentelemetry": {"tracing": {"enabled": False}, "metrics": {"enabled": False}},
        "gears": {
            "api-gateway": {
                "config": {
                    "bind_addr": f"127.0.0.1:{api_port}",
                    "enable_docs": False,
                    "auth_disabled": True,
                    "defaults": {"body_limit_bytes": 64000000},
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
            "gear-orchestrator": {"config": {}},
            "grpc-hub": {"config": {"listen_addr": f"uds://{home}/grpc.sock"}},
            "authn-resolver": {"config": {"vendor": VENDOR}},
            "authz-resolver": {"config": {"vendor": VENDOR}},
            "static-authn-plugin": {
                "config": {
                    "vendor": VENDOR,
                    "priority": 100,
                    "mode": "accept_all",
                    "s2s_credentials": [
                        {"client_id": "mini-chat", "client_secret": "bb-secret"}
                    ],
                }
            },
            "static-authz-plugin": {"config": {"vendor": VENDOR, "priority": 100}},
            "tenant-resolver": {"config": {"vendor": VENDOR}},
            "single-tenant-tr-plugin": {"config": {"vendor": VENDOR}},
            "credstore": {
                "database": {"server": "sqlite_bb", "file": "credstore.db"},
                "config": {"vendor": VENDOR},
            },
            "static-credstore-plugin": {"config": {"secrets": []}},
            "oagw": {
                "config": {
                    "proxy_timeout_secs": 30,
                    "allow_http_upstream": True,
                    "ssrf_policy": {"enabled": False},
                }
            },
            "mini-chat": {
                "database": {"server": "sqlite_bb", "file": "mini_chat.db"},
                "config": {
                    "vendor": VENDOR,
                    "client_credentials": {"client_id": "mini-chat", "client_secret": "bb-secret"},
                    "thread_summary_worker": {"summary_model_id": STANDARD_MODEL},
                    "providers": {
                        "openai": {
                            "kind": "openai_responses",
                            "storage_kind": "openai",
                            "host": "127.0.0.1",
                            "port": mock_port,
                            "use_http": True,
                        }
                    },
                },
            },
            "static-mini-chat-audit-plugin": {
                "config": {"vendor": VENDOR, "priority": 100, "enabled": True}
            },
            "static-mini-chat-model-policy-plugin": {
                "config": {
                    "vendor": VENDOR,
                    "priority": 100,
                    "model_catalog": [
                        catalog_entry(PREMIUM_MODEL, "premium", True),
                        catalog_entry(STANDARD_MODEL, "standard", False),
                    ],
                }
            },
        },
    }


@pytest.fixture(scope="session")
def workdir():
    with tempfile.TemporaryDirectory(prefix="mini-chat-bb-") as d:
        yield Path(d)


@pytest.fixture(scope="session")
def mock_url(workdir):
    port = free_port()
    log = workdir / "mock.log"
    with open(log, "wb") as out:
        proc = subprocess.Popen(
            [sys.executable, str(HERE / "mock_provider.py"), "--port", str(port)],
            stdout=out,
            stderr=subprocess.STDOUT,
            cwd=workdir,
        )
    (workdir / "mock.pid").write_text(str(proc.pid))
    try:
        url = f"http://127.0.0.1:{port}"
        wait_http_ok(f"{url}/_mock/health", proc, log, 20)
        yield url
    finally:
        stop(proc)


@pytest.fixture(scope="session")
def server(workdir, mock_url):
    """Base URL of the running server; ``server.db`` is the mini-chat SQLite file."""
    if not SERVER_BIN.exists():
        pytest.fail(f"server binary {SERVER_BIN} not built (run run.sh)")
    home = workdir / "home"
    home.mkdir()
    api_port = free_port()
    mock_port = int(mock_url.rsplit(":", 1)[1])
    config = workdir / "server.yaml"
    # JSON is valid YAML.
    config.write_text(json.dumps(server_config(home, api_port, mock_port), indent=2))
    log = workdir / "server.log"
    with open(log, "wb") as out:
        proc = subprocess.Popen(
            [str(SERVER_BIN), "--config", str(config), "run"],
            stdout=out,
            stderr=subprocess.STDOUT,
            cwd=workdir,
        )
    (workdir / "server.pid").write_text(str(proc.pid))
    try:
        base = f"http://127.0.0.1:{api_port}"
        wait_http_ok(f"{base}/mini-chat/v1/models", proc, log, STARTUP_TIMEOUT_SECS)
        yield Server(base, home / "mini-chat" / "mini_chat.db", log, mock_url)
    finally:
        stop(proc)
        keep = os.environ.get("MINI_CHAT_BB_KEEP_LOG")
        if keep:
            Path(keep).write_text(log.read_text(errors="replace"))


class Server:
    def __init__(self, base: str, db: Path, log: Path, mock_url: str):
        self.base = base
        self.db = db
        self.log = log
        self.mock_url = mock_url

    def mock_requests(self) -> list:
        return httpx.get(f"{self.mock_url}/_mock/requests", timeout=5).json()


@pytest.fixture()
def api(server):
    with httpx.Client(base_url=f"{server.base}/mini-chat/v1", timeout=60) as client:
        yield client
