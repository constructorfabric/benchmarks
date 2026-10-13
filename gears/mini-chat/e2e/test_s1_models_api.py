"""S1 foundation: Models API, authentication and startup config validation.

Contract sources (gears/mini-chat/docs/DESIGN.md):

* §3.3 "Models API" (``cpt-cf-mini-chat-interface-models-api``): visibility
  algorithm, ``ModelDto`` projection, non-exposure rules, 404 for disabled or
  unknown models with ``context.resource_type`` of the model type;
* §3.3 REST error mapping / ADR-0004: canonical ``Problem``; 401
  ``MISSING_BEARER`` / ``AUTHN_FAILED`` from the api-gateway;
* §3.8: the Models API is a permission-only check, the catalog is not
  tenant data;
* §3.2 "Gear lifecycle" + Appendix B.1: unknown keys are rejected at the top
  level and in the strict sections, ranges are validated at init, worker
  sections accept unknown keys; the static policy plugin rejects invalid
  credit multipliers and unknown kill switches.
"""

from __future__ import annotations

from typing import Any

import pytest

from mc_helpers import RT_MODEL, assert_problem

ENABLED_MODEL_IDS = {"gpt-4.1", "gpt-4.1-mini", "gpt-text-only"}
DISABLED_MODEL_ID = "gpt-disabled"

MODEL_DTO_REQUIRED = {
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "multimodal_capabilities",
    "context_window",
}
MODEL_DTO_FIELDS = MODEL_DTO_REQUIRED | {"description"}
FORBIDDEN_FIELDS = {
    "provider",
    "provider_id",
    "provider_model_id",
    "provider_display_name",
    "input_tokens_credit_multiplier",
    "output_tokens_credit_multiplier",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "credits_micro",
    "policy_version",
    "max_output",
    "max_output_tokens",
    "is_default",
    "preference",
    "estimation_budgets",
    "general_config",
    "system_prompt",
}

MODELS = "/mini-chat/v1/models"


def _model_path(model_id: str) -> str:
    return f"{MODELS}/{model_id}"


def _catalog_entry(catalog: list[dict[str, Any]], model_id: str) -> dict[str, Any]:
    return next(entry for entry in catalog if entry["id"] == model_id)


def _items_by_id(body: dict[str, Any]) -> dict[str, dict[str, Any]]:
    assert set(body) == {"items"}, f"ModelListDto must only carry 'items': {body}"
    assert isinstance(body["items"], list)
    by_id = {item["model_id"]: item for item in body["items"]}
    assert len(by_id) == len(body["items"]), f"duplicate model_id in {body['items']}"
    return by_id


def _assert_model_dto(item: dict[str, Any], entry: dict[str, Any]) -> None:
    """``item`` is exactly the ModelDto projection of catalog ``entry``."""
    keys = set(item)
    assert MODEL_DTO_REQUIRED <= keys, f"missing ModelDto fields {MODEL_DTO_REQUIRED - keys}: {item}"
    assert keys <= MODEL_DTO_FIELDS, f"unexpected fields {keys - MODEL_DTO_FIELDS}: {item}"
    assert not keys & FORBIDDEN_FIELDS, f"forbidden fields exposed {keys & FORBIDDEN_FIELDS}"

    assert item["model_id"] == entry["id"]
    assert item["display_name"] == entry["display_name"]
    assert item["tier"] == entry["tier"].lower()
    assert item["tier"] in {"premium", "standard"}
    assert item["multiplier_display"] == entry["multiplier_display"]
    assert isinstance(item["context_window"], int)
    assert item["context_window"] == entry["context_window"]

    capabilities = item["multimodal_capabilities"]
    assert isinstance(capabilities, list) and all(isinstance(c, str) for c in capabilities)
    assert set(entry["multimodal_capabilities"]) <= set(capabilities)
    assert ("VISION_INPUT" in capabilities) == ("VISION_INPUT" in entry["multimodal_capabilities"])

    if "description" in entry:
        assert item.get("description") == entry["description"]
    else:
        assert item.get("description") is None, f"no description configured: {item}"


# --------------------------------------------------------------------------
# List / Get
# --------------------------------------------------------------------------


def test_list_models_returns_exactly_the_enabled_catalog_models(mc):
    resp = mc.get(MODELS, "A1")

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    by_id = _items_by_id(resp.json())
    assert set(by_id) == ENABLED_MODEL_IDS
    assert DISABLED_MODEL_ID not in by_id


def test_list_models_items_are_the_model_dto_projection(mc):
    resp = mc.get(MODELS, "A1")

    assert resp.status_code == 200, resp.text
    for model_id, item in _items_by_id(resp.json()).items():
        _assert_model_dto(item, _catalog_entry(mc.catalog, model_id))


def test_list_models_hides_provider_routing_and_credit_internals(mc):
    resp = mc.get(MODELS, "A1")

    assert resp.status_code == 200, resp.text
    raw = resp.text
    for item in resp.json()["items"]:
        assert not set(item) & FORBIDDEN_FIELDS, item
    for entry in mc.catalog:
        assert entry["provider_model_id"] not in raw
        assert entry["provider_display_name"] not in raw
        assert entry["provider_id"] not in raw.lower()
    provider = mc.gear_config["providers"]["openai"]
    assert f'{provider["host"]}:{provider["port"]}' not in raw


def test_model_without_description_omits_description(mc):
    resp = mc.get(MODELS, "A1")

    assert resp.status_code == 200, resp.text
    text_only = _items_by_id(resp.json())["gpt-text-only"]
    assert text_only.get("description") is None
    assert "VISION_INPUT" not in text_only["multimodal_capabilities"]


def test_vision_models_advertise_vision_input(mc):
    by_id = _items_by_id(mc.get(MODELS, "A1").json())

    assert "VISION_INPUT" in by_id["gpt-4.1"]["multimodal_capabilities"]
    assert "VISION_INPUT" in by_id["gpt-4.1-mini"]["multimodal_capabilities"]
    assert by_id["gpt-4.1"]["tier"] == "premium"
    assert by_id["gpt-4.1-mini"]["tier"] == "standard"


@pytest.mark.parametrize("model_id", sorted(ENABLED_MODEL_IDS))
def test_get_model_returns_the_same_projection_as_the_list(mc, model_id):
    listed = _items_by_id(mc.get(MODELS, "A1").json())[model_id]

    resp = mc.get(_model_path(model_id), "A1")

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    item = resp.json()
    _assert_model_dto(item, _catalog_entry(mc.catalog, model_id))
    assert item == listed


def test_get_disabled_model_is_not_found(mc):
    resp = mc.get(_model_path(DISABLED_MODEL_ID), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_MODEL)


def test_get_unknown_model_is_not_found(mc):
    resp = mc.get(_model_path("gpt-does-not-exist"), "A1")

    assert_problem(resp, 404, category="not_found", resource_type=RT_MODEL)


def test_disabled_and_unknown_models_are_indistinguishable(mc):
    disabled = assert_problem(
        mc.get(_model_path(DISABLED_MODEL_ID), "A1"), 404, category="not_found", resource_type=RT_MODEL
    )
    unknown = assert_problem(
        mc.get(_model_path("gpt-does-not-exist"), "A1"), 404, category="not_found", resource_type=RT_MODEL
    )

    assert disabled["type"] == unknown["type"]
    assert disabled["title"] == unknown["title"]
    assert set(disabled["context"]) == set(unknown["context"])


# --------------------------------------------------------------------------
# Authentication and tenancy
# --------------------------------------------------------------------------


@pytest.mark.parametrize("path", [MODELS, _model_path("gpt-4.1")])
def test_models_api_without_bearer_token_is_unauthenticated(mc, path):
    resp = mc.get(path, user=None)

    assert_problem(resp, 401, category="unauthenticated", reason="MISSING_BEARER")


@pytest.mark.parametrize("path", [MODELS, _model_path("gpt-4.1")])
def test_models_api_with_invalid_token_is_unauthenticated(mc, path):
    resp = mc.get(path, user=None, headers={"Authorization": "Bearer mc-e2e-not-a-token"})

    assert_problem(resp, 401, category="unauthenticated", reason="AUTHN_FAILED")


def test_catalog_is_the_same_for_every_tenant_and_user(mc):
    tenant_a = mc.get(MODELS, "A1")
    same_tenant_other_user = mc.get(MODELS, "A2")
    tenant_b = mc.get(MODELS, "B1")

    for resp in (tenant_a, same_tenant_other_user, tenant_b):
        assert resp.status_code == 200, resp.text
    expected = _items_by_id(tenant_a.json())
    assert _items_by_id(same_tenant_other_user.json()) == expected
    assert _items_by_id(tenant_b.json()) == expected


def test_tenant_b_user_can_get_a_single_model(mc):
    resp_a = mc.get(_model_path("gpt-4.1-mini"), "A1")
    resp_b = mc.get(_model_path("gpt-4.1-mini"), "B1")

    assert resp_b.status_code == 200, resp_b.text
    assert resp_b.json() == resp_a.json()


# --------------------------------------------------------------------------
# Startup config validation (gear init / plugin init)
# --------------------------------------------------------------------------


def _gear_patch(section: dict[str, Any]) -> dict[str, Any]:
    return {"gears": {"mini-chat": {"config": section}}}


def _policy_plugin_patch(section: dict[str, Any]) -> dict[str, Any]:
    return {"gears": {"static-mini-chat-model-policy-plugin": {"config": section}}}


INVALID_GEAR_CONFIGS = [
    pytest.param(_gear_patch({"mcp": {}}), id="unknown-top-level-key-mcp"),
    pytest.param(
        _gear_patch({"streaming": {"web_search_context_size": "low"}}),
        id="removed-key-streaming-web_search_context_size",
    ),
    pytest.param(
        _gear_patch({"streaming": {"sse_ping_interval_seconds": 3}}),
        id="sse_ping_interval_seconds-below-5",
    ),
    pytest.param(
        _gear_patch({"orphan_watchdog": {"timeout_secs": 30}}),
        id="orphan_watchdog-timeout_secs-below-90",
    ),
    pytest.param(
        _gear_patch({"outbox": {"num_partitions": 3}}),
        id="outbox-num_partitions-not-power-of-two",
    ),
    pytest.param(
        _gear_patch({"providers": {"openai": {"storage_kind": "azure"}}}),
        id="azure-storage-without-api_version",
    ),
    pytest.param(
        _policy_plugin_patch({"kill_switches": {"disable_everything": True}}),
        id="policy-plugin-unknown-kill-switch",
    ),
]


@pytest.mark.slow
@pytest.mark.parametrize("patch", INVALID_GEAR_CONFIGS)
def test_invalid_config_fails_startup(start_server, patch):
    outcome = start_server(patch, expect_fail=True, name="invalid-config")

    assert outcome.exited_nonzero, outcome.describe()
    assert not outcome.became_ready, outcome.describe()


@pytest.mark.slow
def test_zero_credit_multiplier_in_static_catalog_fails_startup(start_server, model_catalog):
    _catalog_entry(model_catalog, "gpt-4.1-mini")["input_tokens_credit_multiplier_micro"] = 0

    outcome = start_server(
        _policy_plugin_patch({"model_catalog": model_catalog}),
        expect_fail=True,
        name="zero-multiplier",
    )

    assert outcome.exited_nonzero, outcome.describe()
    assert not outcome.became_ready, outcome.describe()


@pytest.mark.slow
@pytest.mark.parametrize(
    "worker", ["upload_reaper", "orphan_watchdog", "thread_summary_worker", "cleanup_worker"]
)
def test_unknown_key_in_worker_section_is_ignored(start_server, worker):
    server = start_server(_gear_patch({worker: {"foo": 1}}), name=f"worker-{worker}")

    resp = server.get(MODELS, "A1")
    assert resp.status_code == 200, resp.text
    assert set(_items_by_id(resp.json())) == ENABLED_MODEL_IDS
