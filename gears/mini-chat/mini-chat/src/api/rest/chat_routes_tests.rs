//! Registration of the five chat routes: paths under the URL prefix, operation ids, the base
//! license feature, declared responses and headers, throttling, the `OpenAPI` schemas and the
//! `OData` extensions of the reference subset, and the deferred scope that stays unregistered.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: definition of done
//! `cpt-cf-mini-chat-dod-chat-management-routes-and-deferred`; flows `create-chat` and
//! `list-chats`; state `chat-lifecycle` (no temporary chat).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use toolkit::api::OpenApiInfo;
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{CHATS, RigOptions, a1, rig, rig_with};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

/// `(method, path, operation id, declared statuses)` of the five chat operations in the
/// reference `OpenAPI` subset.
type Expected = (&'static str, &'static str, &'static str, Vec<u16>);

fn expected_operations() -> Vec<Expected> {
    vec![
        (
            "POST",
            "/mini-chat/v1/chats",
            "mini_chat.create_chat",
            vec![201, 400, 401, 403, 422, 500, 503],
        ),
        (
            "GET",
            "/mini-chat/v1/chats",
            "mini_chat.list_chats",
            vec![200, 400, 401, 403, 500, 503],
        ),
        (
            "GET",
            "/mini-chat/v1/chats/{id}",
            "mini_chat.get_chat",
            vec![200, 400, 401, 403, 404, 500, 503],
        ),
        (
            "PATCH",
            "/mini-chat/v1/chats/{id}",
            "mini_chat.update_chat",
            vec![200, 400, 401, 403, 404, 422, 500, 503],
        ),
        (
            "DELETE",
            "/mini-chat/v1/chats/{id}",
            "mini_chat.delete_chat",
            vec![204, 400, 401, 403, 404, 500, 503],
        ),
    ]
}

#[tokio::test]
async fn the_five_chat_operations_are_registered_under_the_url_prefix() {
    let rig = rig(Pdp::TenantOnly).await;
    for (method, path, operation_id, _) in expected_operations() {
        let op = rig
            .openapi
            .op(method, path)
            .unwrap_or_else(|| panic!("{method} {path} is not registered"));
        assert_eq!(
            op.operation_id.as_deref(),
            Some(operation_id),
            "{method} {path}"
        );
        assert_eq!(
            op.tags,
            vec!["Mini Chat Chats".to_owned()],
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_routes_follow_a_configured_url_prefix() {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.prefix = "/chat-api";
    let rig = rig_with(options).await;
    assert!(rig.openapi.op("POST", "/chat-api/v1/chats").is_some());
    assert!(rig.openapi.op("GET", "/chat-api/v1/chats/{id}").is_some());
    let moved = rig.get("/chat-api/v1/chats", &a1()).await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.text);
    let old = rig.get(CHATS, &a1()).await;
    assert_eq!(old.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn every_chat_route_is_authenticated_and_declares_only_the_base_license_feature() {
    let rig = rig(Pdp::TenantOnly).await;
    for (method, path, _, _) in expected_operations() {
        let op = rig.openapi.op(method, path).expect("registered");
        assert!(op.authenticated, "{method} {path}");
        let license = op
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{method} {path} declares no license feature"));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE.to_owned()],
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_declared_responses_are_those_of_the_reference_openapi_without_the_415() {
    let rig = rig(Pdp::TenantOnly).await;
    for (method, path, _, statuses) in expected_operations() {
        let op = rig.openapi.op(method, path).expect("registered");
        let declared: BTreeSet<u16> = op.responses.iter().map(|r| r.status).collect();
        let expected: BTreeSet<u16> = statuses.into_iter().collect();
        assert_eq!(declared, expected, "{method} {path}");
        assert!(
            !declared.contains(&415),
            "{method} {path} must not declare the 415"
        );
        for response in op.responses.iter().filter(|r| r.status >= 400) {
            assert_eq!(
                response.content_type, "application/problem+json",
                "{method} {path} {}",
                response.status
            );
        }
        let unavailable = op.responses.iter().find(|r| r.status == 503).unwrap();
        assert!(
            unavailable
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
            "{method} {path}: Retry-After on the 503"
        );
    }
}

#[tokio::test]
async fn the_create_operation_declares_the_location_header_on_the_201() {
    let rig = rig(Pdp::TenantOnly).await;
    let op = rig.openapi.op("POST", "/mini-chat/v1/chats").unwrap();
    let created = op.responses.iter().find(|r| r.status == 201).unwrap();
    assert!(
        created
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("location")),
        "{:?}",
        created.headers
    );
}

#[tokio::test]
async fn only_the_create_operation_is_bound_to_the_throttling_zones() {
    let rig = rig(Pdp::TenantOnly).await;
    let create = rig.openapi.op("POST", "/mini-chat/v1/chats").unwrap();
    let throttling = create.throttling.as_ref().expect("create is throttled");
    assert_eq!(
        throttling.rate_limit_zone.as_deref(),
        Some("rl_mini_chat_chat")
    );
    assert_eq!(
        throttling.in_flight_limit_zone.as_deref(),
        Some("ifl_mini_chat_chat")
    );
    for (method, path, _, _) in expected_operations() {
        if (method, path) != ("POST", "/mini-chat/v1/chats") {
            let op = rig.openapi.op(method, path).unwrap();
            assert!(op.throttling.is_none(), "{method} {path}");
        }
    }
}

#[tokio::test]
async fn the_item_operations_declare_the_id_path_parameter() {
    let rig = rig(Pdp::TenantOnly).await;
    for method in ["GET", "PATCH", "DELETE"] {
        let op = rig.openapi.op(method, "/mini-chat/v1/chats/{id}").unwrap();
        assert!(
            op.params.iter().any(|p| p.name == "id" && p.required),
            "{method}"
        );
    }
}

fn document(rig: &crate::test_support::chats::ChatRig) -> Value {
    let document = rig
        .openapi
        .inner
        .build_openapi(&OpenApiInfo::default())
        .expect("openapi builds");
    serde_json::to_value(&document).unwrap()
}

#[tokio::test]
async fn the_openapi_document_declares_the_chat_schemas_of_the_reference_subset() {
    let rig = rig(Pdp::TenantOnly).await;
    let value = document(&rig);
    let schemas = &value["components"]["schemas"];

    let detail = &schemas["ChatDetailDto"];
    let required: BTreeSet<&str> = detail["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        required,
        BTreeSet::from([
            "id",
            "model",
            "is_temporary",
            "message_count",
            "created_at",
            "updated_at"
        ]),
        "title is optional"
    );
    for property in [
        "id",
        "model",
        "title",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
    ] {
        assert!(detail["properties"].get(property).is_some(), "{property}");
    }
    for internal in ["user_id", "tenant_id", "deleted_at"] {
        assert!(detail["properties"].get(internal).is_none(), "{internal}");
    }

    let create = &schemas["CreateChatReq"];
    assert!(create["properties"].get("title").is_some());
    assert!(create["properties"].get("model").is_some());
    assert!(create.get("required").is_none() || create["required"] == json!([]));

    let update = &schemas["UpdateChatReq"];
    assert_eq!(update["required"], json!(["title"]));

    assert!(
        schemas.get("Page_ChatDetailDto").is_some(),
        "Page_ChatDetailDto"
    );
    let page_info = &schemas["PageInfo"];
    for property in ["limit", "next_cursor", "prev_cursor"] {
        assert!(
            page_info["properties"].get(property).is_some(),
            "{property}"
        );
    }
}

#[tokio::test]
async fn the_list_operation_declares_the_odata_filter_and_orderby_of_the_reference() {
    let rig = rig(Pdp::TenantOnly).await;
    let value = document(&rig);
    let list = &value["paths"]["/mini-chat/v1/chats"]["get"];

    assert_eq!(
        list["x-odata-filter"],
        json!({
            "allowedFields": {
                "id": ["eq", "ne", "in"],
                "title": ["eq", "ne", "contains", "startswith", "endswith", "in"],
                "updated_at": ["eq", "ne", "gt", "ge", "lt", "le", "in"]
            }
        })
    );
    assert_eq!(
        list["x-odata-orderby"],
        json!({
            "allowedFields": [
                "updated_at asc", "updated_at desc", "id asc", "id desc", "title asc", "title desc"
            ]
        })
    );

    let names: BTreeSet<&str> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["limit", "cursor", "$filter", "$orderby"])
    );
    assert!(
        !names.contains("$select"),
        "$select is accepted but not declared"
    );
}

#[tokio::test]
async fn the_deferred_temporary_chat_route_is_not_registered() {
    let rig = rig(Pdp::TenantOnly).await;
    assert!(
        rig.openapi
            .ops
            .lock()
            .unwrap()
            .iter()
            .all(|op| !op.path.contains(":temporary"))
    );
    let uri = format!("{CHATS}/{}:temporary", Uuid::new_v4());
    let reply = rig.post_json(&uri, &a1(), "{}").await;
    assert!(
        matches!(
            reply.status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ),
        "{} {}",
        reply.status,
        reply.text
    );
    assert!(
        rig.pdp.requests().is_empty(),
        "no authorization for an unregistered route"
    );
}
