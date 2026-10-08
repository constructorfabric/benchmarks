"""Models API (acceptance: Models API)."""

import pytest

from mchelpers import DISABLED_MODEL, ENABLED_MODELS, PREMIUM_MODELS, RT_MODEL, assert_problem

ALLOWED = {"model_id", "display_name", "tier", "multiplier_display", "description", "multimodal_capabilities", "context_window"}
REQUIRED = ALLOWED - {"description"}
FORBIDDEN = {
    "provider_id",
    "provider_model_id",
    "provider",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "credits_micro",
    "policy_version",
    "max_output_tokens",
    "max_output",
    "is_default",
    "preference",
    "general_config",
    "system_prompt",
    "estimation_budgets",
    "enabled",
}


def _assert_projection(m: dict):
    assert REQUIRED <= set(m), m
    assert set(m) <= ALLOWED, f"unexpected model fields: {set(m) - ALLOWED}"
    assert not (FORBIDDEN & set(m))
    assert m["tier"] in ("standard", "premium")
    assert isinstance(m["multimodal_capabilities"], list)
    assert isinstance(m["context_window"], int)
    assert not m["model_id"].startswith("mock-"), "model_id must not be the provider model name"


# Acceptance: Models API — list shows only enabled entries, without internal fields
def test_list_models(api):
    r = api.get("/v1/models")
    assert r.status_code == 200, r.text
    items = r.json()["items"]
    ids = {m["model_id"] for m in items}
    assert ids == ENABLED_MODELS
    assert DISABLED_MODEL not in ids
    for m in items:
        _assert_projection(m)
        assert m["tier"] == ("premium" if m["model_id"] in PREMIUM_MODELS else "standard")
    assert "mock-" not in r.text, "provider model ids must not be exposed"


# Acceptance: Models API — values projected from the catalog
def test_model_values(api):
    m = api.get("/v1/models/prem").json()
    _assert_projection(m)
    assert m["model_id"] == "prem"
    assert m["display_name"] == "E2E prem"
    assert m["tier"] == "premium"
    assert m["multiplier_display"] == "3x"
    assert m["context_window"] == 128000
    assert "VISION_INPUT" in m["multimodal_capabilities"]
    assert m.get("description") == "E2E test model prem"
    nv = api.get("/v1/models/std-novision").json()
    assert "VISION_INPUT" not in nv["multimodal_capabilities"]
    assert nv["tier"] == "standard"


# Acceptance: Models API — get of disabled / unknown model is 404 with the model resource type
@pytest.mark.parametrize("model_id", [DISABLED_MODEL, "does-not-exist"])
def test_get_hidden_model(api, model_id):
    assert_problem(api.get(f"/v1/models/{model_id}"), 404, "not_found", resource_type=RT_MODEL)


# Acceptance: Models API — every visible model is retrievable with the same projection
def test_get_each_listed_model(api):
    items = api.get("/v1/models").json()["items"]
    for m in items:
        g = api.get(f"/v1/models/{m['model_id']}")
        assert g.status_code == 200
        assert g.json() == m


# Acceptance: Models API / Authorization — authentication required
def test_models_require_auth(api_for):
    anon = api_for(None)
    r = anon.get("/v1/models")
    assert r.status_code == 401
    bad = api_for("not-a-known-token")
    assert bad.get("/v1/models").status_code == 401
