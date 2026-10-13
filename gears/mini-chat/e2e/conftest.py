"""Pytest fixtures for the mini-chat black-box suite.

* ``mc``          — session-scoped server with the default options (shared by most tests).
* ``mc_factory``  — starts extra servers with custom ``ServerOptions`` (stopped at session end).

Tests must not rely on global state of the shared server beyond what they create: use a
fresh chat per test and call ``mc.mock_reset()`` before scripting the provider.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from harness import MiniChat, ServerOptions  # noqa: E402


@pytest.fixture(scope="session")
def mc():
    server = MiniChat().start()
    yield server
    server.stop()


@pytest.fixture(scope="session")
def mc_factory():
    started: dict[str, MiniChat] = {}

    def make(key: str, opts: ServerOptions) -> MiniChat:
        if key not in started:
            started[key] = MiniChat(opts).start()
        return started[key]

    yield make
    for s in started.values():
        s.stop()


@pytest.fixture
def fresh(mc):
    """Shared server with the mock provider reset."""
    mc.mock_reset()
    return mc


# ── dedicated servers ──────────────────────────────────────────────────────
# Each is started lazily once per session and reset (mock) per test through the function fixture.

from harness import catalog_entry, default_catalog  # noqa: E402

KILL_SWITCHES = {
    "disable_web_search": True,
    "disable_images": True,
    "disable_code_interpreter": True,
    "disable_file_search": True,
    "disable_premium_tier": True,
}


def limits_catalog() -> list[dict]:
    cat = default_catalog()
    # No separate input limit; input budget = 5000 - 4000 - overhead (≈900 tokens).
    cat.append(catalog_entry("budget-test", "Standard", context_window=5000, max_output_tokens=4000, max_input_tokens=0,
                             web_search=False, file_search=False, code_interpreter=False))
    # max_output_tokens_applied >= context_window: every request is over budget.
    cat.append(catalog_entry("no-room", "Standard", context_window=1000, max_output_tokens=2000, max_input_tokens=0,
                             web_search=False, file_search=False, code_interpreter=False))
    return cat


LIMITS_OPTS = dict(
    catalog=limits_catalog(),
    gear_overrides={
        "rag": {
            "max_documents_per_chat": 3,
            "max_total_upload_mb_per_chat": 1,
            "uploaded_file_max_size_kb": 700,
            "uploaded_image_max_size_kb": 64,
            "max_images_per_message": 2,
        },
        "streaming": {"sse_ping_interval_seconds": 5},
    },
)


def _fresh(srv: MiniChat) -> MiniChat:
    srv.mock_reset()
    return srv


@pytest.fixture(scope="session")
def quota_server(mc_factory):
    """Isolated quota accounting (tests seed and restore quota rows)."""
    return mc_factory("quota", ServerOptions())


@pytest.fixture
def qs(quota_server):
    return _fresh(quota_server)


@pytest.fixture(scope="session")
def ks_server(mc_factory):
    """Kill switches on: web search, images, code interpreter, file search, premium tier."""
    return mc_factory("killswitch", ServerOptions(kill_switches=dict(KILL_SWITCHES)))


@pytest.fixture
def ks(ks_server):
    return _fresh(ks_server)


@pytest.fixture(scope="session")
def force_std_server(mc_factory):
    return mc_factory("force-standard", ServerOptions(kill_switches={"force_standard_tier": True}))


@pytest.fixture
def fs_srv(force_std_server):
    return _fresh(force_std_server)


@pytest.fixture(scope="session")
def limits_server(mc_factory):
    """Small RAG limits, 5 s SSE ping interval and budget-test models."""
    return mc_factory("limits", ServerOptions(**LIMITS_OPTS))


@pytest.fixture
def lim(limits_server):
    return _fresh(limits_server)
