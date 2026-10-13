//! Models API (`GET /v1/models`, `GET /v1/models/{id}`) exercised in-process:
//! the gear's REST router built with a fake model-policy plugin and a fake PDP.
//!
//! Source of truth: DESIGN 3.3 "Models API" (visibility algorithm, non-exposure
//! rules, error table), 2.2 "License Gate", 3.8 (Models resource type,
//! `require_constraints = false`, fail-closed table), ADR-0004 and the OpenAPI
//! schemas `ModelDto` / `ModelListDto` / `ModelTierDto` / `Problem`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use common::{
    FakePdp, FakePolicyPlugin, SECRET_PROVIDER_ID, SECRET_PROVIDER_MODEL_PREFIX, USER_A,
    assert_problem, catalog_entry, catalog_entry_json, ctx_a, get, send,
};
use mini_chat::api::rest::register_models_routes;
use mini_chat::domain::models::ModelsService;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat_sdk::ModelCatalogEntry;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;

const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";
const PDP_MODEL_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
const BASE_LICENSE_FEATURE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

const REQUIRED_KEYS: [&str; 6] = [
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "multimodal_capabilities",
    "context_window",
];
const ALLOWED_KEYS: [&str; 7] = [
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "description",
    "multimodal_capabilities",
    "context_window",
];

struct Harness {
    router: Router,
    plugin: Arc<FakePolicyPlugin>,
    pdp: Arc<FakePdp>,
    openapi: Arc<OpenApiRegistryImpl>,
}

fn harness_with(entries: Vec<ModelCatalogEntry>, pdp: Arc<FakePdp>, prefix: &str) -> Harness {
    let plugin = FakePolicyPlugin::new(entries);
    let gateway = Arc::new(ModelPolicyGateway::from_client(plugin.clone()));
    let service = Arc::new(ModelsService::new(gateway, pdp.enforcer()));
    let openapi = Arc::new(OpenApiRegistryImpl::new());
    let router = register_models_routes(Router::new(), openapi.as_ref(), service, prefix);
    Harness {
        router,
        plugin,
        pdp,
        openapi,
    }
}

fn harness(entries: Vec<ModelCatalogEntry>) -> Harness {
    harness_with(entries, FakePdp::allow(), "/mini-chat")
}

fn three_models() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry("gpt-premium", "Premium", true),
        catalog_entry("gpt-standard", "Standard", true),
        catalog_entry("gpt-off", "Standard", false),
    ]
}

fn ids(items: &Value) -> BTreeSet<String> {
    items["items"]
        .as_array()
        .expect("`items` is an array")
        .iter()
        .map(|i| i["model_id"].as_str().expect("model_id").to_owned())
        .collect()
}

// ── GET /v1/models ───────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_only_globally_enabled_models() {
    let h = harness(three_models());
    let resp = get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;

    assert_eq!(resp.status.as_u16(), 200, "body: {}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(
        ids(&body),
        BTreeSet::from(["gpt-premium".to_owned(), "gpt-standard".to_owned()]),
        "disabled models must be invisible"
    );
}

#[tokio::test]
async fn list_body_is_exactly_the_items_envelope() {
    let h = harness(three_models());
    let body = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["items"], "ModelListDto has only `items`");
}

#[tokio::test]
async fn every_item_has_the_public_projection_and_nothing_else() {
    let h = harness(three_models());
    let body = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    for item in body["items"].as_array().unwrap() {
        let obj = item.as_object().unwrap();
        for key in REQUIRED_KEYS {
            assert!(obj.contains_key(key), "item is missing `{key}`: {item}");
        }
        for key in obj.keys() {
            assert!(
                ALLOWED_KEYS.contains(&key.as_str()),
                "item exposes the non-public field `{key}`: {item}"
            );
        }
    }
}

#[tokio::test]
async fn projection_values_come_from_the_catalog_entry() {
    let h = harness(three_models());
    let body = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    let premium = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["model_id"] == "gpt-premium")
        .expect("premium model listed")
        .clone();

    assert_eq!(premium["display_name"], json!("Display gpt-premium"));
    assert_eq!(premium["tier"], json!("premium"));
    assert_eq!(premium["multiplier_display"], json!("3x"));
    assert_eq!(premium["description"], json!("Description of gpt-premium"));
    assert_eq!(
        premium["multimodal_capabilities"],
        json!(["VISION_INPUT", "RAG"]),
        "capability flags keep their order"
    );
    assert_eq!(premium["context_window"], json!(128_000));
}

#[tokio::test]
async fn tier_is_serialized_as_standard_or_premium() {
    let h = harness(three_models());
    let body = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    let tier_of = |id: &str| -> Value {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["model_id"] == id)
            .unwrap_or_else(|| panic!("{id} not listed"))["tier"]
            .clone()
    };
    assert_eq!(tier_of("gpt-premium"), json!("premium"));
    assert_eq!(tier_of("gpt-standard"), json!("standard"));
}

#[tokio::test]
async fn internal_catalog_fields_never_reach_the_wire() {
    let h = harness(three_models());

    let list = get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;
    let one = get(&h.router, "/mini-chat/v1/models/gpt-premium", &ctx_a()).await;

    for resp in [&list, &one] {
        let raw = resp.raw.as_str();
        for forbidden in [
            SECRET_PROVIDER_ID,
            SECRET_PROVIDER_MODEL_PREFIX,
            "Secret Provider Inc.",
            "SECRET-SYSTEM-PROMPT",
            "7654321",
            "8765432",
            "credits_micro",
            "credit_multiplier",
            "provider_model_id",
            "provider_id",
            "provider",
            "policy_version",
            "max_output",
            "max_input",
            "is_default",
            "enabled",
        ] {
            assert!(
                !raw.contains(forbidden),
                "response leaks `{forbidden}` (DESIGN 3.3 non-exposure rules): {raw}"
            );
        }
    }
}

#[tokio::test]
async fn description_is_optional_and_omitted_when_not_configured() {
    let mut no_description = catalog_entry_json("gpt-nodesc", "Standard", true);
    no_description["description"] = json!("");
    let entries = vec![
        serde_json::from_value(no_description).unwrap(),
        catalog_entry("gpt-desc", "Standard", true),
    ];
    let h = harness(entries);
    let body = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();

    let find = |id: &str| {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["model_id"] == id)
            .unwrap()
            .clone()
    };
    let without = find("gpt-nodesc");
    assert!(
        without
            .get("description")
            .is_none_or(|d| d.is_null() || d == ""),
        "no configured description -> absent (or null): {without}"
    );
    assert_eq!(
        find("gpt-desc")["description"],
        json!("Description of gpt-desc")
    );
}

#[tokio::test]
async fn empty_or_fully_disabled_catalog_yields_an_empty_list() {
    let h = harness(vec![]);
    let resp = get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json(), json!({"items": []}));

    let h = harness(vec![catalog_entry("only-off", "Premium", false)]);
    let resp = get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json(), json!({"items": []}));
}

// ── GET /v1/models/{id} ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_the_same_projection_as_the_list_item() {
    let h = harness(three_models());
    let list = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    let listed = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["model_id"] == "gpt-standard")
        .unwrap()
        .clone();

    let resp = get(&h.router, "/mini-chat/v1/models/gpt-standard", &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "body: {}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    assert_eq!(resp.json(), listed);
}

#[tokio::test]
async fn get_unknown_model_is_a_not_found_problem_for_the_model_resource() {
    let h = harness(three_models());
    let resp = get(&h.router, "/mini-chat/v1/models/does-not-exist", &ctx_a()).await;
    let problem = assert_problem(&resp, 404, "not_found");
    assert_eq!(
        problem["context"]["resource_type"],
        json!(MODEL_RESOURCE_TYPE)
    );
}

#[tokio::test]
async fn get_disabled_model_is_indistinguishable_from_unknown() {
    let h = harness(three_models());
    let disabled = get(&h.router, "/mini-chat/v1/models/gpt-off", &ctx_a()).await;
    let unknown = get(&h.router, "/mini-chat/v1/models/never-existed", &ctx_a()).await;

    let d = assert_problem(&disabled, 404, "not_found");
    let u = assert_problem(&unknown, 404, "not_found");
    assert_eq!(d["context"]["resource_type"], json!(MODEL_RESOURCE_TYPE));
    assert_eq!(d["type"], u["type"]);
    assert_eq!(d["title"], u["title"]);
    assert_eq!(d["status"], u["status"]);
    assert!(
        !disabled.raw.contains(SECRET_PROVIDER_MODEL_PREFIX),
        "a disabled model must not leak catalog details"
    );
}

// ── no local snapshot cache, identity propagation ────────────────────────────

#[tokio::test]
async fn catalog_is_read_from_the_plugin_on_every_request() {
    let h = harness(vec![catalog_entry("first", "Standard", true)]);

    let one = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    assert_eq!(ids(&one), BTreeSet::from(["first".to_owned()]));

    h.plugin
        .set_catalog(vec![catalog_entry("second", "Premium", true)]);

    let two = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    assert_eq!(
        ids(&two),
        BTreeSet::from(["second".to_owned()]),
        "ADR-0008: no snapshot cache - a catalog change is visible immediately"
    );
    assert_eq!(h.plugin.snapshot_calls.load(Ordering::SeqCst), 2);
    assert!(h.plugin.version_calls.load(Ordering::SeqCst) >= 2);

    let gone = get(&h.router, "/mini-chat/v1/models/first", &ctx_a()).await;
    assert_eq!(gone.status.as_u16(), 404);
}

#[tokio::test]
async fn plugin_is_asked_on_behalf_of_the_calling_user() {
    let h = harness(three_models());
    get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;
    let seen = h.plugin.seen_users.lock().unwrap().clone();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|u| *u == USER_A),
        "plugin calls are keyed by the caller's subject id, saw {seen:?}"
    );
}

// ── authorization (PEP) ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_authorizes_action_list_on_the_model_resource_without_constraints() {
    let h = harness(three_models());
    get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;

    let req = h.pdp.last();
    assert_eq!(req.action.name, "list");
    assert_eq!(req.resource.resource_type, PDP_MODEL_TYPE);
    assert!(req.resource.id.is_none(), "list carries no resource id");
    assert!(
        !req.context.require_constraints,
        "Models API is a pure permission check (DESIGN 3.8)"
    );
    assert_eq!(req.subject.id, USER_A);
}

#[tokio::test]
async fn get_authorizes_action_read_on_the_model_resource_without_constraints() {
    let h = harness(three_models());
    get(&h.router, "/mini-chat/v1/models/gpt-standard", &ctx_a()).await;

    let req = h.pdp.last();
    assert_eq!(req.action.name, "read");
    assert_eq!(req.resource.resource_type, PDP_MODEL_TYPE);
    assert!(
        req.resource.id.is_none(),
        "model ids are catalog strings, not UUIDs: no `resource.id` (DESIGN 3.8 matrix)"
    );
    assert!(!req.context.require_constraints);
}

#[tokio::test]
async fn pdp_denial_is_403_permission_denied_and_fail_closed() {
    let h = harness_with(
        three_models(),
        FakePdp::deny("internal-policy-rule-42"),
        "/mini-chat",
    );

    for uri in [
        "/mini-chat/v1/models",
        "/mini-chat/v1/models/gpt-standard",
        "/mini-chat/v1/models/does-not-exist",
    ] {
        let resp = get(&h.router, uri, &ctx_a()).await;
        let problem = assert_problem(&resp, 403, "permission_denied");
        assert_eq!(
            problem["context"]["reason"],
            json!("AUTHZ_DENIED"),
            "{uri}: ADR-0004 reason for a PDP denial"
        );
        assert!(
            !resp.raw.contains("internal-policy-rule-42"),
            "deny_reason.details must not be exposed: {}",
            resp.raw
        );
    }
    assert_eq!(
        h.plugin.total_calls(),
        0,
        "authorization precedes any catalog read (and a denied unknown id is 403, not 404)"
    );
}

#[tokio::test]
async fn unreachable_pdp_is_503_with_retry_after_and_still_fail_closed() {
    let h = harness_with(
        three_models(),
        FakePdp::unreachable("pdp-transport-secret-detail"),
        "/mini-chat",
    );

    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-standard"] {
        let resp = get(&h.router, uri, &ctx_a()).await;
        let problem = assert_problem(&resp, 503, "service_unavailable");
        assert_eq!(
            resp.headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("5"),
            "{uri}: Retry-After: 5"
        );
        assert_eq!(problem["context"]["retry_after_seconds"], json!(5));
        assert!(
            !resp.raw.contains("pdp-transport-secret-detail"),
            "the PDP failure cause is only logged, never returned: {}",
            resp.raw
        );
    }
    assert_eq!(
        h.plugin.total_calls(),
        0,
        "no catalog read after a PDP failure"
    );
}

#[tokio::test]
async fn pdp_denial_and_pdp_outage_are_distinguishable() {
    let denied = harness_with(three_models(), FakePdp::deny("x"), "/mini-chat");
    let down = harness_with(three_models(), FakePdp::unreachable("x"), "/mini-chat");
    let a = get(&denied.router, "/mini-chat/v1/models", &ctx_a()).await;
    let b = get(&down.router, "/mini-chat/v1/models", &ctx_a()).await;
    assert_eq!(a.status.as_u16(), 403);
    assert_eq!(b.status.as_u16(), 503);
}

// ── policy plugin failure ────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_failure_is_a_500_internal_problem_without_the_cause() {
    let h = harness(three_models());
    h.plugin.fail_with("plugin-db-password-leak");

    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-standard"] {
        let resp = get(&h.router, uri, &ctx_a()).await;
        assert_problem(&resp, 500, "internal");
        assert!(
            !resp.raw.contains("plugin-db-password-leak"),
            "internal causes are logged, not returned: {}",
            resp.raw
        );
    }
}

// ── route registration: prefix, operation ids, license gate ──────────────────

#[tokio::test]
async fn routes_live_under_the_configured_url_prefix() {
    let h = harness_with(three_models(), FakePdp::allow(), "/custom-chat");

    let ok = get(&h.router, "/custom-chat/v1/models", &ctx_a()).await;
    assert_eq!(ok.status.as_u16(), 200, "body: {}", ok.raw);
    let ok = get(&h.router, "/custom-chat/v1/models/gpt-standard", &ctx_a()).await;
    assert_eq!(ok.status.as_u16(), 200, "body: {}", ok.raw);

    let miss = get(&h.router, "/mini-chat/v1/models", &ctx_a()).await;
    assert_eq!(
        miss.status.as_u16(),
        404,
        "the default prefix is not mounted"
    );
}

#[tokio::test]
async fn operations_are_registered_with_stable_ids_and_the_base_license_feature() {
    let h = harness(three_models());

    for (key, operation_id) in [
        ("GET:/mini-chat/v1/models", "mini_chat.list_models"),
        ("GET:/mini-chat/v1/models/{id}", "mini_chat.get_model"),
    ] {
        let spec = h
            .openapi
            .operation_specs
            .get(key)
            .unwrap_or_else(|| panic!("operation `{key}` is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id), "{key}");
        let license = spec
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{key} must require a license feature (2.2 License Gate)"));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE_FEATURE.to_owned()],
            "{key}: interim gate is the platform base license feature (ADR-0008)"
        );
    }
}

#[tokio::test]
async fn operations_require_authentication() {
    let h = harness(three_models());
    for key in ["GET:/mini-chat/v1/models", "GET:/mini-chat/v1/models/{id}"] {
        let spec = h.openapi.operation_specs.get(key).expect("registered");
        assert!(
            spec.authenticated,
            "{key} must be registered as an authenticated operation (bearerAuth)"
        );
    }
}

#[tokio::test]
async fn request_without_a_security_context_is_not_served_as_success() {
    // The api-gateway returns 401 before the handler runs; if the handler is
    // reached without an authenticated context it must not return the catalog.
    let h = harness(three_models());
    let resp = send(&h.router, "GET", "/mini-chat/v1/models", None).await;
    assert!(
        !resp.status.is_success(),
        "unauthenticated request must never see the catalog (status {})",
        resp.status
    );
    assert!(!resp.raw.contains("gpt-premium"));
    assert_eq!(h.plugin.total_calls(), 0);
}

#[tokio::test]
async fn tenant_of_the_caller_does_not_change_the_global_catalog() {
    // The catalog is global (DESIGN: "globally enabled"); two tenants see the same list.
    let h = harness(three_models());
    let a = get(&h.router, "/mini-chat/v1/models", &ctx_a())
        .await
        .json();
    let b = get(
        &h.router,
        "/mini-chat/v1/models",
        &common::ctx(common::USER_B, common::TENANT_B),
    )
    .await
    .json();
    assert_eq!(ids(&a), ids(&b));
}
