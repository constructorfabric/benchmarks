//! In-process HTTP tests of the Models API over the real router with stubbed platform
//! clients (PDP, model-policy gateway); no network is used.
//!
//! Slice S1: flows `list-models`, `get-model` and `request-admission`, algorithms
//! `route-registration` and `model-visibility`, definition of done `models-api`, `lifecycle-routes`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use authz_resolver_sdk::PolicyEnforcer;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn;
use mini_chat_sdk::{ModelTier, PolicySnapshot};
use serde_json::{Value, json};
use toolkit::api::operation_builder::OperationSpec;
use toolkit::api::{OpenApiInfo, OpenApiRegistry, OpenApiRegistryImpl, canonical_error_middleware};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

use super::register_routes;
use crate::domain::authz::PepService;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::services::Services;
use crate::test_support::{
    FakeGateway, Pdp, RecordingPdp, SUBJECT_USER_TYPE, TENANT_A, USER_A1, ctx_with, entry, snapshot,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";
const MODEL_RESOURCE: &str = "gts.cf.core.mini_chat.model.v1~";

/// An `OpenApiRegistry` that records every registered operation.
struct RecordingRegistry {
    inner: OpenApiRegistryImpl,
    ops: Mutex<Vec<OperationSpec>>,
}

impl OpenApiRegistry for RecordingRegistry {
    fn register_operation(&self, spec: &OperationSpec) {
        self.ops.lock().unwrap().push(spec.clone());
        self.inner.register_operation(spec);
    }

    fn ensure_schema_raw(&self, name: &str, schemas: Vec<(String, RefOr<Schema>)>) -> String {
        self.inner.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Harness {
    router: Router,
    pdp: Arc<RecordingPdp>,
    gateway: Arc<FakeGateway>,
    registry: Arc<RecordingRegistry>,
}

fn catalog() -> PolicySnapshot {
    snapshot(vec![
        entry(
            "gpt-premium",
            ModelTier::Premium,
            true,
            &json!({
                "display_name": "GPT Premium",
                "description": "Best for complex reasoning tasks",
                "multiplier_display": "2x",
                "multimodal_capabilities": ["VISION_INPUT", "RAG"],
                "context_window": 128_000,
                "provider_model_id": "azure-deployment-7",
                "max_output_tokens": 4096,
                "max_input_tokens": 120_000,
                "preference": { "is_default": true, "sort_order": 1 }
            }),
        ),
        entry("gpt-hidden", ModelTier::Standard, false, &json!({})),
        entry(
            "gpt-standard",
            ModelTier::Standard,
            true,
            &json!({ "display_name": "GPT Standard", "multiplier_display": "1x", "context_window": 64_000 }),
        ),
    ])
}

fn harness_with(behavior: Pdp, gateway: Arc<FakeGateway>, prefix: &str) -> Harness {
    let pdp = RecordingPdp::new(behavior);
    let pep = Arc::new(PepService::new(PolicyEnforcer::new(pdp.clone())));
    let dyn_gateway: Arc<dyn ModelPolicyGateway> = gateway.clone();
    let services = Arc::new(Services::new(pep, dyn_gateway));
    let registry = Arc::new(RecordingRegistry {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = register_routes(Router::new(), registry.as_ref(), prefix, services)
        .layer(from_fn(canonical_error_middleware));
    Harness {
        router,
        pdp,
        gateway,
        registry,
    }
}

fn harness(behavior: Pdp) -> Harness {
    harness_with(behavior, FakeGateway::serving(catalog()), "/mini-chat")
}

fn caller() -> SecurityContext {
    ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"])
}

fn get(uri: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-request-id", "req-42")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(caller());
    request
}

struct Reply {
    status: StatusCode,
    content_type: String,
    retry_after: Option<String>,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text))
    }
}

async fn call(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    Reply {
        status,
        content_type,
        retry_after,
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

// ── GET /v1/models ───────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_only_enabled_entries_in_an_items_object() {
    let h = harness(Pdp::NoConstraints);
    let reply = call(&h.router, get("/mini-chat/v1/models")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(
        keys(&body),
        BTreeSet::from(["items"]),
        "no pagination fields"
    );
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-premium", "gpt-standard"]);
}

#[tokio::test]
async fn each_item_carries_exactly_the_public_fields() {
    let h = harness(Pdp::NoConstraints);
    let body = call(&h.router, get("/mini-chat/v1/models")).await.json();
    let items = body["items"].as_array().unwrap();

    let with_description = &items[0];
    assert_eq!(
        keys(with_description),
        BTreeSet::from([
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "description",
            "multimodal_capabilities",
            "context_window"
        ])
    );
    assert_eq!(with_description["model_id"], "gpt-premium");
    assert_eq!(with_description["display_name"], "GPT Premium");
    assert_eq!(with_description["multiplier_display"], "2x");
    assert_eq!(
        with_description["description"],
        "Best for complex reasoning tasks"
    );
    assert_eq!(
        with_description["multimodal_capabilities"],
        json!(["VISION_INPUT", "RAG"])
    );
    assert_eq!(with_description["context_window"], 128_000);

    let without_description = &items[1];
    assert_eq!(
        keys(without_description),
        BTreeSet::from([
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "multimodal_capabilities",
            "context_window"
        ]),
        "description is omitted when none is configured"
    );
}

#[tokio::test]
async fn tier_is_the_lowercase_standard_or_premium_string() {
    let h = harness(Pdp::NoConstraints);
    let body = call(&h.router, get("/mini-chat/v1/models")).await.json();
    assert_eq!(body["items"][0]["tier"], "premium");
    assert_eq!(body["items"][1]["tier"], "standard");
}

#[tokio::test]
async fn no_internal_field_or_value_appears_in_the_list_or_get_response() {
    let h = harness(Pdp::NoConstraints);
    let forbidden = [
        "provider",
        "provider_model_id",
        "azure-deployment-7",
        "azure_openai",
        "Azure OpenAI",
        "input_tokens_credit_multiplier",
        "output_tokens_credit_multiplier",
        "credits_micro",
        "policy_version",
        "max_output_tokens",
        "max_input_tokens",
        "is_default",
        "sort_order",
        "general_config",
        "system_prompt",
        "estimation_budgets",
    ];
    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-premium"] {
        let text = call(&h.router, get(uri)).await.text;
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "`{needle}` leaked from {uri}: {text}"
            );
        }
    }
}

#[tokio::test]
async fn an_empty_catalog_yields_an_empty_items_array() {
    let h = harness_with(
        Pdp::NoConstraints,
        FakeGateway::serving(snapshot(vec![])),
        "/mini-chat",
    );
    let reply = call(&h.router, get("/mini-chat/v1/models")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({ "items": [] }));
}

// ── GET /v1/models/{id} ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_the_projection_of_one_enabled_model() {
    let h = harness(Pdp::NoConstraints);
    let reply = call(&h.router, get("/mini-chat/v1/models/gpt-standard")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    assert_eq!(body["model_id"], "gpt-standard");
    assert_eq!(body["tier"], "standard");
    assert_eq!(body["context_window"], 64_000);
    assert!(body.get("items").is_none());
}

#[tokio::test]
async fn get_accepts_an_opaque_non_uuid_id() {
    let gateway = FakeGateway::serving(snapshot(vec![entry(
        "gpt-5.2",
        ModelTier::Premium,
        true,
        &json!({}),
    )]));
    let h = harness_with(Pdp::NoConstraints, gateway, "/mini-chat");
    let reply = call(&h.router, get("/mini-chat/v1/models/gpt-5.2")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json()["model_id"], "gpt-5.2");
}

#[tokio::test]
async fn a_disabled_or_unknown_model_is_a_404_not_found_with_the_model_resource_type() {
    let h = harness(Pdp::NoConstraints);
    let mut bodies = Vec::new();
    for id in ["gpt-hidden", "never-existed"] {
        let reply = call(&h.router, get(&format!("/mini-chat/v1/models/{id}"))).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(reply.content_type, "application/problem+json");
        let body = reply.json();
        assert!(
            body["type"].as_str().unwrap().contains("not_found"),
            "{body}"
        );
        assert_eq!(body["context"]["resource_type"], MODEL_RESOURCE);
        bodies.push(body);
    }
    // indistinguishable: only the echoed identifiers differ
    for body in &mut bodies {
        let object = body.as_object_mut().unwrap();
        object.remove("instance");
        body["context"]
            .as_object_mut()
            .unwrap()
            .remove("resource_name");
    }
    assert_eq!(bodies[0], bodies[1]);
}

// ── Problem shape over HTTP ──────────────────────────────────────────────────

#[tokio::test]
async fn every_error_is_a_full_problem_without_a_top_level_code() {
    let cases = vec![
        (
            Pdp::Deny,
            FakeGateway::serving(catalog()),
            "/mini-chat/v1/models",
            403,
        ),
        (
            Pdp::Unreachable,
            FakeGateway::serving(catalog()),
            "/mini-chat/v1/models",
            503,
        ),
        (
            Pdp::NoConstraints,
            FakeGateway::failing(),
            "/mini-chat/v1/models",
            500,
        ),
        (
            Pdp::NoConstraints,
            FakeGateway::serving(catalog()),
            "/mini-chat/v1/models/gpt-hidden",
            404,
        ),
    ];
    for (behavior, gateway, uri, status) in cases {
        let h = harness_with(behavior, gateway, "/mini-chat");
        let reply = call(&h.router, get(uri)).await;
        assert_eq!(reply.status.as_u16(), status, "{uri} {behavior:?}");
        assert_eq!(reply.content_type, "application/problem+json");
        let body = reply.json();
        for key in [
            "type", "title", "status", "detail", "instance", "trace_id", "context",
        ] {
            assert!(
                body.get(key).is_some(),
                "{status}: missing `{key}` in {body}"
            );
        }
        assert!(
            body.get("code").is_none(),
            "{status}: top-level code in {body}"
        );
        assert_eq!(body["status"], status);
        assert_eq!(body["instance"], uri);
        assert_eq!(body["trace_id"], "req-42");
    }
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_pdp_deny_is_a_403_authz_denied_and_the_catalog_is_not_read() {
    let h = harness(Pdp::Deny);
    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-premium"] {
        let reply = call(&h.router, get(uri)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(reply.json()["context"]["reason"], "AUTHZ_DENIED");
    }
    assert_eq!(h.gateway.call_count(), 0);
}

#[tokio::test]
async fn a_pdp_failure_is_a_503_with_retry_after_5_and_the_catalog_is_not_read() {
    let h = harness(Pdp::Unreachable);
    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-premium"] {
        let reply = call(&h.router, get(uri)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(reply.retry_after.as_deref(), Some("5"));
        let body = reply.json();
        assert_eq!(body["context"]["retry_after_seconds"], 5);
        assert!(
            !reply.text.contains("pdp unreachable"),
            "cause leaked: {}",
            reply.text
        );
    }
    assert_eq!(h.gateway.call_count(), 0);
}

#[tokio::test]
async fn the_models_api_is_decision_only_so_a_decision_without_constraints_is_admitted() {
    let h = harness(Pdp::NoConstraints);
    assert_eq!(
        call(&h.router, get("/mini-chat/v1/models")).await.status,
        StatusCode::OK
    );
    let request = h.pdp.requests().pop().unwrap();
    assert!(!request.context.require_constraints);
    assert_eq!(request.action.name, "list");
}

#[tokio::test]
async fn a_policy_plugin_failure_is_a_generic_500() {
    let h = harness_with(Pdp::NoConstraints, FakeGateway::failing(), "/mini-chat");
    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/gpt-premium"] {
        let reply = call(&h.router, get(uri)).await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
        assert!(reply.json()["type"].as_str().unwrap().contains("internal"));
        assert!(
            !reply.text.contains("provider-secret-detail"),
            "cause leaked: {}",
            reply.text
        );
    }
}

// ── route registration ───────────────────────────────────────────────────────

#[tokio::test]
async fn routes_live_under_the_configured_url_prefix_only() {
    let h = harness_with(
        Pdp::NoConstraints,
        FakeGateway::serving(catalog()),
        "/chat-api",
    );
    assert_eq!(
        call(&h.router, get("/chat-api/v1/models")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        call(&h.router, get("/mini-chat/v1/models")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn this_feature_registers_exactly_the_two_models_operations() {
    let h = harness(Pdp::NoConstraints);
    let ops = h.registry.ops.lock().unwrap();
    // the chat endpoints of the chat-management feature are checked in `chat_routes_tests`
    let described: BTreeSet<(String, String, String)> = ops
        .iter()
        .filter(|op| op.path.contains("/v1/models"))
        .map(|op| {
            (
                op.method.to_string(),
                op.path.clone(),
                op.operation_id.clone().unwrap(),
            )
        })
        .collect();
    let expected: BTreeSet<(String, String, String)> = [
        ("GET", "/mini-chat/v1/models", "mini_chat.list_models"),
        ("GET", "/mini-chat/v1/models/{id}", "mini_chat.get_model"),
    ]
    .into_iter()
    .map(|(m, p, id)| (m.to_owned(), p.to_owned(), id.to_owned()))
    .collect();
    assert_eq!(described, expected);
}

#[test]
fn the_upload_get_and_delete_attachment_operations_match_the_extracted_contract() {
    // slices S1-upload-lifecycle and S2-deletion-cleanup-tools of the attachments feature: the
    // three operations, the multipart request body, their success and error statuses and the
    // `Retry-After` of the 503
    let h = harness(Pdp::NoConstraints);
    let ops = h.registry.ops.lock().unwrap();
    let attachment_ops: Vec<_> = ops
        .iter()
        .filter(|op| op.path.contains("/attachments"))
        .collect();
    let described: BTreeSet<(String, String, String)> = attachment_ops
        .iter()
        .map(|op| {
            (
                op.method.to_string(),
                op.path.clone(),
                op.operation_id.clone().unwrap(),
            )
        })
        .collect();
    let expected: BTreeSet<(String, String, String)> = [
        (
            "POST",
            "/mini-chat/v1/chats/{id}/attachments",
            "mini_chat.upload_attachment",
        ),
        (
            "GET",
            "/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "mini_chat.get_attachment",
        ),
        (
            "DELETE",
            "/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "mini_chat.delete_attachment",
        ),
    ]
    .into_iter()
    .map(|(m, p, id)| (m.to_owned(), p.to_owned(), id.to_owned()))
    .collect();
    assert_eq!(described, expected);

    for op in &attachment_ops {
        assert_eq!(op.tags, vec!["Mini Chat Attachments".to_owned()]);
        let retry = op.responses.iter().find(|r| r.status == 503).unwrap();
        assert!(
            retry
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("retry-after")),
            "{} must declare Retry-After on 503",
            op.path
        );
    }
    let upload = attachment_ops
        .iter()
        .find(|op| op.method.as_str() == "POST")
        .unwrap();
    let statuses: BTreeSet<u16> = upload.responses.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        BTreeSet::from([201, 400, 401, 403, 404, 409, 429, 500, 503])
    );
    let body = upload.request_body.as_ref().expect("a multipart body");
    assert_eq!(body.content_type, "multipart/form-data");
    let get = attachment_ops
        .iter()
        .find(|op| op.method.as_str() == "GET")
        .unwrap();
    let statuses: BTreeSet<u16> = get.responses.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        BTreeSet::from([200, 400, 401, 403, 404, 500, 503])
    );
    assert!(get.request_body.is_none());
    let delete = attachment_ops
        .iter()
        .find(|op| op.method.as_str() == "DELETE")
        .unwrap();
    let statuses: BTreeSet<u16> = delete.responses.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        BTreeSet::from([204, 400, 401, 403, 404, 409, 500, 503])
    );
    assert!(delete.request_body.is_none());
}

#[test]
fn deferred_surfaces_are_not_registered() {
    let h = harness(Pdp::NoConstraints);
    for op in h.registry.ops.lock().unwrap().iter() {
        for banned in ["mcp", ":temporary", "mcp-connections", "/admin/"] {
            assert!(
                !op.path.contains(banned),
                "{} registers a deferred surface",
                op.path
            );
        }
    }
}

#[test]
fn every_route_is_authenticated_and_declares_only_the_base_license_feature() {
    let h = harness(Pdp::NoConstraints);
    let ops = h.registry.ops.lock().unwrap();
    assert!(!ops.is_empty());
    for op in ops.iter() {
        assert!(op.authenticated, "{}", op.path);
        let license = op
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no license feature", op.path));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE.to_owned()],
            "{}",
            op.path
        );
        assert!(
            !license
                .license_names
                .iter()
                .any(|name| name.contains("ai_chat"))
        );
    }
}

#[test]
fn error_responses_are_declared_except_the_415() {
    let h = harness(Pdp::NoConstraints);
    let ops = h.registry.ops.lock().unwrap();
    for op in ops.iter().filter(|op| op.path.contains("/v1/models")) {
        let statuses: BTreeSet<u16> = op.responses.iter().map(|r| r.status).collect();
        let mut expected = BTreeSet::from([200, 401, 403, 500, 503]);
        if op.path.ends_with("{id}") {
            expected.insert(404);
        }
        assert_eq!(statuses, expected, "{}", op.path);
        assert!(
            !statuses.contains(&415) && !statuses.contains(&400),
            "{}",
            op.path
        );
        let unavailable = op.responses.iter().find(|r| r.status == 503).unwrap();
        assert!(
            unavailable
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("retry-after")),
            "{} must declare Retry-After on 503",
            op.path
        );
        for response in op.responses.iter().filter(|r| r.status >= 400) {
            assert_eq!(
                response.content_type, "application/problem+json",
                "{} {}",
                op.path, response.status
            );
        }
    }
}

#[test]
fn the_get_operation_declares_its_path_parameter_and_the_models_tag() {
    let h = harness(Pdp::NoConstraints);
    let ops = h.registry.ops.lock().unwrap();
    let models: Vec<_> = ops
        .iter()
        .filter(|op| op.path.contains("/v1/models"))
        .collect();
    let get_model = models.iter().find(|op| op.path.ends_with("{id}")).unwrap();
    assert!(
        get_model
            .params
            .iter()
            .any(|p| p.name == "id" && p.required)
    );
    for op in models {
        assert_eq!(op.tags, vec!["Mini Chat Models".to_owned()], "{}", op.path);
    }
}

#[test]
fn the_openapi_document_declares_the_models_schemas_of_the_reference_subset() {
    let h = harness(Pdp::NoConstraints);
    let document = h
        .registry
        .inner
        .build_openapi(&OpenApiInfo::default())
        .expect("openapi builds");
    let value = serde_json::to_value(&document).unwrap();
    let schemas = &value["components"]["schemas"];

    let model = &schemas["ModelDto"];
    let required: BTreeSet<&str> = model["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        required,
        BTreeSet::from([
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "multimodal_capabilities",
            "context_window"
        ])
    );
    assert!(model["properties"].get("description").is_some());

    let list = &schemas["ModelListDto"];
    assert_eq!(list["required"], json!(["items"]));
    assert!(list["properties"].get("page_info").is_none());

    let paths = value["paths"].as_object().unwrap();
    assert!(paths.contains_key("/mini-chat/v1/models"));
    assert!(paths.contains_key("/mini-chat/v1/models/{id}"));
}
