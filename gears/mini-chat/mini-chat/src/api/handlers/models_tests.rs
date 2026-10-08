//! Models API tests (DESIGN §3.3 "Models API": visibility and non-exposure rules).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use http::StatusCode;

use crate::testing::{NO_VISION, PREMIUM, STANDARD, TINY, TestApp, ctx_a1};

const MODELS: &str = "/mini-chat/v1/models";

fn keys(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

#[tokio::test]
async fn list_returns_only_enabled_models_without_internal_fields() {
    let t = TestApp::new().await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", MODELS, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![PREMIUM, STANDARD, NO_VISION, TINY]);
    let expected: BTreeSet<String> = [
        "model_id",
        "display_name",
        "tier",
        "multiplier_display",
        "description",
        "multimodal_capabilities",
        "context_window",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for m in items {
        assert_eq!(keys(m), expected, "{m}");
    }
    let premium = &items[0];
    assert_eq!(premium["display_name"], PREMIUM.to_uppercase());
    assert_eq!(premium["tier"], "premium");
    assert_eq!(premium["multiplier_display"], "3x");
    assert_eq!(premium["multimodal_capabilities"], serde_json::json!(["VISION_INPUT"]));
    assert_eq!(premium["context_window"], 128_000);
    assert_eq!(items[1]["tier"], "standard");
    let text = body.to_string();
    for secret in ["provider", "-provider", "credit_multiplier", "is_default", "policy_version", "max_output", "system_prompt"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "model:list"));
}

#[tokio::test]
async fn get_model_applies_the_visibility_rule() {
    let t = TestApp::new().await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{MODELS}/{STANDARD}"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["model_id"], STANDARD);
    assert_eq!(body["description"], format!("{STANDARD} model"));
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "model:read"));
    for id in ["old-model", "unknown-model"] {
        let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{MODELS}/{id}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["context"]["resource_type"], "gts.cf.core.mini_chat.model.v1~");
    }
    // Disabling a model hides it from both endpoints.
    t.policy.with_snapshot(|s| s.model_catalog[1].enabled = false);
    let (st, _, _) = t.call(&ctx_a1(), "GET", &format!("{MODELS}/{STANDARD}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (_, _, body) = t.call(&ctx_a1(), "GET", MODELS, None).await;
    assert!(!body.to_string().contains(&format!("\"{STANDARD}\"")));
}

#[tokio::test]
async fn empty_description_is_omitted() {
    let t = TestApp::new().await;
    t.policy.with_snapshot(|s| s.model_catalog[0].description.clear());
    let (_, _, body) = t.call(&ctx_a1(), "GET", &format!("{MODELS}/{PREMIUM}"), None).await;
    assert!(body.get("description").is_none(), "{body}");
}

#[tokio::test]
async fn models_authorization_errors() {
    let t = TestApp::new().await;
    t.authz.deny.store(true, Ordering::SeqCst);
    for uri in [MODELS.to_owned(), format!("{MODELS}/{PREMIUM}")] {
        let (st, _, body) = t.call(&ctx_a1(), "GET", &uri, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    }
    t.authz.deny.store(false, Ordering::SeqCst);
    t.authz.unavailable.store(true, Ordering::SeqCst);
    let (st, headers, _) = t.call(&ctx_a1(), "GET", MODELS, None).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get(http::header::RETRY_AFTER).unwrap(), "5");
}
