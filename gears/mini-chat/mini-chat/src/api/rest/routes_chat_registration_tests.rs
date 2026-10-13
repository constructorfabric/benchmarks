#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Cross-cutting tests of `cpt-cf-mini-chat-feature-chat-crud`: the REST
//! registration (`cpt-cf-mini-chat-dod-chat-crud-rest-routes`: eight routes,
//! authentication, license gate, `OpenAPI` metadata) and the authorization
//! contract of every operation (`cpt-cf-mini-chat-dod-chat-crud-quality-gates`:
//! PEP action and `resource.id`, deny, PDP failure, owner and tenant isolation).

use std::collections::{BTreeMap, BTreeSet};

use axum::http::StatusCode;
use serde_json::json;
use toolkit::api::operation_builder::{OperationSpec, ResponseSpec};
use uuid::Uuid;

use super::BASE_LICENSE_FEATURE;
use crate::chat_test_env::{
    CHAT_PEP_TYPE, CHAT_TYPE, ChatEnv, Options, OutboxMode, Reply, chat_uri, reaction_uri,
};
use crate::test_doubles::{
    PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx, user_ctx,
};

// ================================================================================================
// Registration
// ================================================================================================

const CREATE: &str = "POST:/mini-chat/v1/chats";
const LIST: &str = "GET:/mini-chat/v1/chats";
const GET: &str = "GET:/mini-chat/v1/chats/{id}";
const PATCH: &str = "PATCH:/mini-chat/v1/chats/{id}";
const DELETE: &str = "DELETE:/mini-chat/v1/chats/{id}";
const MESSAGES: &str = "GET:/mini-chat/v1/chats/{id}/messages";
const PUT_REACTION: &str = "PUT:/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction";
const DELETE_REACTION: &str = "DELETE:/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction";

const ALL: [&str; 8] = [
    CREATE,
    LIST,
    GET,
    PATCH,
    DELETE,
    MESSAGES,
    PUT_REACTION,
    DELETE_REACTION,
];

fn spec(env: &ChatEnv, key: &str) -> OperationSpec {
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

fn param_names(spec: &OperationSpec) -> BTreeSet<String> {
    spec.params.iter().map(|p| p.name.clone()).collect()
}

#[tokio::test]
async fn exactly_the_eight_chat_operations_are_registered_under_the_url_prefix() {
    let env = ChatEnv::new().await;

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
    let env = ChatEnv::with(Options {
        prefix: "/chat-api/",
        ..Options::default()
    })
    .await;

    let mut registered: Vec<String> = env
        .openapi
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
    let env = ChatEnv::new().await;

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
async fn every_operation_has_a_unique_id_a_summary_and_a_tag() {
    let env = ChatEnv::new().await;
    let mut operation_ids = BTreeSet::new();

    for key in ALL {
        let spec = spec(&env, key);

        let operation_id = spec.operation_id.clone().unwrap_or_default();
        assert!(!operation_id.is_empty(), "{key}");
        assert!(operation_ids.insert(operation_id), "{key}: duplicate id");
        assert!(
            spec.summary.as_deref().is_some_and(|s| !s.is_empty()),
            "{key}"
        );
        assert!(!spec.tags.is_empty(), "{key}");
    }
}

#[tokio::test]
async fn the_success_responses_and_their_schemas_match_the_contract() {
    let env = ChatEnv::new().await;

    assert_eq!(schema_name(&spec(&env, CREATE), 201), "ChatDetailDto");
    assert_eq!(schema_name(&spec(&env, LIST), 200), "Page_ChatDetailDto");
    assert_eq!(schema_name(&spec(&env, GET), 200), "ChatDetailDto");
    assert_eq!(schema_name(&spec(&env, PATCH), 200), "ChatDetailDto");
    assert_eq!(
        schema_name(&spec(&env, MESSAGES), 200),
        "Page_MiniChatMessageDto"
    );
    assert_eq!(
        schema_name(&spec(&env, PUT_REACTION), 200),
        "MiniChatReactionDto"
    );
    for key in [DELETE, DELETE_REACTION] {
        let spec = spec(&env, key);
        assert!(statuses(&spec).contains(&204), "{key}");
        assert!(response(&spec, 204).schema.is_none(), "{key}: no body");
    }
}

#[tokio::test]
async fn the_201_response_declares_the_location_header() {
    let env = ChatEnv::new().await;

    let created = response(&spec(&env, CREATE), 201);

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
async fn the_request_bodies_use_the_documented_schemas() {
    let env = ChatEnv::new().await;

    for (key, expected) in [
        (CREATE, "CreateChatReq"),
        (PATCH, "UpdateChatReq"),
        (PUT_REACTION, "SetReactionReq"),
    ] {
        let spec = spec(&env, key);
        let body = spec
            .request_body
            .unwrap_or_else(|| panic!("{key} declares no request body"));

        assert_eq!(body.content_type, "application/json", "{key}");
        assert!(body.required, "{key}");
        assert!(
            matches!(
                &body.schema,
                toolkit::api::operation_builder::RequestBodySchema::Ref { schema_name } if schema_name == expected
            ),
            "{key}: {:?}",
            body.schema
        );
    }
    for key in [LIST, GET, DELETE, MESSAGES, DELETE_REACTION] {
        assert!(spec(&env, key).request_body.is_none(), "{key}");
    }
}

#[tokio::test]
async fn every_route_documents_the_problem_responses_it_can_return() {
    let env = ChatEnv::new().await;
    let with_path_id = [GET, PATCH, DELETE, MESSAGES, PUT_REACTION, DELETE_REACTION];
    let with_json_body = [CREATE, PATCH, PUT_REACTION];

    for key in ALL {
        let statuses = statuses(&spec(&env, key));

        for status in [400, 401, 403, 500, 503] {
            assert!(
                statuses.contains(&status),
                "{key} lacks {status}: {statuses:?}"
            );
        }
        assert!(!statuses.contains(&415), "{key}: 415 is not declared");
        assert_eq!(
            statuses.contains(&404),
            with_path_id.contains(&key),
            "{key}: {statuses:?}"
        );
        assert_eq!(
            statuses.contains(&422),
            with_json_body.contains(&key),
            "{key}: {statuses:?}"
        );
    }
}

#[tokio::test]
async fn the_503_response_declares_the_retry_after_header() {
    let env = ChatEnv::new().await;

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
async fn the_path_parameters_are_documented() {
    let env = ChatEnv::new().await;

    for key in [GET, PATCH, DELETE, MESSAGES, PUT_REACTION, DELETE_REACTION] {
        assert!(param_names(&spec(&env, key)).contains("id"), "{key}");
    }
    for key in [PUT_REACTION, DELETE_REACTION] {
        assert!(param_names(&spec(&env, key)).contains("msg_id"), "{key}");
    }
}

#[tokio::test]
async fn the_list_routes_declare_limit_cursor_filter_and_orderby_but_not_select() {
    let env = ChatEnv::new().await;

    for key in [LIST, MESSAGES] {
        let names = param_names(&spec(&env, key));

        for expected in ["limit", "cursor", "$filter", "$orderby"] {
            assert!(
                names.contains(expected),
                "{key} lacks {expected}: {names:?}"
            );
        }
        assert!(!names.contains("$select"), "{key}: $select is not declared");
    }
}

#[tokio::test]
async fn the_list_routes_allow_filter_and_orderby_only_over_their_fields() {
    let env = ChatEnv::new().await;

    for (key, fields) in [
        (LIST, ["id", "title", "updated_at"]),
        (MESSAGES, ["created_at", "id", "role"]),
    ] {
        let spec = spec(&env, key);

        let filter = spec
            .vendor_extensions
            .x_odata_filter
            .as_ref()
            .unwrap_or_else(|| panic!("{key} declares no filter fields"));
        let filterable: BTreeSet<&str> = filter.allowed_fields.keys().map(String::as_str).collect();
        assert_eq!(
            filterable,
            fields.iter().copied().collect::<BTreeSet<_>>(),
            "{key}"
        );
        let orderby = spec
            .vendor_extensions
            .x_odata_orderby
            .as_ref()
            .unwrap_or_else(|| panic!("{key} declares no orderby fields"));
        let orderable: BTreeSet<String> = orderby.allowed_fields.iter().cloned().collect();
        let expected: BTreeSet<String> = fields
            .iter()
            .flat_map(|f| [format!("{f} asc"), format!("{f} desc")])
            .collect();
        assert_eq!(orderable, expected, "{key}");
    }
}

#[tokio::test]
async fn no_temporary_chat_route_is_registered() {
    let env = ChatEnv::new().await;
    assert!(
        !env.openapi.operation_specs.is_empty(),
        "the routes are registered"
    );

    for entry in &env.openapi.operation_specs {
        assert!(!entry.key().contains("temporary"), "{}", entry.key());
    }
}

// ================================================================================================
// Authorization contract of every operation
// ================================================================================================

#[derive(Clone, Copy)]
struct Op {
    name: &'static str,
    action: &'static str,
    has_chat_id: bool,
}

const OPS: [Op; 8] = [
    Op {
        name: "create",
        action: "create",
        has_chat_id: false,
    },
    Op {
        name: "list",
        action: "list",
        has_chat_id: false,
    },
    Op {
        name: "get",
        action: "read",
        has_chat_id: true,
    },
    Op {
        name: "rename",
        action: "update",
        has_chat_id: true,
    },
    Op {
        name: "delete",
        action: "delete",
        has_chat_id: true,
    },
    Op {
        name: "messages",
        action: "list_messages",
        has_chat_id: true,
    },
    Op {
        name: "set_reaction",
        action: "set_reaction",
        has_chat_id: true,
    },
    Op {
        name: "delete_reaction",
        action: "delete_reaction",
        has_chat_id: true,
    },
];

struct World {
    env: ChatEnv,
    chat: Uuid,
    message: Uuid,
}

async fn world(pdp: PdpReply) -> World {
    let env = ChatEnv::with(Options {
        pdp,
        outbox: OutboxMode::Running,
        ..Options::default()
    })
    .await;
    let chat = env.seed_chat(1, Some("Subject"), 10).await;
    env.seed_message(chat, 1, "user", 1).await;
    let message = env.seed_message(chat, 2, "assistant", 2).await;
    World { env, chat, message }
}

/// Runs `op` against `chat` / `message` as `ctx` with valid input.
async fn run(
    env: &ChatEnv,
    op: Op,
    chat: Uuid,
    message: Uuid,
    ctx: &toolkit_security::SecurityContext,
) -> Reply {
    match op.name {
        "create" => {
            env.send_json("POST", "/mini-chat/v1/chats", ctx, &json!({ "title": "t" }))
                .await
        }
        "list" => env.get_as(ctx, "/mini-chat/v1/chats").await,
        "get" => env.get_as(ctx, &chat_uri(chat)).await,
        "rename" => {
            env.send_json(
                "PATCH",
                &chat_uri(chat),
                ctx,
                &json!({ "title": "renamed" }),
            )
            .await
        }
        "delete" => {
            env.send_raw("DELETE", &chat_uri(chat), ctx, None, None)
                .await
        }
        "messages" => {
            env.get_as(ctx, &format!("{}/messages", chat_uri(chat)))
                .await
        }
        "set_reaction" => {
            env.send_json(
                "PUT",
                &reaction_uri(chat, message),
                ctx,
                &json!({ "reaction": "like" }),
            )
            .await
        }
        "delete_reaction" => {
            env.send_raw("DELETE", &reaction_uri(chat, message), ctx, None, None)
                .await
        }
        other => panic!("unknown op {other}"),
    }
}

async fn everything(env: &ChatEnv) -> Vec<String> {
    let mut dumps = Vec::new();
    for table in [
        "chats",
        "messages",
        "attachments",
        "message_attachments",
        "message_reactions",
        "chat_turns",
    ] {
        dumps.push(env.dump(table).await);
    }
    dumps.push(env.outbox_message_count().await.to_string());
    dumps
}

#[tokio::test]
async fn every_operation_asks_the_pdp_once_for_its_design_action_with_the_chat_id() {
    for op in OPS {
        let world = world(PdpReply::AllowScoped).await;

        run(&world.env, op, world.chat, world.message, &user_ctx()).await;

        let request = world.env.only_pdp_request();
        assert_eq!(request.action.name, op.action, "{}", op.name);
        assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE, "{}", op.name);
        assert_eq!(
            request.resource.id,
            op.has_chat_id.then_some(world.chat),
            "resource.id of {}",
            op.name
        );
        assert!(request.context.require_constraints, "{}", op.name);
        assert_eq!(request.subject.id, USER);
        assert_eq!(
            request.subject.properties.get("tenant_id"),
            Some(&json!(USER_TENANT.to_string()))
        );
    }
}

#[tokio::test]
async fn a_pdp_deny_is_403_authz_denied_on_every_operation_and_changes_nothing() {
    for op in OPS {
        let world = world(PdpReply::Deny).await;
        let before = everything(&world.env).await;

        let reply = run(&world.env, op, world.chat, world.message, &user_ctx()).await;

        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{}: {}",
            op.name,
            reply.text
        );
        assert_eq!(
            reply.json["context"]["reason"], "AUTHZ_DENIED",
            "{}",
            op.name
        );
        assert!(!reply.text.contains("deny_reason"), "{}", reply.text);
        assert_eq!(
            everything(&world.env).await,
            before,
            "{} changed state",
            op.name
        );
        assert!(
            world
                .env
                .plugin
                .snapshot_requests
                .lock()
                .unwrap()
                .is_empty(),
            "{}: no catalog read after a deny",
            op.name
        );
    }
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_five_on_every_operation_and_changes_nothing() {
    for op in OPS {
        let world = world(PdpReply::Failure).await;
        let before = everything(&world.env).await;

        let reply = run(&world.env, op, world.chat, world.message, &user_ctx()).await;

        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}: {}",
            op.name,
            reply.text
        );
        assert_eq!(reply.header("retry-after"), Some("5"), "{}", op.name);
        assert_eq!(
            reply.json["context"]["retry_after_seconds"], 5,
            "{}",
            op.name
        );
        assert_eq!(
            everything(&world.env).await,
            before,
            "{} changed state",
            op.name
        );
    }
}

#[tokio::test]
async fn a_chat_of_another_user_or_tenant_is_404_on_every_route_with_a_chat_id() {
    // The PDP returns only a tenant predicate, or even two tenants: the owner
    // and tenant predicates of the gear keep foreign chats invisible.
    for pdp in [PdpReply::AllowScoped, PdpReply::AllowTwoTenants] {
        for ctx in [other_user_ctx(), other_tenant_ctx()] {
            for op in OPS.iter().filter(|op| op.has_chat_id) {
                let world = world(pdp).await;
                let before = everything(&world.env).await;

                let reply = run(&world.env, *op, world.chat, world.message, &ctx).await;

                reply.assert_not_found(CHAT_TYPE);
                assert_eq!(
                    everything(&world.env).await,
                    before,
                    "{} changed state",
                    op.name
                );
            }
        }
    }
}

#[tokio::test]
async fn the_owner_is_served_on_every_route_with_the_same_world() {
    // Guards the isolation test above against a fixture that always answers 404.
    for op in OPS {
        let world = world(PdpReply::AllowScoped).await;

        let reply = run(&world.env, op, world.chat, world.message, &user_ctx()).await;

        assert!(
            reply.status.is_success(),
            "{} must succeed for the owner: {} {}",
            op.name,
            reply.status,
            reply.text
        );
    }
}

#[tokio::test]
async fn responses_never_contain_user_tenant_or_provider_identifiers() {
    let mut seen = BTreeMap::new();

    for op in OPS {
        let world = world(PdpReply::AllowScoped).await;
        let reply = run(&world.env, op, world.chat, world.message, &user_ctx()).await;
        assert!(reply.status.is_success(), "{}: {}", op.name, reply.text);
        seen.insert(op.name, reply.text);
    }

    for (name, text) in seen {
        for forbidden in ["user_id", "tenant_id", "deleted_at", "provider_response_id"] {
            assert!(
                !text.contains(forbidden),
                "{name} leaked `{forbidden}`: {text}"
            );
        }
    }
}
