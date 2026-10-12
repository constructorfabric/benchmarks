"""Models API black-box tests (acceptance: "Read-only model list/get reflects only
enabled catalog entries, without exposing internal fields").

Source: DESIGN 3.3 "Models API" (visibility algorithm, non-exposure rules, error
table), ADR-0004 (RFC 9457 problems), OpenAPI ``ModelDto`` / ``ModelListDto``.

The catalog is served by the bundled static model-policy plugin from
``catalog.py``: ``mock-premium`` and ``mock-standard`` are enabled,
``mock-disabled`` is not.  Catalog *order* is not part of the public contract
(DESIGN: "Catalog ordering is not part of the public contract beyond the
default-model fallback"), so list assertions compare sets.
"""

from __future__ import annotations

import pytest

from . import catalog
from .support import RESOURCE_TYPES, assert_problem, bearer

REQUIRED_KEYS = {"model_id", "display_name", "tier", "multiplier_display",
                 "multimodal_capabilities", "context_window"}
ALLOWED_KEYS = REQUIRED_KEYS | {"description"}

# Wire names that must never leak (DESIGN 3.3 "Non-Exposure Rules").
FORBIDDEN_FRAGMENTS = [
    "provider", "provider_model_id", "provider_id", catalog.SECRET_PROVIDER_MODEL_PREFIX,
    "Mock Provider Inc.", catalog.SECRET_SYSTEM_PROMPT,
    "credit_multiplier", "credits_micro", "3000000", "15000000",
    "policy_version", "max_output", "max_input", "is_default", "enabled", "sort_order",
]


def items_by_id(body):
    return {item["model_id"]: item for item in body["items"]}


# --------------------------------------------------------------------------- #
# GET /v1/models
# --------------------------------------------------------------------------- #


@pytest.mark.smoke
def test_list_returns_only_enabled_catalog_entries(api):
    resp = api.get("/v1/models")

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    ids = set(items_by_id(resp.json()))
    assert ids == set(catalog.ENABLED_IDS)
    assert catalog.DISABLED_ID not in ids


def test_list_body_is_the_items_envelope(api):
    body = api.get("/v1/models").json()
    assert set(body) == {"items"}
    assert isinstance(body["items"], list)


def test_list_items_have_the_public_projection_and_nothing_else(api):
    for item in api.get("/v1/models").json()["items"]:
        keys = set(item)
        assert REQUIRED_KEYS <= keys, f"missing {REQUIRED_KEYS - keys} in {item}"
        assert keys <= ALLOWED_KEYS, f"non-public fields exposed: {keys - ALLOWED_KEYS}"


@pytest.mark.parametrize("model_id", catalog.ENABLED_IDS)
def test_list_item_values_come_from_the_catalog(api, model_id):
    item = items_by_id(api.get("/v1/models").json())[model_id]
    assert item == catalog.public_projection(model_id)


def test_tier_is_lowercase_standard_or_premium(api):
    items = items_by_id(api.get("/v1/models").json())
    assert items[catalog.PREMIUM_ID]["tier"] == "premium"
    assert items[catalog.STANDARD_ID]["tier"] == "standard"


def test_context_window_is_an_integer_and_capabilities_an_array(api):
    for item in api.get("/v1/models").json()["items"]:
        assert isinstance(item["context_window"], int) and item["context_window"] > 0
        assert isinstance(item["multimodal_capabilities"], list)
        assert all(isinstance(c, str) for c in item["multimodal_capabilities"])


def test_internal_catalog_fields_are_never_exposed(api):
    list_text = api.get("/v1/models").text
    get_text = api.get(f"/v1/models/{catalog.PREMIUM_ID}").text
    for text in (list_text, get_text):
        for fragment in FORBIDDEN_FRAGMENTS:
            assert fragment not in text, f"`{fragment}` leaked into: {text}"


def test_the_catalog_is_global_for_every_tenant_and_user(api):
    expected = set(catalog.ENABLED_IDS)
    for user in ("a", "a_reviewer", "b"):
        resp = api.get("/v1/models", user=user)
        assert resp.status_code == 200, f"{user}: {resp.text}"
        assert set(items_by_id(resp.json())) == expected, user


def test_listing_models_never_calls_the_llm_provider(api, mock_provider):
    api.get("/v1/models")
    api.get(f"/v1/models/{catalog.PREMIUM_ID}")
    assert mock_provider.requests == []


# --------------------------------------------------------------------------- #
# GET /v1/models/{id}
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("model_id", catalog.ENABLED_IDS)
def test_get_returns_the_same_projection_as_the_list(api, model_id):
    resp = api.get(f"/v1/models/{model_id}")

    assert resp.status_code == 200, resp.text
    assert resp.headers["content-type"].startswith("application/json")
    assert resp.json() == catalog.public_projection(model_id)
    assert resp.json() == items_by_id(api.get("/v1/models").json())[model_id]


def test_get_unknown_model_is_a_not_found_problem_for_the_model_resource(api):
    resp = api.get("/v1/models/does-not-exist")

    problem = assert_problem(resp, 404, "not_found")
    assert problem["context"]["resource_type"] == RESOURCE_TYPES["model"]
    assert problem["context"]["resource_type"] == "gts.cf.core.mini_chat.model.v1~"


def test_get_disabled_model_is_indistinguishable_from_an_unknown_one(api):
    disabled = api.get(f"/v1/models/{catalog.DISABLED_ID}")
    unknown = api.get("/v1/models/never-existed")

    d = assert_problem(disabled, 404, "not_found")
    u = assert_problem(unknown, 404, "not_found")
    assert d["context"]["resource_type"] == RESOURCE_TYPES["model"]
    assert (d["type"], d["title"], d["status"]) == (u["type"], u["title"], u["status"])
    assert catalog.SECRET_PROVIDER_MODEL_PREFIX not in disabled.text




# --------------------------------------------------------------------------- #
# Authentication / routing
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("path", ["/v1/models", f"/v1/models/{catalog.PREMIUM_ID}"])
def test_missing_bearer_token_is_401_unauthenticated(api, path):
    resp = api.get(path, user=None)

    problem = assert_problem(resp, 401, "unauthenticated")
    assert problem["context"].get("reason") == "MISSING_BEARER"


@pytest.mark.parametrize("path", ["/v1/models", f"/v1/models/{catalog.PREMIUM_ID}"])
def test_invalid_bearer_token_is_401_unauthenticated(api, path):
    resp = api.get(path, user=None, headers={"Authorization": "Bearer not-a-configured-token"})

    problem = assert_problem(resp, 401, "unauthenticated")
    assert problem["context"].get("reason") == "AUTHN_FAILED"


def test_a_valid_token_of_any_configured_user_is_accepted(api):
    for user in ("a", "a_reviewer", "b"):
        assert api.get("/v1/models", headers=bearer(user)).status_code == 200


def test_routes_exist_only_under_the_gear_url_prefix(api):
    outside = api.get("/v1/models", raw_path=True)
    assert outside.status_code == 404, outside.text
    assert api.get("/v1/models").status_code == 200


def test_model_ids_are_not_resolved_under_other_resource_paths(api):
    resp = api.get(f"/v1/models/{catalog.PREMIUM_ID}/extra")
    assert resp.status_code == 404
