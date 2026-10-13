#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Models API (`GET /mini-chat/v1/models`, `GET /mini-chat/v1/models/{id}`).

mod common;

use axum::http::StatusCode;
use common::{PdpMode, TestApp, disabled, premium_model, standard_model, standard_no_vision};
use serde_json::Value;
use toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE;
use uuid::Uuid;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn model_ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|m| m["model_id"].as_str().expect("model_id").to_owned())
        .collect()
}

#[tokio::test]
async fn list_returns_only_enabled_in_catalog_order() {
    let app = TestApp::builder()
        .catalog(vec![
            standard_model("s1"),
            disabled(premium_model("p-off")),
            premium_model("p1"),
            standard_no_vision("s-novision"),
        ])
        .build()
        .await;
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).get("/mini-chat/v1/models").await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(model_ids(&resp.json()), ["s1", "p1", "s-novision"]);
}

#[tokio::test]
async fn model_dto_has_no_internal_fields() {
    let app = TestApp::builder()
        .catalog(vec![premium_model("p1"), standard_model("s1")])
        .build()
        .await;
    let (user, tenant) = ids();

    let body = app
        .as_user(user, tenant)
        .get("/mini-chat/v1/models")
        .await
        .json();
    let items = body["items"].as_array().expect("items");
    for item in items {
        let obj = item.as_object().expect("object");
        for key in [
            "provider_id",
            "provider_model_id",
            "input_tokens_credit_multiplier_micro",
            "output_tokens_credit_multiplier_micro",
            "is_default",
            "preference",
            "policy_version",
        ] {
            assert!(!obj.contains_key(key), "{key} exposed in {item}");
        }
        for key in [
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "multimodal_capabilities",
            "context_window",
        ] {
            assert!(obj.contains_key(key), "{key} missing in {item}");
        }
    }
    let p1 = &items[0];
    assert_eq!(p1["tier"], "premium");
    assert_eq!(p1["description"], "Premium model p1");
    assert_eq!(p1["multiplier_display"], "2x");
    assert_eq!(p1["context_window"], 128_000);
    assert_eq!(
        p1["multimodal_capabilities"],
        serde_json::json!(["VISION_INPUT", "RAG"])
    );
    let s1 = items[1].as_object().unwrap();
    assert_eq!(s1["tier"], "standard");
    assert!(
        !s1.contains_key("description"),
        "empty description must be absent"
    );
}

#[tokio::test]
async fn get_returns_single_enabled_model() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    let resp = app
        .as_user(user, tenant)
        .get("/mini-chat/v1/models/p1")
        .await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["model_id"], "p1");
    assert_eq!(body["display_name"], "Model p1");
    assert!(body.get("provider_model_id").is_none());
}

#[tokio::test]
async fn get_disabled_or_unknown_is_404_model() {
    let app = TestApp::builder()
        .catalog(vec![standard_model("s1"), disabled(premium_model("p-off"))])
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    for path in ["/mini-chat/v1/models/p-off", "/mini-chat/v1/models/unknown"] {
        let resp = client.get(path).await;
        assert_eq!(
            resp.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            resp.text()
        );
        let body = resp.json();
        assert!(
            body["type"]
                .as_str()
                .unwrap()
                .contains("cf.core.err.not_found.v1~"),
            "{body}"
        );
        assert_eq!(
            body["context"]["resource_type"],
            "gts.cf.core.mini_chat.model.v1~"
        );
    }
}

#[tokio::test]
async fn pdp_deny_is_403() {
    let app = TestApp::builder().build().await;
    app.pdp.set_mode(PdpMode::Deny);
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    for path in ["/mini-chat/v1/models", "/mini-chat/v1/models/p1"] {
        let resp = client.get(path).await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "{path}: {}",
            resp.text()
        );
        let body = resp.json();
        assert!(
            body["type"]
                .as_str()
                .unwrap()
                .contains("cf.core.err.permission_denied.v1~")
        );
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    }
}

#[tokio::test]
async fn pdp_failure_is_503_retry_after() {
    let app = TestApp::builder().build().await;
    app.pdp.set_mode(PdpMode::Fail);
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).get("/mini-chat/v1/models").await;

    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );
    assert_eq!(resp.header("retry-after").as_deref(), Some("5"));
    let body = resp.json();
    assert_eq!(body["context"]["retry_after_seconds"], 5);
    assert!(
        !body["detail"].as_str().unwrap().contains("fake pdp"),
        "cause must not leak: {body}"
    );
}

#[tokio::test]
async fn models_pdp_request_uses_model_resource_without_constraints() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    app.as_user(user, tenant).get("/mini-chat/v1/models").await;
    let list = app.pdp.last_request();
    assert_eq!(
        list.resource.resource_type,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert_eq!(list.action.name, "list");
    assert!(!list.context.require_constraints);

    app.as_user(user, tenant)
        .get("/mini-chat/v1/models/p1")
        .await;
    let read = app.pdp.last_request();
    assert_eq!(read.action.name, "read");
    assert!(read.resource.id.is_none());
    assert!(!read.context.require_constraints);
}

#[tokio::test]
async fn model_routes_are_authenticated_and_license_gated() {
    let app = TestApp::builder().build().await;
    let ops = app.operations();
    let mut seen: Vec<(String, String, String)> = Vec::new();
    for op in &ops {
        assert!(op.authenticated, "{} not authenticated", op.path);
        let lic = op
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no license requirement", op.path));
        assert_eq!(lic.license_names, [CORE_GLOBAL_BASE_LICENSE_FEATURE]);
        seen.push((
            op.method.to_string(),
            op.path.clone(),
            op.operation_id.clone().unwrap_or_default(),
        ));
    }
    for expected in [
        ("GET", "/mini-chat/v1/models", "mini_chat.list_models"),
        ("GET", "/mini-chat/v1/models/{id}", "mini_chat.get_model"),
    ] {
        assert!(
            seen.iter()
                .any(|(m, p, o)| (m.as_str(), p.as_str(), o.as_str()) == expected),
            "{expected:?} not registered: {seen:?}"
        );
    }
}

#[tokio::test]
async fn routes_follow_configured_url_prefix() {
    let app = TestApp::builder()
        .config(|c| c.url_prefix = "/chat-api".to_owned())
        .build()
        .await;
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).get("/chat-api/v1/models").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let resp = app.as_user(user, tenant).get("/mini-chat/v1/models").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}
