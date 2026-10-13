#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the REST registration of the three attachment routes
//! (`cpt-cf-mini-chat-dod-attachments-rest-routes`): operations, license gate,
//! the multipart request body, the documented responses and the schemas of
//! `docs/api/api.json`.

use std::collections::BTreeSet;

use axum::Router;
use serde_json::Value;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::api::openapi_registry::OpenApiInfo;
use toolkit::api::operation_builder::{OperationSpec, RequestBodySchema, ResponseSpec};

use super::{BASE_LICENSE_FEATURE, register_attachment_routes};
use crate::attachment_test_env::AttEnv;

const UPLOAD: &str = "POST:/mini-chat/v1/chats/{id}/attachments";
const GET: &str = "GET:/mini-chat/v1/chats/{id}/attachments/{attachment_id}";
const DELETE: &str = "DELETE:/mini-chat/v1/chats/{id}/attachments/{attachment_id}";
const ALL: [&str; 3] = [UPLOAD, GET, DELETE];

fn spec(env: &AttEnv, key: &str) -> OperationSpec {
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

fn schema_name(spec: &OperationSpec, status: u16) -> String {
    response(spec, status)
        .schema
        .unwrap_or_else(|| panic!("the {status} response has no schema"))
        .schema_name()
        .to_owned()
}

fn set(values: &[u16]) -> BTreeSet<u16> {
    values.iter().copied().collect()
}

#[tokio::test]
async fn exactly_the_three_attachment_operations_are_registered_under_the_url_prefix() {
    let env = AttEnv::new().await;

    let mut registered: Vec<String> = env
        .openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();
    registered.sort();

    let mut expected: Vec<String> = ALL.iter().map(|k| (*k).to_owned()).collect();
    expected.sort();
    assert_eq!(registered, expected);
}

#[tokio::test]
async fn the_operations_follow_a_configured_url_prefix() {
    let env = AttEnv::new().await;
    let openapi = OpenApiRegistryImpl::new();

    let _router = register_attachment_routes(Router::new(), &openapi, env.service, "/chat-api/");

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
    let env = AttEnv::new().await;

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
async fn the_operations_have_their_ids_the_attachments_tag_and_a_summary() {
    let env = AttEnv::new().await;

    for (key, id) in [
        (UPLOAD, "mini_chat.upload_attachment"),
        (GET, "mini_chat.get_attachment"),
        (DELETE, "mini_chat.delete_attachment"),
    ] {
        let spec = spec(&env, key);

        assert_eq!(spec.operation_id.as_deref(), Some(id), "{key}");
        assert_eq!(spec.tags, vec!["Mini Chat Attachments".to_owned()], "{key}");
        assert!(
            spec.summary.as_deref().is_some_and(|s| !s.is_empty()),
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_upload_takes_a_required_multipart_body_with_a_binary_file_field() {
    let env = AttEnv::new().await;

    let body = spec(&env, UPLOAD).request_body.expect("a request body");

    assert_eq!(body.content_type, "multipart/form-data");
    assert!(body.required);
    assert!(
        matches!(&body.schema, RequestBodySchema::MultipartFile { field_name } if field_name == "file"),
        "{:?}",
        body.schema
    );
    assert!(spec(&env, GET).request_body.is_none());
    assert!(spec(&env, DELETE).request_body.is_none());
}

#[tokio::test]
async fn the_success_responses_have_their_schemas() {
    let env = AttEnv::new().await;

    assert_eq!(schema_name(&spec(&env, UPLOAD), 201), "AttachmentDetailDto");
    assert_eq!(schema_name(&spec(&env, GET), 200), "AttachmentDetailDto");
    let delete = spec(&env, DELETE);
    assert!(statuses(&delete).contains(&204));
    assert!(response(&delete, 204).schema.is_none(), "no body");
}

#[tokio::test]
async fn the_upload_documents_its_problem_responses() {
    let env = AttEnv::new().await;

    let statuses = statuses(&spec(&env, UPLOAD));

    assert_eq!(
        statuses,
        set(&[201, 400, 401, 403, 404, 409, 429, 500, 503])
    );
}

#[tokio::test]
async fn the_get_and_delete_document_their_problem_responses() {
    let env = AttEnv::new().await;

    assert_eq!(
        statuses(&spec(&env, GET)),
        set(&[200, 400, 401, 403, 404, 500, 503])
    );
    assert_eq!(
        statuses(&spec(&env, DELETE)),
        set(&[204, 400, 401, 403, 404, 409, 500, 503])
    );
}

#[tokio::test]
async fn every_503_declares_the_retry_after_header() {
    let env = AttEnv::new().await;

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

#[tokio::test]
async fn the_201_has_no_location_header() {
    let env = AttEnv::new().await;

    let created = response(&spec(&env, UPLOAD), 201);

    assert!(
        created
            .headers
            .iter()
            .all(|h| !h.name.eq_ignore_ascii_case("location"))
    );
}

#[tokio::test]
async fn the_path_parameters_are_documented() {
    let env = AttEnv::new().await;

    let upload: BTreeSet<String> = spec(&env, UPLOAD)
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let get: BTreeSet<String> = spec(&env, GET)
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect();

    assert!(upload.contains("id"));
    assert!(get.contains("id") && get.contains("attachment_id"));
}

// ================================================================================================
// The generated document against docs/api/api.json
// ================================================================================================

fn generated(env: &AttEnv) -> Value {
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
async fn the_attachment_schemas_match_the_committed_api_document() {
    let env = AttEnv::new().await;
    let generated = generated(&env);
    let committed = committed();

    for name in [
        "AttachmentDetailDto",
        "AttachmentKindDto",
        "AttachmentStatusDto",
        "ImgThumbnailDto",
    ] {
        let ours = &generated["components"]["schemas"][name];
        let theirs = &committed["components"]["schemas"][name];

        assert!(!ours.is_null(), "{name} is generated");
        assert_eq!(ours["required"], theirs["required"], "{name}: required");
        let ours_props: BTreeSet<&String> = ours
            .get("properties")
            .and_then(Value::as_object)
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        let theirs_props: BTreeSet<&String> = theirs
            .get("properties")
            .and_then(Value::as_object)
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        assert_eq!(ours_props, theirs_props, "{name}: properties");
        assert_eq!(ours.get("enum"), theirs.get("enum"), "{name}: enum");
    }
}

#[tokio::test]
async fn the_generated_paths_and_operations_match_the_committed_api_document() {
    let env = AttEnv::new().await;
    let generated = generated(&env);
    let committed = committed();

    for (path, method) in [
        ("/mini-chat/v1/chats/{id}/attachments", "post"),
        (
            "/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "get",
        ),
        (
            "/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "delete",
        ),
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
    let body = &generated["paths"]["/mini-chat/v1/chats/{id}/attachments"]["post"]["requestBody"];
    assert_eq!(body["required"], true);
    let schema = &body["content"]["multipart/form-data"]["schema"];
    assert_eq!(schema["properties"]["file"]["format"], "binary");
    assert_eq!(schema["required"], serde_json::json!(["file"]));
}
