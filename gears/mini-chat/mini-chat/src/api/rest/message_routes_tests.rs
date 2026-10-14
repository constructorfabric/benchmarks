//! The `OpenAPI` surface of the message-streaming routes: operation ids, paths, declared statuses
//! (415 is not declared), the SSE response, parameters, `OData` filter and order fields, the
//! license gate and the schemas of the reference subset.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-send-error-contract` (step 3, the declared statuses);
//! flows `list-messages`, `send-message`, `turn-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use toolkit::api::OpenApiInfo;

use crate::test_support::send::send_rig;

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

/// `(method, path, operation id, tag, declared statuses)`.
type Expected = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Vec<u16>,
);

fn expected() -> Vec<Expected> {
    vec![
        (
            "GET",
            "/mini-chat/v1/chats/{id}/messages",
            "mini_chat.list_messages",
            "Mini Chat Messages",
            vec![200, 400, 401, 403, 404, 500, 503],
        ),
        (
            "POST",
            "/mini-chat/v1/chats/{id}/messages:stream",
            "mini_chat.stream_message",
            "Mini Chat Messages",
            vec![200, 400, 401, 403, 404, 409, 422, 429, 500, 503],
        ),
        (
            "GET",
            "/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.get_turn",
            "Mini Chat Turns",
            vec![200, 400, 401, 403, 404, 500, 503],
        ),
    ]
}

#[tokio::test]
async fn the_three_operations_are_registered_once_with_their_ids_and_tags() {
    let rig = send_rig().await;
    for (method, path, operation_id, tag, _) in expected() {
        let count = rig
            .openapi
            .ops
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.method.as_str() == method && op.path == path)
            .count();
        assert_eq!(count, 1, "{method} {path}");
        let op = rig.openapi.op(method, path).unwrap();
        assert_eq!(
            op.operation_id.as_deref(),
            Some(operation_id),
            "{method} {path}"
        );
        assert_eq!(op.tags, vec![tag.to_owned()], "{method} {path}");
    }
}

#[tokio::test]
async fn the_declared_statuses_are_the_documented_ones_and_415_is_not_declared() {
    let rig = send_rig().await;
    for (method, path, _, _, statuses) in expected() {
        let op = rig.openapi.op(method, path).unwrap();
        let declared: BTreeSet<u16> = op.responses.iter().map(|r| r.status).collect();
        assert_eq!(
            declared,
            statuses.iter().copied().collect::<BTreeSet<_>>(),
            "{method} {path}"
        );
        assert!(!declared.contains(&415), "{method} {path}");
        for response in op.responses.iter().filter(|r| r.status >= 400) {
            assert_eq!(
                response.content_type, "application/problem+json",
                "{path} {}",
                response.status
            );
        }
        let unavailable = op.responses.iter().find(|r| r.status == 503).unwrap();
        assert!(
            unavailable
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
            "{path}: Retry-After on 503"
        );
    }
}

#[tokio::test]
async fn the_send_endpoint_declares_an_event_stream_response_and_a_json_request() {
    let rig = send_rig().await;
    let op = rig
        .openapi
        .op("POST", "/mini-chat/v1/chats/{id}/messages:stream")
        .unwrap();
    let ok = op.responses.iter().find(|r| r.status == 200).unwrap();
    assert_eq!(ok.content_type, "text/event-stream");
    assert!(op.request_body.is_some());
    assert!(op.params.iter().any(|p| p.name == "id" && p.required));
}

#[tokio::test]
async fn the_list_declares_its_parameters_and_the_odata_fields() {
    let rig = send_rig().await;
    let document = rig
        .openapi
        .inner
        .build_openapi(&OpenApiInfo::default())
        .unwrap();
    let value = serde_json::to_value(&document).unwrap();
    let list = &value["paths"]["/mini-chat/v1/chats/{id}/messages"]["get"];
    let names: BTreeSet<&str> = list["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["id", "limit", "cursor", "$filter", "$orderby"]),
        "$select is not declared"
    );
    let filter = &list["x-odata-filter"]["allowedFields"];
    assert_eq!(
        filter["created_at"],
        serde_json::json!(["eq", "ne", "gt", "ge", "lt", "le", "in"])
    );
    assert_eq!(filter["id"], serde_json::json!(["eq", "ne", "in"]));
    assert_eq!(
        filter["role"],
        serde_json::json!(["eq", "ne", "contains", "startswith", "endswith", "in"])
    );
    assert_eq!(
        list["x-odata-orderby"]["allowedFields"],
        serde_json::json!([
            "created_at asc",
            "created_at desc",
            "id asc",
            "id desc",
            "role asc",
            "role desc"
        ])
    );
}

#[tokio::test]
async fn the_turn_status_declares_both_path_parameters() {
    let rig = send_rig().await;
    let op = rig
        .openapi
        .op("GET", "/mini-chat/v1/chats/{id}/turns/{request_id}")
        .unwrap();
    for name in ["id", "request_id"] {
        assert!(
            op.params.iter().any(|p| p.name == name && p.required),
            "{name}"
        );
    }
}

#[tokio::test]
async fn every_route_is_authenticated_and_declares_only_the_base_license_feature() {
    let rig = send_rig().await;
    for (method, path, _, _, _) in expected() {
        let op = rig.openapi.op(method, path).unwrap();
        assert!(op.authenticated, "{path}");
        let license = op
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{path}: no license"));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE.to_owned()],
            "{path}"
        );
    }
}

#[tokio::test]
async fn the_schemas_of_the_reference_subset_are_declared() {
    let rig = send_rig().await;
    let document = rig
        .openapi
        .inner
        .build_openapi(&OpenApiInfo::default())
        .unwrap();
    let value = serde_json::to_value(&document).unwrap();
    let schemas = &value["components"]["schemas"];

    let required = |name: &str| -> BTreeSet<String> {
        schemas[name]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} has no required list in {}", schemas[name]))
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let set = |items: &[&str]| {
        items
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        required("MiniChatMessageDto"),
        set(&[
            "id",
            "request_id",
            "role",
            "content",
            "attachments",
            "my_reaction",
            "created_at"
        ])
    );
    assert_eq!(
        required("AttachmentSummaryDto"),
        set(&["attachment_id", "kind", "filename", "status"])
    );
    assert_eq!(
        required("TurnStatusResponse"),
        set(&["request_id", "state", "updated_at"])
    );
    assert_eq!(required("StreamMessageRequest"), set(&["content"]));
    assert!(
        schemas["MiniChatMessageDto"]["properties"]
            .get("model")
            .is_some()
    );
}
