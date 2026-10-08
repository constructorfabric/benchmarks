"""Boot smoke: the gear starts under the e2e config and serves the Models API."""

import pytest

from .helpers import PREFIX, TOKEN_A, TOKEN_B, api, assert_problem, db, find_db_path

pytestmark = pytest.mark.usefixtures("server")


def test_models_listed():
    r = api(TOKEN_A).get(f"{PREFIX}/models")
    assert r.status_code == 200, r.text
    ids = {m["model_id"] for m in r.json()["items"]}
    assert "gpt-disabled" not in ids
    assert {"gpt-premium", "gpt-standard", "gpt-novision", "gpt-azure", "gpt-tiny"} <= ids


def test_models_visible_to_other_tenant():
    r = api(TOKEN_B).get(f"{PREFIX}/models")
    assert r.status_code == 200, r.text


def test_unauthenticated_is_401():
    r = api(None).get(f"{PREFIX}/models")
    assert r.status_code == 401, r.text
    assert_problem(r, 401, "unauthenticated")


def test_unknown_model_is_404_problem():
    r = api(TOKEN_A).get(f"{PREFIX}/models/gpt-disabled")
    assert_problem(r, 404, "not_found", resource_type="gts.cf.core.mini_chat.model.v1~")


def test_db_helper_finds_gear_database():
    assert find_db_path().name == "mini_chat.db"
    with db() as conn:
        tables = {row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    assert "chats" in tables


def test_model_list_and_get_expose_only_public_catalog_fields():
    s = api(TOKEN_A)
    items = s.get(f"{PREFIX}/models").json()["items"]
    assert [m["model_id"] for m in items] == [
        "gpt-premium", "gpt-standard", "gpt-novision", "gpt-azure", "gpt-tiny", "gpt-4.1-mini"]
    public = {"model_id", "display_name", "tier", "multiplier_display", "description",
              "multimodal_capabilities", "context_window"}
    for m in items:
        assert set(m) == public, m
    premium = s.get(f"{PREFIX}/models/gpt-premium")
    assert premium.status_code == 200, premium.text
    assert premium.json() == items[0] == {
        "model_id": "gpt-premium", "display_name": "gpt-premium", "tier": "premium",
        "multiplier_display": "3x", "description": "e2e model gpt-premium",
        "multimodal_capabilities": ["VISION_INPUT"], "context_window": 128000}
    tiny = next(m for m in items if m["model_id"] == "gpt-tiny")
    assert (tiny["tier"], tiny["context_window"], tiny["multimodal_capabilities"]) == ("standard", 4096,
                                                                                       ["VISION_INPUT"])
    # Internal catalog fields (provider ids, prompts, multipliers, budgets) never leak.
    raw = s.get(f"{PREFIX}/models").text
    for secret in ("provider_id", "provider_model_id", "system_prompt", "credit_multiplier",
                   "estimation_budgets", "max_input_tokens", "You are a helpful assistant."):
        assert secret not in raw
