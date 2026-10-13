#![allow(clippy::unwrap_used, clippy::expect_used)]
//! In-process REST tests of the CRUD handlers (extractors, status codes, headers, JSON shape).

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use http::{Request, StatusCode};
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_routes;
use crate::domain::service::chats::test_rows::{
    enc, env_with_pdp, insert_turn, insert_turn_messages,
};
use crate::domain::service::test_support::{DenyPdp, TENANT_A, TestEnv, ctx_a1};

fn router(env: &TestEnv) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(
        Router::new(),
        &openapi,
        Arc::clone(&env.services),
        &env.deps.cfg.url_prefix,
    )
    .layer(axum::middleware::from_fn(
        toolkit::api::canonical_error_middleware,
    ))
}

fn request(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    ctx: SecurityContext,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(body.map_or_else(Body::empty, |j| Body::from(serde_json::to_vec(&j).unwrap())))
        .unwrap();
    req.extensions_mut().insert(ctx);
    req
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, http::HeaderMap, serde_json::Value) {
    let resp = router
        .clone()
        .oneshot(request(method, uri, body, ctx_a1()))
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, headers, json)
}

#[tokio::test]
async fn chat_crud_over_http() {
    let env = TestEnv::default_env().await;
    let r = router(&env);

    // Create (untitled) -> 201 + Location, no title key.
    let (st, headers, body) = call(
        &r,
        "POST",
        "/mini-chat/v1/chats",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(headers["location"], format!("/mini-chat/v1/chats/{id}"));
    assert!(body.get("title").is_none(), "{body}");
    assert_eq!(body["model"], "gpt-premium");
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["message_count"], 0);
    assert!(body.get("user_id").is_none());

    // null title / null model behave like absent.
    let (st, _, body) = call(
        &r,
        "POST",
        "/mini-chat/v1/chats",
        Some(serde_json::json!({"title": null, "model": null})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    // Invalid title -> 400 INVALID_TITLE.
    let (st, _, body) = call(
        &r,
        "POST",
        "/mini-chat/v1/chats",
        Some(serde_json::json!({"title": "  "})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "INVALID_TITLE"
    );

    // Get.
    let (st, _, body) = call(&r, "GET", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["id"], id);

    // Patch with unknown field -> 200, model unchanged.
    let (st, _, body) = call(
        &r,
        "PATCH",
        &format!("/mini-chat/v1/chats/{id}"),
        Some(serde_json::json!({"title": "Renamed", "model": "gpt-standard"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "Renamed");
    assert_eq!(body["model"], "gpt-premium");

    // Patch without title -> 422; with null -> 422; malformed -> 400.
    for b in [serde_json::json!({}), serde_json::json!({"title": null})] {
        let (st, _, body) = call(&r, "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(b)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    // List.
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/chats?limit=1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["page_info"]["limit"], 1);
    assert!(body["page_info"]["next_cursor"].is_string());

    // List: limit=0, bad filter, unsupported option.
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/chats?limit=0", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "INVALID_LIMIT"
    );
    let (st, _, body) = call(
        &r,
        "GET",
        &format!(
            "/mini-chat/v1/chats?{}={}",
            enc("$filter"),
            enc("bogus eq 1")
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.odata.query.v1~"
    );
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/chats?%24skip=1", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

    // Delete -> 204, then 404.
    let (st, _, _) = call(&r, "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, body) = call(&r, "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.chat.v1~"
    );

    // Non-UUID path param -> 400 invalid_path_params.
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/chats/not-a-uuid", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "invalid_path_params"
    );
    env.shutdown().await;
}

#[tokio::test]
async fn messages_reactions_turns_models_over_http() {
    let env = TestEnv::default_env().await;
    let r = router(&env);
    let (_, _, body) = call(
        &r,
        "POST",
        "/mini-chat/v1/chats",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    let chat_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let (user, asst) =
        insert_turn_messages(&env, TENANT_A, chat_id, OffsetDateTime::now_utc()).await;
    insert_turn(
        &env,
        TENANT_A,
        chat_id,
        user.request_id.unwrap(),
        "completed",
        None,
        Some(asst.id),
        false,
    )
    .await;

    // Messages.
    let (st, _, body) = call(
        &r,
        "GET",
        &format!("/mini-chat/v1/chats/{chat_id}/messages"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["attachments"], serde_json::json!([]));
    assert!(items[0]["my_reaction"].is_null());
    assert!(items[0].as_object().unwrap().contains_key("my_reaction"));
    assert!(items[0].get("model").is_none());
    assert!(items[0].get("input_tokens").is_none());
    assert_eq!(items[1]["role"], "assistant");
    assert_eq!(items[1]["model"], "gpt-premium");
    assert_eq!(items[1]["input_tokens"], 100);
    assert_eq!(items[1]["output_tokens"], 50);
    assert_eq!(items[0]["request_id"], items[1]["request_id"]);

    // Reaction: invalid value -> 400 INVALID_REACTION; missing field -> 422.
    let uri = format!(
        "/mini-chat/v1/chats/{chat_id}/messages/{}/reaction",
        asst.id
    );
    let (st, _, body) = call(
        &r,
        "PUT",
        &uri,
        Some(serde_json::json!({"reaction": "love"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["context"]["field_violations"][0]["reason"],
        "INVALID_REACTION"
    );
    let (st, _, _) = call(&r, "PUT", &uri, Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _, body) = call(
        &r,
        "PUT",
        &uri,
        Some(serde_json::json!({"reaction": "like"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["reaction"], "like");
    assert_eq!(body["message_id"], asst.id.to_string());
    assert!(body["created_at"].is_string());
    let (_, _, body) = call(
        &r,
        "GET",
        &format!("/mini-chat/v1/chats/{chat_id}/messages"),
        None,
    )
    .await;
    assert_eq!(body["items"][1]["my_reaction"], "like");
    let (st, _, _) = call(&r, "DELETE", &uri, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, _) = call(&r, "DELETE", &uri, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let user_uri = format!(
        "/mini-chat/v1/chats/{chat_id}/messages/{}/reaction",
        user.id
    );
    let (st, _, body) = call(
        &r,
        "PUT",
        &user_uri,
        Some(serde_json::json!({"reaction": "like"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["context"]["violations"][0]["subject"],
        "reaction_target"
    );
    let (st, _, _) = call(
        &r,
        "PUT",
        &format!("/mini-chat/v1/chats/{chat_id}/messages/zzz/reaction"),
        Some(serde_json::json!({"reaction": "like"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Turn status.
    let (st, _, body) = call(
        &r,
        "GET",
        &format!(
            "/mini-chat/v1/chats/{chat_id}/turns/{}",
            user.request_id.unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "done");
    assert_eq!(body["assistant_message_id"], asst.id.to_string());
    assert!(body.get("error_code").is_none());
    let (st, _, body) = call(
        &r,
        "GET",
        &format!("/mini-chat/v1/chats/{chat_id}/turns/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.turn.v1~"
    );

    // Models.
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/models", None).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-premium", "gpt-standard"]);
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/models/gpt-standard", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["tier"], "standard");
    let (st, _, body) = call(&r, "GET", "/mini-chat/v1/models/gpt-disabled", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.model.v1~"
    );

    env.shutdown().await;
}

#[tokio::test]
async fn invalid_reaction_is_checked_before_pep_over_http() {
    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let r = router(&env);
    let uri = format!(
        "/mini-chat/v1/chats/{}/messages/{}/reaction",
        Uuid::new_v4(),
        Uuid::new_v4()
    );
    let (st, _, body) = call(
        &r,
        "PUT",
        &uri,
        Some(serde_json::json!({"reaction": "meh"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let (st, _, body) = call(
        &r,
        "PUT",
        &uri,
        Some(serde_json::json!({"reaction": "like"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    env.shutdown().await;
}
