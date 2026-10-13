#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The REST registration of the three turn-mutation routes
//! (`cpt-cf-mini-chat-dod-turn-mutations-rest-routes`): operations, license
//! gate, request bodies, the documented responses, the url prefix and the
//! schemas of `docs/api/api.json`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Router;
use serde_json::Value;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::api::openapi_registry::OpenApiInfo;
use toolkit::api::operation_builder::{OperationSpec, RequestBodySchema, ResponseSpec};

use super::{BASE_LICENSE_FEATURE, register_turn_mutation_routes};
use crate::mutation_test_env::MutationEnv;

const RETRY: &str = "POST:/mini-chat/v1/chats/{id}/turns/{request_id}/retry";
const EDIT: &str = "PATCH:/mini-chat/v1/chats/{id}/turns/{request_id}";
const DELETE: &str = "DELETE:/mini-chat/v1/chats/{id}/turns/{request_id}";
const ALL: [&str; 3] = [RETRY, EDIT, DELETE];

fn spec(env: &MutationEnv, key: &str) -> OperationSpec {
    env.openapi
        .operation_specs
        .get(key)
        .unwrap_or_else(|| {
            let known: Vec<String> = env
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

fn statuses(spec: &OperationSpec) -> BTreeSet<u16> {
    spec.responses.iter().map(|r| r.status).collect()
}

fn response(spec: &OperationSpec, status: u16) -> ResponseSpec {
    spec.responses
        .iter()
        .find(|r| r.status == status)
        .unwrap_or_else(|| panic!("no {status} response: {:?}", statuses(spec)))
        .clone()
}

fn set(values: &[u16]) -> BTreeSet<u16> {
    values.iter().copied().collect()
}

#[tokio::test]
async fn the_three_mutation_operations_are_registered_next_to_the_status_route() {
    let env = MutationEnv::new().await;

    let mut turn_keys: Vec<String> = env
        .openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .filter(|key| key.contains("/turns/"))
        .collect();
    turn_keys.sort();

    let mut expected = vec![
        RETRY.to_owned(),
        EDIT.to_owned(),
        DELETE.to_owned(),
        "GET:/mini-chat/v1/chats/{id}/turns/{request_id}".to_owned(),
    ];
    expected.sort();
    assert_eq!(turn_keys, expected);
}

#[tokio::test]
async fn the_operations_follow_a_configured_url_prefix() {
    let env = MutationEnv::new().await;
    let openapi = OpenApiRegistryImpl::new();

    let _router = register_turn_mutation_routes(
        Router::new(),
        &openapi,
        Arc::clone(&env.service),
        "/chat-api/",
    );

    let mut registered: Vec<String> = openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();
    registered.sort();
    let mut expected: Vec<String> = ALL
        .iter()
        .map(|k| k.replace("/mini-chat/", "/chat-api/"))
        .collect();
    expected.sort();
    assert_eq!(
        registered, expected,
        "a trailing slash of the prefix is ignored"
    );
}

#[tokio::test]
async fn every_route_requires_authentication_and_the_base_license_feature() {
    let env = MutationEnv::new().await;

    for key in ALL {
        let spec = spec(&env, key);

        assert!(spec.authenticated, "{key}");
        let license = spec
            .license_requirement
            .unwrap_or_else(|| panic!("{key} declares no license feature"));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE_FEATURE.to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_operations_have_their_ids_the_turns_tag_and_a_summary() {
    let env = MutationEnv::new().await;

    for (key, id) in [
        (RETRY, "mini_chat.retry_turn"),
        (EDIT, "mini_chat.edit_turn"),
        (DELETE, "mini_chat.delete_turn"),
    ] {
        let spec = spec(&env, key);

        assert_eq!(spec.operation_id.as_deref(), Some(id), "{key}");
        assert_eq!(spec.tags, vec!["Mini Chat Turns".to_owned()], "{key}");
        assert!(
            spec.summary.as_deref().is_some_and(|s| !s.is_empty()),
            "{key}"
        );
    }
}

#[tokio::test]
async fn both_path_ids_are_documented_on_every_route() {
    let env = MutationEnv::new().await;

    for key in ALL {
        let names: BTreeSet<String> = spec(&env, key)
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect();

        assert!(
            names.contains("id") && names.contains("request_id"),
            "{key}: {names:?}"
        );
    }
}

#[tokio::test]
async fn the_edit_takes_a_required_json_edit_turn_request_and_the_others_no_body() {
    let env = MutationEnv::new().await;

    let body = spec(&env, EDIT).request_body.expect("a request body");

    assert_eq!(body.content_type, "application/json");
    assert!(body.required);
    assert!(
        matches!(&body.schema, RequestBodySchema::Ref { schema_name } if schema_name == "EditTurnRequest"),
        "{:?}",
        body.schema
    );
    assert!(spec(&env, RETRY).request_body.is_none());
    assert!(spec(&env, DELETE).request_body.is_none());
}

#[tokio::test]
async fn retry_and_edit_answer_200_with_the_sse_event_schema_and_delete_204_without_a_body() {
    let env = MutationEnv::new().await;

    for key in [RETRY, EDIT] {
        let ok = response(&spec(&env, key), 200);

        assert_eq!(ok.content_type, "text/event-stream", "{key}");
        assert_eq!(
            ok.schema.as_ref().unwrap().schema_name(),
            "MiniChatSseEvent",
            "{key}"
        );
    }
    let delete = spec(&env, DELETE);
    assert!(statuses(&delete).contains(&204));
    assert!(response(&delete, 204).schema.is_none(), "no body");
    assert!(!statuses(&delete).contains(&200));
}

#[tokio::test]
async fn each_route_documents_exactly_its_problem_responses() {
    let env = MutationEnv::new().await;

    assert_eq!(
        statuses(&spec(&env, RETRY)),
        set(&[200, 400, 401, 403, 404, 409, 429, 500, 503])
    );
    assert_eq!(
        statuses(&spec(&env, EDIT)),
        set(&[200, 400, 401, 403, 404, 409, 422, 429, 500, 503])
    );
    assert_eq!(
        statuses(&spec(&env, DELETE)),
        set(&[204, 400, 401, 403, 404, 409, 500, 503])
    );
}

#[tokio::test]
async fn every_503_declares_the_retry_after_header() {
    let env = MutationEnv::new().await;

    for key in ALL {
        let unavailable = response(&spec(&env, key), 503);

        assert!(
            unavailable
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
            "{key}: {:?}",
            unavailable.headers
        );
    }
}

#[test]
fn the_edit_request_schema_has_the_one_required_content_field() {
    let schema = serde_json::to_value(
        <super::super::dto::EditTurnRequest as utoipa::PartialSchema>::schema(),
    )
    .unwrap();

    let properties: BTreeSet<String> = schema["properties"]
        .as_object()
        .expect("an object schema")
        .keys()
        .cloned()
        .collect();
    assert_eq!(properties, ["content".to_owned()].into_iter().collect());
    assert_eq!(schema["required"], serde_json::json!(["content"]));
}

// ================================================================================================
// The generated document against docs/api/api.json
// ================================================================================================

fn generated(env: &MutationEnv) -> Value {
    let doc = env
        .openapi
        .build_openapi(&OpenApiInfo::default())
        .expect("the OpenAPI document");
    serde_json::to_value(&doc).unwrap()
}

fn committed() -> Value {
    serde_json::from_str(include_str!("../../../../../../docs/api/api.json"))
        .expect("docs/api/api.json")
}

#[tokio::test]
async fn the_edit_turn_request_schema_matches_the_committed_api_document() {
    let env = MutationEnv::new().await;
    let ours = &generated(&env)["components"]["schemas"]["EditTurnRequest"];
    let theirs = &committed()["components"]["schemas"]["EditTurnRequest"];

    assert!(!ours.is_null(), "EditTurnRequest is generated");
    assert_eq!(ours["required"], theirs["required"]);
    let props = |value: &Value| -> BTreeSet<String> {
        value["properties"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    assert_eq!(props(ours), props(theirs));
}

#[tokio::test]
async fn the_generated_paths_and_operations_match_the_committed_api_document() {
    let env = MutationEnv::new().await;
    let generated = generated(&env);
    let committed = committed();

    for (path, method) in [
        ("/mini-chat/v1/chats/{id}/turns/{request_id}/retry", "post"),
        ("/mini-chat/v1/chats/{id}/turns/{request_id}", "patch"),
        ("/mini-chat/v1/chats/{id}/turns/{request_id}", "delete"),
    ] {
        let ours = &generated["paths"][path][method];
        let theirs = &committed["paths"][path][method];

        assert!(!ours.is_null(), "{method} {path} is generated");
        assert_eq!(
            ours["operationId"], theirs["operationId"],
            "{method} {path}"
        );
        assert_eq!(ours["tags"], theirs["tags"], "{method} {path}");
        let ours_status: BTreeSet<&String> =
            ours["responses"].as_object().unwrap().keys().collect();
        let theirs_status: BTreeSet<&String> =
            theirs["responses"].as_object().unwrap().keys().collect();
        assert_eq!(ours_status, theirs_status, "{method} {path}");
    }
}
