#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the Models API through the REST layer
//! (`cpt-cf-mini-chat-flow-gear-foundation-list-models`, `-get-model`, the
//! request-gate part that is visible here, `cpt-cf-mini-chat-dod-gear-foundation-models-api`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, PolicyPluginError};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt;

use super::{BASE_LICENSE_FEATURE, register_routes};
use crate::test_doubles::{
    Behavior, FakePdp, FakePolicyPlugin, PdpReply, PluginWorld, entry, entry_json, models_service,
    user_ctx,
};

const PREFIX: &str = "/mini-chat";
const NOT_FOUND_TYPE: &str = "gts.cf.core.mini_chat.model.v1~";

struct App {
    router: Router,
    openapi: OpenApiRegistryImpl,
    plugin: Arc<FakePolicyPlugin>,
}

fn catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
    let enabled = entry_json("model-a", true);
    let disabled = entry_json("model-b", false);
    let mut bare = entry_json("model-c", true);
    bare["description"] = json!("");
    bare["tier"] = json!("standard");
    let spaced = entry_json("Team A", true);
    let dotted = entry_json("gpt-5.2", true);
    [enabled, disabled, bare, spaced, dotted]
        .into_iter()
        .map(entry)
        .collect()
}

fn app_with(reply: PdpReply, prefix: &str) -> App {
    let plugin = FakePolicyPlugin::with_catalog(catalog());
    let world = PluginWorld::new();
    world.register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
    let pdp = FakePdp::new(reply);
    let service = models_service(
        &pdp,
        world
            .policy_gateway()
            .with_call_timeout(Duration::from_millis(50)),
    );
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, service, prefix);
    App {
        router,
        openapi,
        plugin,
    }
}

fn app() -> App {
    app_with(PdpReply::Allow, PREFIX)
}

struct Reply {
    status: StatusCode,
    retry_after: Option<String>,
    text: String,
    json: Value,
}

async fn get(router: &Router, uri: &str) -> Reply {
    let mut request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(user_ctx());
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        retry_after,
        text,
        json,
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

fn item_ids(reply: &Reply) -> Vec<&str> {
    reply.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["model_id"].as_str().unwrap())
        .collect()
}

const FULL_KEYS: [&str; 7] = [
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "description",
    "multimodal_capabilities",
    "context_window",
];

// ---- list ---------------------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_200_with_the_enabled_models_in_catalog_order() {
    let app = app();

    let reply = get(&app.router, "/mini-chat/v1/models").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json), BTreeSet::from(["items"]));
    assert_eq!(
        item_ids(&reply),
        vec!["model-a", "model-c", "Team A", "gpt-5.2"]
    );
}

#[tokio::test]
async fn every_item_has_exactly_the_documented_keys_and_description_is_omitted_when_empty() {
    let app = app();

    let reply = get(&app.router, "/mini-chat/v1/models").await;

    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(keys(&items[0]), BTreeSet::from(FULL_KEYS));
    assert_eq!(
        items[0],
        json!({
            "model_id": "model-a",
            "display_name": "Model model-a",
            "tier": "premium",
            "multiplier_display": "2x",
            "description": "About model-a",
            "multimodal_capabilities": ["VISION_INPUT", "RAG"],
            "context_window": 128_000,
        })
    );
    let without_description: BTreeSet<&str> = FULL_KEYS
        .into_iter()
        .filter(|k| *k != "description")
        .collect();
    assert_eq!(keys(&items[1]), without_description);
    assert_eq!(items[1]["tier"], "standard");
}

#[tokio::test]
async fn the_response_exposes_no_provider_routing_credit_or_prompt_detail() {
    let app = app();

    let list = get(&app.router, "/mini-chat/v1/models").await;
    let single = get(&app.router, "/mini-chat/v1/models/model-a").await;

    assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    assert_eq!(single.status, StatusCode::OK, "{}", single.text);
    assert!(list.text.contains("model-a") && single.text.contains("model-a"));
    for text in [&list.text, &single.text] {
        for forbidden in [
            "provider",
            "credit",
            "policy_version",
            "max_output",
            "max_input",
            "is_default",
            "preference",
            "estimation",
            "general_config",
            "prompt",
            "SECRET",
            "icon",
            "openai",
        ] {
            assert!(!text.contains(forbidden), "`{forbidden}` leaked: {text}");
        }
    }
}

#[tokio::test]
async fn list_of_an_empty_catalog_is_200_with_no_items() {
    let app = app();
    app.plugin.catalog.lock().unwrap().clear();

    let reply = get(&app.router, "/mini-chat/v1/models").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json, json!({ "items": [] }));
}

// ---- get ----------------------------------------------------------------------------------------

#[tokio::test]
async fn get_of_an_enabled_model_returns_its_projection() {
    let app = app();

    let reply = get(&app.router, "/mini-chat/v1/models/model-a").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json), BTreeSet::from(FULL_KEYS));
    assert_eq!(reply.json["model_id"], "model-a");
    assert_eq!(reply.json["context_window"], 128_000);
}

#[tokio::test]
async fn the_model_id_is_an_opaque_path_string() {
    let app = app();

    let spaced = get(&app.router, "/mini-chat/v1/models/Team%20A").await;
    let dotted = get(&app.router, "/mini-chat/v1/models/gpt-5.2").await;

    assert_eq!(spaced.status, StatusCode::OK, "{}", spaced.text);
    assert_eq!(spaced.json["model_id"], "Team A");
    assert_eq!(dotted.status, StatusCode::OK, "{}", dotted.text);
    assert_eq!(dotted.json["model_id"], "gpt-5.2");
}

#[tokio::test]
async fn get_of_an_unknown_model_is_404_not_found_with_the_model_resource_type() {
    let app = app();

    let reply = get(&app.router, "/mini-chat/v1/models/no-such-model").await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text);
    assert_eq!(reply.json["context"]["resource_type"], NOT_FOUND_TYPE);
    assert!(
        reply.json["type"].as_str().unwrap().contains("not_found"),
        "{}",
        reply.text
    );
    assert!(
        reply.json.get("code").is_none(),
        "no top-level code: {}",
        reply.text
    );
}

#[tokio::test]
async fn an_id_that_is_not_a_uuid_is_not_a_validation_error() {
    let app = app();

    let reply = get(&app.router, "/mini-chat/v1/models/not-a-uuid").await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text);
    assert_eq!(
        reply.json["context"]["resource_type"], NOT_FOUND_TYPE,
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn a_disabled_model_is_404_and_indistinguishable_from_an_unknown_one() {
    let app = app();

    let disabled = get(&app.router, "/mini-chat/v1/models/model-b").await;
    let unknown = get(&app.router, "/mini-chat/v1/models/model-zzz").await;

    assert_eq!(disabled.status, StatusCode::NOT_FOUND);
    assert_eq!(disabled.json["context"]["resource_type"], NOT_FOUND_TYPE);
    assert_eq!(unknown.json["context"]["resource_type"], NOT_FOUND_TYPE);
    assert_eq!(disabled.json["context"], unknown.json["context"]);
    assert_eq!(disabled.json["detail"], unknown.json["detail"]);
    assert_eq!(disabled.json["title"], unknown.json["title"]);
    assert_eq!(disabled.json["type"], unknown.json["type"]);
    assert!(!disabled.text.contains("disabled"), "{}", disabled.text);
}

// ---- PEP ------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pdp_deny_is_403_authz_denied_on_both_routes_without_deny_details() {
    let app = app_with(PdpReply::Deny, PREFIX);

    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/model-a"] {
        let reply = get(&app.router, uri).await;

        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}: {}", reply.text);
        assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED", "{uri}");
        assert!(!reply.text.contains("deny_reason"), "{}", reply.text);
    }
    assert!(app.plugin.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_five_on_both_routes() {
    let app = app_with(PdpReply::Failure, PREFIX);

    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/model-a"] {
        let reply = get(&app.router, uri).await;

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{uri}: {}",
            reply.text
        );
        assert_eq!(reply.retry_after.as_deref(), Some("5"), "{uri}");
        assert_eq!(reply.json["context"]["retry_after_seconds"], 5, "{uri}");
    }
}

// ---- plugin failures --------------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_policy_plugin_is_500_internal_without_the_cause() {
    let app = app();
    app.plugin
        .set_snapshot(Behavior::Fail(PolicyPluginError::Internal(
            "database password=hunter2".to_owned(),
        )));

    for uri in ["/mini-chat/v1/models", "/mini-chat/v1/models/model-a"] {
        let reply = get(&app.router, uri).await;

        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{uri}: {}",
            reply.text
        );
        assert!(
            reply.json["type"].as_str().unwrap().contains("internal"),
            "{}",
            reply.text
        );
        assert!(!reply.text.contains("hunter2"), "{}", reply.text);
    }
}

#[tokio::test]
async fn a_policy_plugin_that_never_answers_is_500_internal() {
    let app = app();
    app.plugin.set_version(Behavior::Hang);

    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        get(&app.router, "/mini-chat/v1/models"),
    )
    .await
    .expect("the plugin call is bounded");

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn an_unregistered_policy_plugin_is_500_internal() {
    let world = PluginWorld::new();
    let pdp = FakePdp::new(PdpReply::Allow);
    let service = models_service(&pdp, world.policy_gateway());
    let router = register_routes(Router::new(), &OpenApiRegistryImpl::new(), service, PREFIX);

    let reply = get(&router, "/mini-chat/v1/models").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
}

// ---- url_prefix --------------------------------------------------------------------------------------

#[tokio::test]
async fn the_routes_live_under_the_configured_url_prefix() {
    let app = app_with(PdpReply::Allow, "/chat-api");

    let served = get(&app.router, "/chat-api/v1/models").await;
    let single = get(&app.router, "/chat-api/v1/models/model-a").await;
    let elsewhere = get(&app.router, "/mini-chat/v1/models").await;

    assert_eq!(served.status, StatusCode::OK, "{}", served.text);
    assert_eq!(single.status, StatusCode::OK, "{}", single.text);
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
}

// ---- registration metadata ------------------------------------------------------------------------------

fn spec_of(app: &App, key: &str) -> toolkit::api::operation_builder::OperationSpec {
    app.openapi
        .operation_specs
        .get(key)
        .unwrap_or_else(|| {
            let known: Vec<String> = app
                .openapi
                .operation_specs
                .iter()
                .map(|e| e.key().clone())
                .collect();
            panic!("{key} is not registered; registered: {known:?}")
        })
        .value()
        .clone()
}

#[test]
fn exactly_the_two_models_operations_are_registered() {
    let app = app();

    let mut registered: Vec<String> = app
        .openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();
    registered.sort();

    assert_eq!(
        registered,
        vec![
            "GET:/mini-chat/v1/models".to_owned(),
            "GET:/mini-chat/v1/models/{id}".to_owned(),
        ]
    );
}

#[test]
fn every_route_requires_authentication_and_the_base_license_feature() {
    let app = app();

    for key in ["GET:/mini-chat/v1/models", "GET:/mini-chat/v1/models/{id}"] {
        let spec = spec_of(&app, key);

        assert!(spec.authenticated, "{key}");
        let license = spec.license_requirement.expect("license feature declared");
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE_FEATURE.to_owned()],
            "{key}"
        );
    }
}

#[test]
fn the_routes_are_documented_with_their_responses() {
    let app = app();

    let list = spec_of(&app, "GET:/mini-chat/v1/models");
    let get_one = spec_of(&app, "GET:/mini-chat/v1/models/{id}");

    let statuses = |spec: &toolkit::api::operation_builder::OperationSpec| -> BTreeSet<u16> {
        spec.responses.iter().map(|r| r.status).collect()
    };
    for status in [200, 401, 403, 500, 503] {
        assert!(
            statuses(&list).contains(&status),
            "list lacks {status}: {:?}",
            statuses(&list)
        );
    }
    for status in [200, 401, 403, 404, 500, 503] {
        assert!(
            statuses(&get_one).contains(&status),
            "get lacks {status}: {:?}",
            statuses(&get_one)
        );
    }
    for spec in [&list, &get_one] {
        assert!(
            spec.operation_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(spec.summary.as_deref().is_some_and(|s| !s.is_empty()));
        assert!(!spec.tags.is_empty());
    }
    assert!(
        get_one.params.iter().any(|p| p.name == "id"),
        "the path parameter is documented"
    );
}
