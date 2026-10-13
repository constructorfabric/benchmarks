"""Fixtures for the mini-chat black-box e2e suite.

Self-contained: it does not use the repo-wide ``test_env`` / ``gear_test_env``
fixtures (it may coexist with them in one pytest session, but never shares or
starts their servers).  See README.md.
"""

from __future__ import annotations

import copy
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import pytest
import requests
import yaml

# Package-relative imports: the repo-wide testing/e2e/helpers/ package would otherwise shadow ``helpers``.
from . import helpers
from .mock_provider import MockProvider, start_mock

PROJECT_ROOT = Path(__file__).resolve().parents[4]  # suites/mini_chat_dev -> repo root
BASE_CONFIG = PROJECT_ROOT / "config" / "e2e-local.yaml"
DEFAULT_BINARY = PROJECT_ROOT / "target" / "debug" / "cf-gears-example-server"
PORT = int(os.environ.get("MINI_CHAT_E2E_PORT", "8086"))
HEALTH_TIMEOUT_S = float(os.environ.get("MINI_CHAT_E2E_HEALTH_TIMEOUT", "90"))

VENDOR = "constructorfabric"


# ── generated configuration ─────────────────────────────────────────────────


def _model(id_: str, *, tier: str = "Standard", provider: str = "openai", enabled: bool = True,
           vision: bool = True, web_search: bool = False, file_search: bool = False,
           code_interpreter: bool = False, context_window: int = 128000, max_output_tokens: int = 16384,
           max_input_tokens: int = 120000, is_default: bool = False, sort_order: int = 0,
           provider_model_id: str | None = None, in_mult: int = 1_000_000, out_mult: int = 3_000_000) -> dict:
    premium = tier == "Premium"
    return {
        "id": id_,
        "provider_model_id": provider_model_id or id_,
        "display_name": id_,
        "description": f"e2e model {id_}",
        "provider_id": provider,
        "provider_display_name": provider,
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": "You are a helpful assistant.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"] if vision else [],
        "context_window": context_window,
        "max_output_tokens": max_output_tokens,
        "max_input_tokens": max_input_tokens,
        "input_tokens_credit_multiplier_micro": 3_000_000 if premium else in_mult,
        "output_tokens_credit_multiplier_micro": 15_000_000 if premium else out_mult,
        "multiplier_display": "3x" if premium else "1x",
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
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0,
                           "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": True, "structured_output": True},
            "tool_support": {"web_search": web_search, "file_search": file_search,
                             "image_generation": False, "code_interpreter": code_interpreter, "mcp": False},
            "supported_endpoints": {"chat_completions": True, "responses": True, "embeddings": False,
                                    "image_generation": False, "audio_speech_generation": False,
                                    "audio_transcription": False, "audio_translation": False},
        },
        "preference": {"is_default": is_default, "sort_order": sort_order},
    }


def model_catalog() -> list[dict]:
    """The e2e catalog (see README.md for what each model is for)."""
    return [
        _model("gpt-premium", tier="Premium", web_search=True, file_search=True, code_interpreter=True,
               is_default=True, sort_order=0),
        _model("gpt-standard", file_search=True, code_interpreter=True, sort_order=1),
        _model("gpt-novision", vision=False, sort_order=2),
        _model("gpt-azure", provider="azure", sort_order=3),
        _model("gpt-tiny", context_window=4096, max_output_tokens=1024, max_input_tokens=3072, sort_order=4),
        _model("gpt-disabled", enabled=False, sort_order=5),
        # Summary model (thread_summary_worker.summary_model_id default); must be enabled.
        _model("gpt-4.1-mini", sort_order=6),
    ]


def build_config(mock_port: int, port: int, home: Path) -> dict:
    """``config/e2e-local.yaml`` patched for the mini-chat e2e run."""
    cfg = copy.deepcopy(yaml.safe_load(BASE_CONFIG.read_text()))
    gears = cfg["gears"]

    cfg["server"]["home_dir"] = str(home / ".cf-gears")
    gears["api-gateway"]["config"]["bind_addr"] = f"127.0.0.1:{port}"
    # Tests create chats in bursts; lift the production-like throttle.
    zones = gears["api-gateway"]["config"]
    zones["rate_limit_zones"]["rl_mini_chat_chat"].update(rate_limit="1000/s", burst_limit=1000)
    # Known boot issue: the seeded entity extends an account-management schema that is
    # not part of this feature set and makes the server exit.
    gears["types-registry"]["config"]["entities"] = []
    gears["grpc-hub"]["config"]["listen_addr"] = f"uds://{home}/grpc.sock"

    oagw = gears.setdefault("oagw", {}).setdefault("config", {})
    oagw.update(proxy_timeout_secs=10, allow_http_upstream=True)
    oagw["ssrf_policy"] = {"enabled": False}

    openai = {"kind": "openai_responses", "host": "127.0.0.1", "port": mock_port, "use_http": True,
              "upstream_alias": "mock-openai", "storage_kind": "openai"}
    azure = {"kind": "openai_responses", "host": "127.0.0.1", "port": mock_port, "use_http": True,
             "upstream_alias": "mock-azure", "storage_kind": "azure",
             "api_path": "/openai/v1/responses", "api_version": "2025-03-01-preview"}
    mc = gears["mini-chat"]["config"]
    mc["vendor"] = VENDOR
    mc["providers"] = {"openai": openai, "azure": azure}
    mc["orphan_watchdog"] = {"timeout_secs": 90, "scan_interval_secs": 1}
    mc["upload_reaper"] = {"scan_interval_secs": 1, "stale_after_secs": 60}
    # Minimum ping interval: OAGW's proxy_timeout_secs (10) ends a silent upstream
    # earlier than the default 15 s, so pings are only observable below it.
    mc["streaming"] = {"sse_ping_interval_seconds": 5}

    gears["static-mini-chat-audit-plugin"] = {"config": {"vendor": VENDOR, "priority": 100, "enabled": True}}
    gears["static-mini-chat-model-policy-plugin"] = {"config": {
        "vendor": VENDOR, "priority": 100, "model_catalog": model_catalog()}}
    return cfg


def pytest_collection_modifyitems(config, items):
    """The repo pytest.ini caps tests at 10 s (fixture setup included); the server boot and
    streaming scenarios need more.  Raise the default for this suite only."""
    here = Path(__file__).parent
    for item in items:
        if here in Path(str(item.fspath)).parents and item.get_closest_marker("timeout") is None:
            item.add_marker(pytest.mark.timeout(int(os.environ.get("MINI_CHAT_E2E_TEST_TIMEOUT", "120"))))


# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture(scope="session")
def mock():
    """Started mock OpenAI/Azure provider (``mock_provider.MockProvider``), on a free port."""
    m = start_mock()
    try:
        yield m
    finally:
        m.stop()


@pytest.fixture
def reset_mock(mock: MockProvider):
    """Clear mock queues/recorded requests before (and after) the test; yields the mock."""
    mock.reset()
    yield mock
    mock.reset()


class ServerProcess:
    """The gear server process the suite controls.  Only ever signals its own pid."""

    def __init__(self, binary: Path, config_path: Path, log_path: Path, env: dict, base_url: str) -> None:
        self.binary, self.config_path, self.log_path = binary, config_path, log_path
        self.env, self.base_url = env, base_url
        self.proc: subprocess.Popen | None = None

    def start(self) -> None:
        """Start the server (log appended) and wait for ``/healthz``; fail the test otherwise."""
        with open(self.log_path, "ab") as log:
            self.proc = subprocess.Popen([str(self.binary), "--config", str(self.config_path), "run"],
                                         cwd=str(PROJECT_ROOT), stdout=log, stderr=subprocess.STDOUT,
                                         env=self.env)
        deadline = time.monotonic() + HEALTH_TIMEOUT_S
        while True:
            if self.proc.poll() is not None:
                pytest.fail(f"server exited with {self.proc.returncode} during boot; log {self.log_path}:\n"
                            + self.log_path.read_text(errors="replace")[-4000:])
            try:
                if requests.get(f"{self.base_url}/healthz", timeout=2).status_code == 200:
                    return
            except requests.RequestException:
                pass
            if time.monotonic() > deadline:
                pytest.fail(f"server not healthy after {HEALTH_TIMEOUT_S}s; log {self.log_path}:\n"
                            + self.log_path.read_text(errors="replace")[-4000:])
            time.sleep(0.5)

    def kill(self) -> None:
        """Crash: SIGKILL the server's own pid (no graceful shutdown) and reap it."""
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=10)

    def stop(self) -> None:
        """Graceful stop (SIGTERM, SIGKILL after 20 s) of the server's own pid."""
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)

    def crash_and_restart(self) -> None:
        """Kill the server (as a crash would) and start it again over the same DB and config."""
        self.kill()
        self.start()


@pytest.fixture(scope="session")
def server(mock: MockProvider):
    """Boot ``cf-gears-example-server`` with the generated config; yield its base URL."""
    binary = Path(os.environ.get("E2E_BINARY") or DEFAULT_BINARY)
    if not binary.exists():
        pytest.fail(f"server binary not found: {binary} (build it or set E2E_BINARY)")

    tmp = Path(tempfile.mkdtemp(prefix="mini-chat-e2e-"))
    config_path = tmp / "config.yaml"
    config_path.write_text(yaml.safe_dump(build_config(mock.port, PORT, tmp), sort_keys=False))
    log_path = tmp / "server.log"
    env = {**os.environ, "HOME": str(tmp)}
    base_url = f"http://127.0.0.1:{PORT}"
    srv = ServerProcess(binary, config_path, log_path, env, base_url)
    try:
        srv.start()
        helpers.STATE.update(base_url=base_url, home=str(tmp), log=str(log_path), config=str(config_path),
                             server=srv)
        yield base_url
    finally:
        # Only ever signal the pid we started (never pkill/pgrep -f).
        srv.stop()
        helpers.STATE.update(base_url=None, server=None)
        if not os.environ.get("MINI_CHAT_E2E_KEEP"):  # keep config/log/DB for debugging when set
            shutil.rmtree(tmp, ignore_errors=True)


@pytest.fixture
def server_ctl(server) -> ServerProcess:
    """The running ``ServerProcess`` (``crash_and_restart()``, ``kill()``, ``start()``).  A test that
    restarts the server must leave it running for the rest of the session."""
    return helpers.STATE["server"]


@pytest.fixture(scope="module")
def outbox_capture(server, request):
    """Durable copy of every outbox message enqueued while the module runs (``helpers.outbox_capture``).

    The capture table is named by the test module's ``CAPTURE`` constant; trigger and table are
    dropped when the module finishes. Yields the table name (read it with ``helpers.captured_outbox``).
    """
    table = getattr(request.module, "CAPTURE", None)
    assert table, f"{request.module.__name__} must define CAPTURE (capture table name)"
    with helpers.outbox_capture(table) as t:
        yield t
