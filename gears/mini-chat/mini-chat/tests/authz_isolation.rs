#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Tenant and owner isolation of chats, and fail-closed authorization (DESIGN section 3.8).
//!
//! The harness PDP is switched with `AuthzMode`: `OwnerScoped` (tenant + owner constraints),
//! `TenantOnly` (tenant constraint only: the gear must add the owner predicate itself),
//! `Deny`, `Unavailable` and `NoConstraints`.

mod common;

use common::{
    CHAT_RESOURCE_TYPE, CHATS_PATH, MODELS_PATH, PREMIUM_MODEL, assert_not_found, assert_problem,
    chat_id, chat_path, chat_row, chat_row_count, create_default_chat, create_titled_chat,
    create_titled_chats, delete_chat, env_with_authz, expect_status, get_chat, item_ids,
    item_titles, items, list_chats_ok, model_path, patch_chat, uuid_hex,
};
use http::{Method, StatusCode};
use mini_chat::test_support::{AuthzMode, TestEnv, TestResponse, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Intruder {
    SameTenant,
    OtherTenant,
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    Rename,
    Delete,
}

fn intruder_user(owner: &TestUser, intruder: Intruder) -> TestUser {
    match intruder {
        Intruder::SameTenant => TestUser::in_tenant(owner.tenant_id),
        Intruder::OtherTenant => TestUser::new(),
    }
}

/// The owner creates a chat; the intruder then tries `access` on it under `mode`.
async fn foreign_access(
    mode: AuthzMode,
    intruder: Intruder,
    access: Access,
) -> (TestEnv, TestUser, String, TestResponse) {
    let env = env_with_authz(mode).await;
    let owner = TestUser::new();
    let id = chat_id(&create_titled_chat(&env, &owner, "Private").await);
    let other = intruder_user(&owner, intruder);
    let resp = match access {
        Access::Read => get_chat(&env, &other, &id).await,
        Access::Rename => patch_chat(&env, &other, &id, json!({ "title": "Hijacked" })).await,
        Access::Delete => delete_chat(&env, &other, &id).await,
    };
    (env, owner, id, resp)
}

async fn assert_foreign_access_is_hidden(mode: AuthzMode, intruder: Intruder, access: Access) {
    let (env, owner, id, resp) = foreign_access(mode, intruder, access).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    let row = chat_row(&env, &id).await.unwrap();
    assert_eq!(
        row.title.as_deref(),
        Some("Private"),
        "foreign access changed the chat"
    );
    assert!(!row.deleted, "foreign access deleted the chat");
    expect_status(&get_chat(&env, &owner, &id).await, StatusCode::OK);
}

// ===========================================================================
// Foreign chats are hidden (404, never 403) - owner-scoped PDP
// ===========================================================================

#[tokio::test]
async fn owner_scoped_same_tenant_user_gets_404_reading_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::OwnerScoped, Intruder::SameTenant, Access::Read)
        .await;
}

#[tokio::test]
async fn owner_scoped_same_tenant_user_gets_404_renaming_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::OwnerScoped, Intruder::SameTenant, Access::Rename)
        .await;
}

#[tokio::test]
async fn owner_scoped_same_tenant_user_gets_404_deleting_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::OwnerScoped, Intruder::SameTenant, Access::Delete)
        .await;
}

#[tokio::test]
async fn owner_scoped_other_tenant_user_gets_404_reading_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::OwnerScoped, Intruder::OtherTenant, Access::Read)
        .await;
}

#[tokio::test]
async fn owner_scoped_other_tenant_user_gets_404_renaming_a_foreign_chat() {
    assert_foreign_access_is_hidden(
        AuthzMode::OwnerScoped,
        Intruder::OtherTenant,
        Access::Rename,
    )
    .await;
}

#[tokio::test]
async fn owner_scoped_other_tenant_user_gets_404_deleting_a_foreign_chat() {
    assert_foreign_access_is_hidden(
        AuthzMode::OwnerScoped,
        Intruder::OtherTenant,
        Access::Delete,
    )
    .await;
}

// ===========================================================================
// Tenant-only PDP: the gear must add the owner predicate itself (DESIGN 3.8 notes)
// ===========================================================================

#[tokio::test]
async fn tenant_only_pdp_same_tenant_user_gets_404_reading_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::TenantOnly, Intruder::SameTenant, Access::Read)
        .await;
}

#[tokio::test]
async fn tenant_only_pdp_same_tenant_user_gets_404_renaming_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::TenantOnly, Intruder::SameTenant, Access::Rename)
        .await;
}

#[tokio::test]
async fn tenant_only_pdp_same_tenant_user_gets_404_deleting_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::TenantOnly, Intruder::SameTenant, Access::Delete)
        .await;
}

#[tokio::test]
async fn tenant_only_pdp_other_tenant_user_gets_404_reading_a_foreign_chat() {
    assert_foreign_access_is_hidden(AuthzMode::TenantOnly, Intruder::OtherTenant, Access::Read)
        .await;
}

#[tokio::test]
async fn tenant_only_pdp_list_hides_other_users_chats_in_the_same_tenant() {
    let env = env_with_authz(AuthzMode::TenantOnly).await;
    let owner = TestUser::new();
    let colleague = TestUser::in_tenant(owner.tenant_id);
    create_titled_chats(&env, &owner, &["owner-1", "owner-2"]).await;
    create_titled_chats(&env, &colleague, &["colleague-1"]).await;

    let owner_page = list_chats_ok(&env, &owner, &[]).await;
    let colleague_page = list_chats_ok(&env, &colleague, &[]).await;

    assert_eq!(item_titles(&owner_page), vec!["owner-2", "owner-1"]);
    assert_eq!(item_titles(&colleague_page), vec!["colleague-1"]);
}

#[tokio::test]
async fn tenant_only_pdp_created_chat_row_carries_the_callers_user_id() {
    let env = env_with_authz(AuthzMode::TenantOnly).await;
    let user = TestUser::new();

    let id = chat_id(&create_default_chat(&env, &user).await);

    let row = chat_row(&env, &id).await.unwrap();
    assert_eq!(row.tenant_hex, uuid_hex(user.tenant_id));
    assert_eq!(row.user_hex, uuid_hex(user.user_id));
}

// ===========================================================================
// Listing and row ownership
// ===========================================================================

#[tokio::test]
async fn list_chats_returns_only_the_callers_chats() {
    let env = TestEnv::new().await;
    let alice = TestUser::new();
    let bob_same_tenant = TestUser::in_tenant(alice.tenant_id);
    let carol_other_tenant = TestUser::new();
    let alice_ids = create_titled_chats(&env, &alice, &["alice-1", "alice-2"]).await;
    create_titled_chats(&env, &bob_same_tenant, &["bob-1"]).await;
    create_titled_chats(&env, &carol_other_tenant, &["carol-1"]).await;

    let alice_page = list_chats_ok(&env, &alice, &[]).await;
    let bob_page = list_chats_ok(&env, &bob_same_tenant, &[]).await;
    let carol_page = list_chats_ok(&env, &carol_other_tenant, &[]).await;

    let mut expected = alice_ids;
    expected.reverse();
    assert_eq!(item_ids(&alice_page), expected);
    assert_eq!(item_titles(&bob_page), vec!["bob-1"]);
    assert_eq!(item_titles(&carol_page), vec!["carol-1"]);
}

#[tokio::test]
async fn list_filter_by_a_foreign_chat_id_returns_nothing() {
    let env = TestEnv::new().await;
    let owner = TestUser::new();
    let intruder = TestUser::in_tenant(owner.tenant_id);
    let id = chat_id(&create_titled_chat(&env, &owner, "Private").await);

    let filter = format!("id eq {id}");
    let page = list_chats_ok(&env, &intruder, &[("$filter", filter.as_str())]).await;

    assert!(items(&page).is_empty(), "{page}");
}

#[tokio::test]
async fn created_chat_row_carries_tenant_and_user_from_the_security_context() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    let id = chat_id(&create_default_chat(&env, &user).await);

    let row = chat_row(&env, &id).await.unwrap();
    assert_eq!(row.tenant_hex, uuid_hex(user.tenant_id));
    assert_eq!(row.user_hex, uuid_hex(user.user_id));
}

#[tokio::test]
async fn created_chat_ignores_identity_supplied_in_the_request_body() {
    let env = TestEnv::new().await;
    let user = TestUser::new();
    let spoofed_tenant = Uuid::new_v4();
    let spoofed_user = Uuid::new_v4();

    let resp = common::post_chat(
        &env,
        &user,
        json!({ "title": "Spoof", "tenant_id": spoofed_tenant, "user_id": spoofed_user }),
    )
    .await;

    expect_status(&resp, StatusCode::CREATED);
    let row = chat_row(&env, &chat_id(&resp.json())).await.unwrap();
    assert_eq!(row.tenant_hex, uuid_hex(user.tenant_id));
    assert_eq!(row.user_hex, uuid_hex(user.user_id));
}

#[tokio::test]
async fn foreign_chat_and_missing_chat_are_indistinguishable() {
    let env = TestEnv::new().await;
    let owner = TestUser::new();
    let intruder = TestUser::in_tenant(owner.tenant_id);
    let id = chat_id(&create_default_chat(&env, &owner).await);

    let foreign = get_chat(&env, &intruder, &id).await;
    let missing = get_chat(&env, &intruder, &Uuid::new_v4().to_string()).await;

    let foreign_problem = assert_not_found(&foreign, CHAT_RESOURCE_TYPE);
    let missing_problem = assert_not_found(&missing, CHAT_RESOURCE_TYPE);
    assert_eq!(foreign_problem["type"], missing_problem["type"]);
    assert_eq!(foreign_problem["title"], missing_problem["title"]);
    assert_eq!(foreign_problem["status"], missing_problem["status"]);
}

// ===========================================================================
// Fail-closed authorization on the chat routes
// ===========================================================================

fn chat_routes(chat_id: &str) -> Vec<(Method, String, Option<Value>)> {
    vec![
        (
            Method::POST,
            CHATS_PATH.to_owned(),
            Some(json!({ "title": "t" })),
        ),
        (Method::GET, CHATS_PATH.to_owned(), None),
        (Method::GET, chat_path(chat_id), None),
        (
            Method::PATCH,
            chat_path(chat_id),
            Some(json!({ "title": "t" })),
        ),
        (Method::DELETE, chat_path(chat_id), None),
    ]
}

/// Sends every chat route as a fresh user under `mode`; the chat id is random because a
/// refused decision must not depend on the chat existing.
async fn all_chat_route_responses(mode: AuthzMode) -> (TestEnv, Vec<(String, TestResponse)>) {
    let env = env_with_authz(mode).await;
    let user = TestUser::new();
    let mut responses = Vec::new();
    for (method, path, body) in chat_routes(&Uuid::new_v4().to_string()) {
        let label = format!("{method} {path}");
        responses.push((label, env.request(&user, method, &path, body).await));
    }
    (env, responses)
}

#[tokio::test]
async fn deny_mode_rejects_every_chat_route_with_403_permission_denied() {
    let (_env, responses) = all_chat_route_responses(AuthzMode::Deny).await;

    for (label, resp) in &responses {
        let problem = assert_problem(resp, 403, Some("permission_denied"));
        assert!(problem["context"].is_object(), "{label}: {problem}");
    }
}

#[tokio::test]
async fn deny_mode_reports_the_authz_denied_reason() {
    let (_env, responses) = all_chat_route_responses(AuthzMode::Deny).await;

    for (label, resp) in &responses {
        let problem = assert_problem(resp, 403, Some("permission_denied"));
        assert_eq!(
            problem["context"]["reason"].as_str(),
            Some("AUTHZ_DENIED"),
            "{label}: {problem}"
        );
    }
}

#[tokio::test]
async fn deny_mode_on_a_missing_chat_is_403_not_404() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = get_chat(&env, &TestUser::new(), &Uuid::new_v4().to_string()).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn unavailable_mode_rejects_every_chat_route_with_503_service_unavailable() {
    let (_env, responses) = all_chat_route_responses(AuthzMode::Unavailable).await;

    for (_label, resp) in &responses {
        assert_problem(resp, 503, Some("service_unavailable"));
    }
}

#[tokio::test]
async fn unavailable_mode_sends_retry_after_5_on_every_chat_route() {
    let (_env, responses) = all_chat_route_responses(AuthzMode::Unavailable).await;

    for (label, resp) in &responses {
        let retry_after = resp
            .headers
            .get(http::header::RETRY_AFTER)
            .unwrap_or_else(|| panic!("{label}: 503 must carry Retry-After"));
        assert_eq!(retry_after.to_str().unwrap(), "5", "{label}");
    }
}

#[tokio::test]
async fn unavailable_mode_problem_context_carries_retry_after_seconds_5() {
    let env = env_with_authz(AuthzMode::Unavailable).await;

    let resp = get_chat(&env, &TestUser::new(), &Uuid::new_v4().to_string()).await;

    let problem = assert_problem(&resp, 503, Some("service_unavailable"));
    assert_eq!(
        problem["context"]["retry_after_seconds"],
        json!(5),
        "{problem}"
    );
}

#[tokio::test]
async fn no_constraints_mode_rejects_every_chat_route_with_403() {
    let (_env, responses) = all_chat_route_responses(AuthzMode::NoConstraints).await;

    for (_label, resp) in &responses {
        assert_problem(resp, 403, Some("permission_denied"));
    }
}

#[tokio::test]
async fn refused_create_never_inserts_a_chat_row() {
    for mode in [
        AuthzMode::Deny,
        AuthzMode::Unavailable,
        AuthzMode::NoConstraints,
    ] {
        let env = env_with_authz(mode).await;

        let resp = common::post_chat(&env, &TestUser::new(), json!({ "title": "t" })).await;

        assert!(!resp.status.is_success(), "create must be refused");
        assert_eq!(chat_row_count(&env).await, 0);
    }
}

#[tokio::test]
async fn invalid_title_is_rejected_with_400_before_the_authorization_check() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = common::post_chat(&env, &TestUser::new(), json!({ "title": "   " })).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        common::field_violation_reason(&problem, "title").as_deref(),
        Some("INVALID_TITLE")
    );
}

// ===========================================================================
// Fail-closed authorization on the models routes (decision only, no constraints)
// ===========================================================================

#[tokio::test]
async fn deny_mode_rejects_list_models_with_403() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = common::get(&env, &TestUser::new(), MODELS_PATH).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn deny_mode_rejects_get_model_with_403() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = common::get(&env, &TestUser::new(), &model_path(PREMIUM_MODEL)).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn deny_mode_is_checked_before_the_model_lookup() {
    let env = env_with_authz(AuthzMode::Deny).await;

    let resp = common::get(&env, &TestUser::new(), &model_path("no-such-model")).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn unavailable_mode_rejects_list_models_with_503_and_retry_after_5() {
    let env = env_with_authz(AuthzMode::Unavailable).await;

    let resp = common::get(&env, &TestUser::new(), MODELS_PATH).await;

    assert_problem(&resp, 503, Some("service_unavailable"));
    let retry_after = resp.headers.get(http::header::RETRY_AFTER).unwrap();
    assert_eq!(retry_after.to_str().unwrap(), "5");
}

#[tokio::test]
async fn no_constraints_mode_still_serves_models_because_only_a_decision_is_required() {
    let env = env_with_authz(AuthzMode::NoConstraints).await;

    let resp = common::get(&env, &TestUser::new(), MODELS_PATH).await;

    expect_status(&resp, StatusCode::OK);
}
