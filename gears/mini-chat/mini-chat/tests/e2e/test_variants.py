"""Behaviour that needs a different server configuration: kill switches and
daily tool quotas (each module-scoped stack runs its own server)."""

import uuid

import pytest

import mc
from conftest import start_stack
from mc import problem_reason
from test_attachments import XLSX, make_png


def _kill_switches(cfg: str) -> str:
    return cfg.replace(
        "      default_standard_limits:",
        "      kill_switches:\n"
        "        disable_web_search: true\n"
        "        disable_images: true\n"
        "        disable_code_interpreter: true\n"
        "        disable_premium_tier: true\n"
        "      default_standard_limits:",
    )


def _limits(cfg: str) -> str:
    cfg = cfg.replace("        web_search_daily_quota: 1000", "        web_search_daily_quota: 1\n        code_interpreter_daily_quota: 1")
    return cfg.replace(
        "      default_standard_limits:",
        "      kill_switches:\n        force_standard_tier: true\n      default_standard_limits:",
    )


@pytest.fixture(scope="module")
def env_ks():
    with start_stack(_kill_switches) as e:
        yield e


@pytest.fixture(scope="module")
def env_lim():
    with start_stack(_limits) as e:
        yield e


def test_kill_switches(env_ks):
    api = mc.Client(env_ks, "user-a")
    mock = mc.Mock(env_ks)
    chat = api.create_chat()
    cid = chat["id"]
    mark = mock.mark()
    s = api.send(cid, "search", web_search={"enabled": True})
    assert s.status == 400
    v = s.body["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("web_search", "FEATURE_DISABLED")
    assert not [x for x in mock.since(mark) if x["path"].endswith("/responses")]
    # Image upload rejected while images are disabled.
    r = api.upload(cid, "p.png", make_png(), "image/png")
    assert r.status_code == 400
    v = r.json()["context"]["violations"][0]
    assert (v["subject"], v["type"]) == ("images", "FEATURE_DISABLED")
    # XLSX-only upload rejected while code interpreter is disabled.
    r = api.upload(cid, "s.xlsx", b"PK\x03\x04", XLSX)
    assert r.status_code == 400 and problem_reason(r.json()) == "CODE_INTERPRETER_UNAVAILABLE"
    # Premium tier disabled: the premium chat is downgraded.
    s = api.send(cid, "hello")
    done = s.first("done")
    assert done["quota_decision"] == "downgrade"
    assert done["downgrade_reason"] == "disable_premium_tier"
    assert done["effective_model"] != "gpt-premium"
    assert done["selected_model"] == "gpt-premium"


def test_force_standard_and_daily_tool_quotas(env_lim):
    api = mc.Client(env_lim, "user-a")
    mock = mc.Mock(env_lim)
    chat = api.create_chat()
    cid = chat["id"]
    s = api.send(cid, "one search #websearch", web_search={"enabled": True})
    done = s.first("done")
    assert done["downgrade_reason"] == "force_standard_tier"
    assert done["effective_model"] == "gpt-standard"
    mark = mock.mark()
    s = api.send(cid, "another search", web_search={"enabled": True})
    assert s.status == 429
    v = s.body["context"]["violations"][0]
    assert v["subject"] == "web_search" and v["description"] == "quota_exceeded"
    assert not [x for x in mock.since(mark) if x["path"].endswith("/responses")]
    # A request that does not use the tool is not rejected by the tool quota.
    assert api.send(cid, "no search").terminal[0] == "done"
