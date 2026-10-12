#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Models API: `GET /v1/models` and `GET /v1/models/{id}` (DESIGN section 3.3 "Models API").
//!
//! Only globally enabled catalog entries are visible and only the public projection is
//! exposed; disabled and unknown models are `404 not_found`.

mod common;

use common::{
    DISABLED_MODEL, MODEL_RESOURCE_TYPE, MODELS_PATH, PREMIUM_MODEL, STANDARD_MODEL,
    assert_not_found, catalog_entry, env_with_catalog, expect_status, get, items, model_path,
    without_description,
};
use http::StatusCode;
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};

const PUBLIC_MEMBERS: [&str; 7] = [
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "description",
    "multimodal_capabilities",
    "context_window",
];

const REQUIRED_MEMBERS: [&str; 6] = [
    "model_id",
    "display_name",
    "tier",
    "multiplier_display",
    "multimodal_capabilities",
    "context_window",
];

// Internal catalog data that must never reach a client (DESIGN "Non-Exposure Rules").
const FORBIDDEN_MEMBERS: [&str; 18] = [
    "id",
    "provider",
    "provider_id",
    "provider_model_id",
    "provider_display_name",
    "multipliers",
    "input_tokens_credit_multiplier",
    "output_tokens_credit_multiplier",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "credits_micro",
    "policy_version",
    "max_output",
    "max_output_tokens",
    "max_input_tokens",
    "is_default",
    "enabled",
    "estimation_budgets",
];

async fn list_models(env: &TestEnv, user: &TestUser) -> Value {
    let resp = get(env, user, MODELS_PATH).await;
    expect_status(&resp, StatusCode::OK);
    resp.json()
}

fn model_ids(page: &Value) -> Vec<String> {
    let mut ids: Vec<String> = items(page)
        .iter()
        .map(|item| item["model_id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

fn find_model<'a>(page: &'a Value, model_id: &str) -> &'a Value {
    items(page)
        .iter()
        .find(|item| item["model_id"].as_str() == Some(model_id))
        .unwrap_or_else(|| panic!("model `{model_id}` missing from {page}"))
}

// ===========================================================================
// List models
// ===========================================================================

#[tokio::test]
async fn list_models_returns_an_items_envelope() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    let object = page.as_object().unwrap();
    assert_eq!(object.keys().collect::<Vec<_>>(), vec!["items"], "{page}");
    assert!(page["items"].is_array());
}

#[tokio::test]
async fn list_models_returns_only_enabled_models() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    assert_eq!(
        model_ids(&page),
        vec![PREMIUM_MODEL.to_owned(), STANDARD_MODEL.to_owned()],
        "the disabled model must be hidden: {page}"
    );
}

#[tokio::test]
async fn list_models_hides_a_disabled_model() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    assert!(
        items(&page)
            .iter()
            .all(|item| item["model_id"].as_str() != Some(DISABLED_MODEL)),
        "{page}"
    );
}

#[tokio::test]
async fn list_models_items_expose_exactly_the_public_projection() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    assert!(!items(&page).is_empty());
    for item in items(&page) {
        let object = item.as_object().unwrap();
        for member in REQUIRED_MEMBERS {
            assert!(object.contains_key(member), "missing `{member}`: {item}");
        }
        for key in object.keys() {
            assert!(
                PUBLIC_MEMBERS.contains(&key.as_str()),
                "`{key}` is not part of the public projection: {item}"
            );
        }
    }
}

#[tokio::test]
async fn list_models_never_exposes_provider_routing_or_credit_internals() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    for item in items(&page) {
        let object = item.as_object().unwrap();
        for member in FORBIDDEN_MEMBERS {
            assert!(!object.contains_key(member), "`{member}` leaked: {item}");
        }
    }
}

#[tokio::test]
async fn list_models_reports_tier_as_premium_or_standard() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    assert_eq!(find_model(&page, PREMIUM_MODEL)["tier"], json!("premium"));
    assert_eq!(find_model(&page, STANDARD_MODEL)["tier"], json!("standard"));
}

#[tokio::test]
async fn list_models_reports_capabilities_context_window_and_display_fields() {
    let env = TestEnv::new().await;

    let page = list_models(&env, &TestUser::new()).await;

    let model = find_model(&page, PREMIUM_MODEL);
    assert_eq!(model["multimodal_capabilities"], json!(["VISION_INPUT"]));
    assert_eq!(model["context_window"], json!(128_000));
    assert!(
        model["display_name"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    );
    assert!(model["multiplier_display"].is_string());
}

#[tokio::test]
async fn list_models_returns_the_catalog_description_when_configured() {
    let env = env_with_catalog(vec![catalog_entry("m-described", "Standard", true, true)]).await;

    let page = list_models(&env, &TestUser::new()).await;

    assert_eq!(
        find_model(&page, "m-described")["description"],
        json!("Description of m-described")
    );
}

#[tokio::test]
async fn list_models_omits_the_description_when_none_is_configured() {
    let env = env_with_catalog(vec![without_description(catalog_entry(
        "m-plain", "Standard", true, true,
    ))])
    .await;

    let page = list_models(&env, &TestUser::new()).await;

    let description = &find_model(&page, "m-plain")["description"];
    assert!(
        description.is_null(),
        "description is optional and must not be invented: {description}"
    );
}

#[tokio::test]
async fn list_models_is_empty_when_every_catalog_entry_is_disabled() {
    let env = env_with_catalog(vec![catalog_entry("m-off", "Standard", false, true)]).await;

    let page = list_models(&env, &TestUser::new()).await;

    assert!(items(&page).is_empty(), "{page}");
}

#[tokio::test]
async fn list_models_is_identical_for_users_of_different_tenants() {
    let env = TestEnv::new().await;

    let first = list_models(&env, &TestUser::new()).await;
    let second = list_models(&env, &TestUser::new()).await;

    assert_eq!(model_ids(&first), model_ids(&second));
}

// ===========================================================================
// Get model
// ===========================================================================

#[tokio::test]
async fn get_model_returns_the_same_projection_as_the_list_item() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let page = list_models(&env, &user).await;

    let resp = get(&env, &user, &model_path(PREMIUM_MODEL)).await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(&resp.json(), find_model(&page, PREMIUM_MODEL));
}

#[tokio::test]
async fn get_model_exposes_only_the_public_projection() {
    let env = TestEnv::new().await;

    let resp = get(&env, &TestUser::new(), &model_path(STANDARD_MODEL)).await;

    expect_status(&resp, StatusCode::OK);
    let model = resp.json();
    for key in model.as_object().unwrap().keys() {
        assert!(
            PUBLIC_MEMBERS.contains(&key.as_str()),
            "`{key}` leaked: {model}"
        );
    }
    assert_eq!(model["model_id"], json!(STANDARD_MODEL));
    assert_eq!(model["tier"], json!("standard"));
}

#[tokio::test]
async fn get_disabled_model_is_404_naming_the_model_resource() {
    let env = TestEnv::new().await;

    let resp = get(&env, &TestUser::new(), &model_path(DISABLED_MODEL)).await;

    assert_not_found(&resp, MODEL_RESOURCE_TYPE);
}

#[tokio::test]
async fn get_unknown_model_is_404_naming_the_model_resource() {
    let env = TestEnv::new().await;

    let resp = get(&env, &TestUser::new(), &model_path("no-such-model")).await;

    assert_not_found(&resp, MODEL_RESOURCE_TYPE);
}
