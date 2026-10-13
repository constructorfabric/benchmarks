//! Authorization and tenant/owner isolation of every S2 endpoint (chats,
//! messages, reactions): the per-operation matrix sent to the PDP, the
//! fail-closed table, 404 masking of foreign resources and the owner-only
//! visibility of chat content.
//!
//! Source of truth: DESIGN 2.1 "Tenant-Scoped Everything" / "Owner-Only Chat
//! Content", 3.8 (per-operation authorization matrix, fail-closed behaviour,
//! defence-in-depth owner predicate), the REST error table (403 `AUTHZ_DENIED`,
//! 503 + `Retry-After: 5`, 404 masking, `invalid_path_params`), ADR-0004 and
//! the acceptance criterion "Every operation is scoped to its owner;
//! cross-tenant or foreign access is rejected consistently".

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;

use authz_resolver_sdk::models::EvaluationRequest;
use chat_support::{
    CHAT_PDP_TYPE, ChatHarness, ChatHarnessOptions, ChatSeed, FakePdp, MessageSeed, PdpMode, Resp,
    TENANT_A, TENANT_B, USER_A, USER_A2, USER_B, assert_not_found, assert_problem,
    assert_violation_reason, chat_harness, chat_harness_with, ctx, ctx_a, ctx_a2, ctx_b, ids_of,
    page_ids, page_items, send_body,
};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

struct Endpoint {
    name: &'static str,
    method: &'static str,
    uri: String,
    body: Option<Value>,
    /// PEP action of the authorization matrix (DESIGN 3.8).
    action: &'static str,
    /// `resource.id` is the chat id (absent for create and list).
    chat_scoped: bool,
}

struct World {
    h: ChatHarness,
    chat: Uuid,
    assistant_msg: Uuid,
}

async fn world_with(h: ChatHarness) -> World {
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x700).titled("Private").at(100))
        .await;
    h.seed_message(MessageSeed::user(chat).with_id(0x7001).at(1))
        .await;
    let assistant_msg = h
        .seed_message(MessageSeed::assistant(chat).with_id(0x7002).at(2))
        .await;
    h.seed_reaction(TENANT_A, USER_A, assistant_msg, "like")
        .await;
    World {
        h,
        chat,
        assistant_msg,
    }
}

async fn world() -> World {
    world_with(chat_harness().await).await
}

impl World {
    /// Every S2 endpoint against the world's chat; `DELETE /chats/{id}` is last.
    fn endpoints(&self) -> Vec<Endpoint> {
        let h = &self.h;
        vec![
            Endpoint {
                name: "POST /chats",
                method: "POST",
                uri: h.chats_uri(),
                body: Some(json!({"title": "new"})),
                action: "create",
                chat_scoped: false,
            },
            Endpoint {
                name: "GET /chats",
                method: "GET",
                uri: h.chats_uri(),
                body: None,
                action: "list",
                chat_scoped: false,
            },
            Endpoint {
                name: "GET /chats/{id}",
                method: "GET",
                uri: h.chat_uri(self.chat),
                body: None,
                action: "read",
                chat_scoped: true,
            },
            Endpoint {
                name: "PATCH /chats/{id}",
                method: "PATCH",
                uri: h.chat_uri(self.chat),
                body: Some(json!({"title": "renamed"})),
                action: "update",
                chat_scoped: true,
            },
            Endpoint {
                name: "GET /chats/{id}/messages",
                method: "GET",
                uri: h.messages_uri(self.chat),
                body: None,
                action: "list_messages",
                chat_scoped: true,
            },
            Endpoint {
                name: "PUT reaction",
                method: "PUT",
                uri: h.reaction_uri(self.chat, self.assistant_msg),
                body: Some(json!({"reaction": "dislike"})),
                action: "set_reaction",
                chat_scoped: true,
            },
            Endpoint {
                name: "DELETE reaction",
                method: "DELETE",
                uri: h.reaction_uri(self.chat, self.assistant_msg),
                body: None,
                action: "delete_reaction",
                chat_scoped: true,
            },
            Endpoint {
                name: "DELETE /chats/{id}",
                method: "DELETE",
                uri: h.chat_uri(self.chat),
                body: None,
                action: "delete",
                chat_scoped: true,
            },
        ]
    }

    /// The endpoints that address the world's chat (everything but create / list).
    fn chat_endpoints(&self) -> Vec<Endpoint> {
        self.endpoints()
            .into_iter()
            .filter(|e| e.chat_scoped)
            .collect()
    }

    async fn call(&self, endpoint: &Endpoint, ctx: &SecurityContext) -> Resp {
        match &endpoint.body {
            Some(body) => {
                send_body(
                    &self.h.router,
                    endpoint.method,
                    &endpoint.uri,
                    Some(ctx),
                    Some("application/json"),
                    &body.to_string(),
                )
                .await
            }
            None => {
                send_body(
                    &self.h.router,
                    endpoint.method,
                    &endpoint.uri,
                    Some(ctx),
                    None,
                    "",
                )
                .await
            }
        }
    }
}

fn since(requests: &[EvaluationRequest], before: usize) -> Vec<EvaluationRequest> {
    requests[before..].to_vec()
}

// ═════════════════════════════════════════════════════════════════════════════
// per-operation authorization matrix and fail-closed behaviour (3.8)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn every_endpoint_asks_the_pdp_for_its_matrix_action_on_the_chat_resource() {
    let w = world().await;
    for endpoint in w.endpoints() {
        let before = w.h.pdp.recorded().len();
        let resp = w.call(&endpoint, &ctx_a()).await;
        assert!(
            resp.status.is_success(),
            "{}: status {} body {}",
            endpoint.name,
            resp.status,
            resp.raw
        );
        let sent = since(&w.h.pdp.recorded(), before);
        assert!(
            !sent.is_empty(),
            "{}: the PDP was not consulted",
            endpoint.name
        );
        for req in sent {
            assert_eq!(req.action.name, endpoint.action, "{}", endpoint.name);
            assert_eq!(
                req.resource.resource_type, CHAT_PDP_TYPE,
                "{}: reactions and messages are actions on the Chat resource",
                endpoint.name
            );
            assert!(req.context.require_constraints, "{}", endpoint.name);
            if endpoint.chat_scoped {
                assert_eq!(
                    req.resource.id,
                    Some(w.chat),
                    "{}: resource.id",
                    endpoint.name
                );
            } else {
                assert!(
                    req.resource.id.is_none(),
                    "{}: no resource.id",
                    endpoint.name
                );
            }
        }
    }
}

#[tokio::test]
async fn a_pdp_denial_is_403_authz_denied_on_every_endpoint_and_nothing_changes() {
    let w = world_with(
        chat_harness_with(ChatHarnessOptions {
            pdp: FakePdp::deny("internal-policy-rule-42"),
            ..Default::default()
        })
        .await,
    )
    .await;

    for endpoint in w.endpoints() {
        let resp = w.call(&endpoint, &ctx_a()).await;
        let problem = assert_problem(&resp, 403, "permission_denied");
        assert_eq!(
            problem["context"]["reason"],
            json!("AUTHZ_DENIED"),
            "{}",
            endpoint.name
        );
        assert!(
            !resp.raw.contains("internal-policy-rule-42"),
            "{}: deny_reason.details is not exposed",
            endpoint.name
        );
    }

    let chats = w.h.all_chats().await;
    assert_eq!(chats.len(), 1, "nothing was created");
    assert_eq!(chats[0].title.as_deref(), Some("Private"));
    assert!(chats[0].deleted_at.is_none(), "nothing was deleted");
    let reactions = w.h.all_reactions().await;
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].reaction, "like", "nothing was changed");
    w.h.cleanup.assert_none().await;
}

#[tokio::test]
async fn a_denied_request_for_an_unknown_chat_is_403_not_404() {
    let h = chat_harness_with(ChatHarnessOptions {
        pdp: FakePdp::deny("no grant"),
        ..Default::default()
    })
    .await;
    let resp = h.get(&h.chat_uri(Uuid::new_v4()), &ctx_a()).await;
    assert_problem(&resp, 403, "permission_denied");
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_with_retry_after_5_on_every_endpoint() {
    let w = world_with(
        chat_harness_with(ChatHarnessOptions {
            pdp: FakePdp::unreachable("pdp-transport-secret-detail"),
            ..Default::default()
        })
        .await,
    )
    .await;

    for endpoint in w.endpoints() {
        let resp = w.call(&endpoint, &ctx_a()).await;
        let problem = assert_problem(&resp, 503, "service_unavailable");
        assert_eq!(
            resp.headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("5"),
            "{}: Retry-After: 5",
            endpoint.name
        );
        assert_eq!(
            problem["context"]["retry_after_seconds"],
            json!(5),
            "{}",
            endpoint.name
        );
        assert!(
            !resp.raw.contains("pdp-transport-secret-detail"),
            "{}: the cause is only logged",
            endpoint.name
        );
    }
    assert_eq!(w.h.all_chats().await.len(), 1);
    assert!(w.h.all_chats().await[0].deleted_at.is_none());
}

#[tokio::test]
async fn a_permit_without_constraints_is_403_because_chat_operations_need_them() {
    let w = world_with(
        chat_harness_with(ChatHarnessOptions {
            pdp: FakePdp::new(PdpMode::AllowWithoutConstraints),
            ..Default::default()
        })
        .await,
    )
    .await;
    for endpoint in w.endpoints() {
        let resp = w.call(&endpoint, &ctx_a()).await;
        assert_problem(&resp, 403, "permission_denied");
    }
    assert_eq!(
        w.h.all_chats().await.len(),
        1,
        "fail closed: nothing created"
    );
}

#[tokio::test]
async fn non_uuid_path_parameters_are_400_invalid_path_params_before_authorization() {
    let w = world().await;
    let prefix = &w.h.prefix;
    let cases = [
        ("GET", format!("{prefix}/v1/chats/not-a-uuid"), None),
        (
            "PATCH",
            format!("{prefix}/v1/chats/not-a-uuid"),
            Some(json!({"title": "x"})),
        ),
        ("DELETE", format!("{prefix}/v1/chats/not-a-uuid"), None),
        ("GET", format!("{prefix}/v1/chats/12345/messages"), None),
        (
            "PUT",
            format!("{prefix}/v1/chats/{}/messages/xyz/reaction", w.chat),
            Some(json!({"reaction": "like"})),
        ),
        (
            "DELETE",
            format!("{prefix}/v1/chats/{}/messages/xyz/reaction", w.chat),
            None,
        ),
    ];
    for (method, uri, body) in cases {
        let resp = send_body(
            &w.h.router,
            method,
            &uri,
            Some(&ctx_a()),
            body.as_ref().map(|_| "application/json"),
            &body.map(|b| b.to_string()).unwrap_or_default(),
        )
        .await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_violation_reason(&problem, "invalid_path_params");
    }
    assert!(
        w.h.pdp.recorded().is_empty(),
        "rejected before the PDP is asked"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// owner and tenant isolation
// ═════════════════════════════════════════════════════════════════════════════

async fn assert_foreign_caller_is_masked(w: &World, caller: &SecurityContext, who: &str) {
    for endpoint in w.chat_endpoints() {
        let resp = w.call(&endpoint, caller).await;
        assert_not_found(&resp, "chat");
        assert!(
            !resp.raw.contains("Private"),
            "{who} / {}: no content of the chat leaks: {}",
            endpoint.name,
            resp.raw
        );
    }

    // Nothing of the owner's data changed.
    let row = w.h.chat_row(w.chat).await.expect("chat row");
    assert_eq!(row.title.as_deref(), Some("Private"), "{who}: not renamed");
    assert!(row.deleted_at.is_none(), "{who}: not deleted");
    assert_eq!(
        chat_support::from_db(row.updated_at),
        chat_support::ts(100),
        "{who}: not touched"
    );
    let reactions = w.h.reactions_of(w.assistant_msg).await;
    assert_eq!(reactions.len(), 1, "{who}: reaction untouched");
    assert_eq!(
        (reactions[0].user_id, reactions[0].reaction.as_str()),
        (USER_A, "like")
    );
    w.h.cleanup.assert_none().await;

    // The owner still sees everything.
    assert_eq!(
        w.h.get(&w.h.chat_uri(w.chat), &ctx_a())
            .await
            .status
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn a_user_of_another_tenant_gets_404_on_every_chat_endpoint() {
    let w = world().await;
    assert_foreign_caller_is_masked(&w, &ctx_b(), "tenant B user").await;
}

#[tokio::test]
async fn a_second_user_of_the_same_tenant_gets_404_on_every_chat_endpoint() {
    let w = world().await;
    assert_foreign_caller_is_masked(&w, &ctx_a2(), "tenant A second user").await;
}

#[tokio::test]
async fn the_same_user_id_in_another_tenant_is_a_different_principal() {
    let w = world().await;
    assert_foreign_caller_is_masked(&w, &ctx(USER_A, TENANT_B), "user A id in tenant B").await;
}

#[tokio::test]
async fn foreign_and_unknown_chats_are_indistinguishable() {
    let w = world().await;
    let unknown = Uuid::new_v4();
    let h = &w.h;
    let pairs: Vec<(&str, Resp, Resp)> = vec![
        (
            "GET",
            h.get(&h.chat_uri(w.chat), &ctx_b()).await,
            h.get(&h.chat_uri(unknown), &ctx_b()).await,
        ),
        (
            "PATCH",
            h.patch_json(&h.chat_uri(w.chat), &ctx_a2(), &json!({"title": "x"}))
                .await,
            h.patch_json(&h.chat_uri(unknown), &ctx_a2(), &json!({"title": "x"}))
                .await,
        ),
        (
            "DELETE",
            h.delete(&h.chat_uri(w.chat), &ctx_b()).await,
            h.delete(&h.chat_uri(unknown), &ctx_b()).await,
        ),
        (
            "messages",
            h.get(&h.messages_uri(w.chat), &ctx_a2()).await,
            h.get(&h.messages_uri(unknown), &ctx_a2()).await,
        ),
        (
            "reaction",
            h.put_json(
                &h.reaction_uri(w.chat, w.assistant_msg),
                &ctx_b(),
                &json!({"reaction": "like"}),
            )
            .await,
            h.put_json(
                &h.reaction_uri(unknown, w.assistant_msg),
                &ctx_b(),
                &json!({"reaction": "like"}),
            )
            .await,
        ),
    ];
    for (name, foreign, missing) in pairs {
        let f = assert_not_found(&foreign, "chat");
        let m = assert_not_found(&missing, "chat");
        for key in ["type", "title", "status", "detail"] {
            assert_eq!(
                f[key], m[key],
                "{name}: `{key}` must not tell the cases apart"
            );
        }
        assert_eq!(
            f["context"]["resource_type"], m["context"]["resource_type"],
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_foreign_chat_is_masked_as_chat_not_found_even_with_a_valid_message_id() {
    let w = world().await;
    let resp =
        w.h.put_json(
            &w.h.reaction_uri(w.chat, w.assistant_msg),
            &ctx_b(),
            &json!({"reaction": "like"}),
        )
        .await;
    assert_not_found(&resp, "chat");
    assert_eq!(w.h.reactions_of(w.assistant_msg).await.len(), 1);
}

#[tokio::test]
async fn list_shows_only_the_callers_own_live_chats() {
    let h = chat_harness().await;
    let a1 = h
        .seed_chat(ChatSeed::of_a().with_id(1).titled("a1").at(10))
        .await;
    let a2 = h
        .seed_chat(ChatSeed::of_a().with_id(2).titled("a2").at(20))
        .await;
    h.seed_chat(
        ChatSeed::of_a()
            .with_id(3)
            .titled("a-gone")
            .at(30)
            .deleted(),
    )
    .await;
    let r1 = h
        .seed_chat(
            ChatSeed::new(TENANT_A, USER_A2)
                .with_id(4)
                .titled("r1")
                .at(40),
        )
        .await;
    let b1 = h
        .seed_chat(
            ChatSeed::new(TENANT_B, USER_B)
                .with_id(5)
                .titled("b1")
                .at(50),
        )
        .await;
    // The same user id in another tenant is a different principal.
    let a_in_b = h
        .seed_chat(
            ChatSeed::new(TENANT_B, USER_A)
                .with_id(6)
                .titled("a-in-b")
                .at(60),
        )
        .await;

    let a = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_ids(&a), ids_of(&[a2, a1]));
    let reviewer = h.list_chats("", &ctx_a2()).await.json();
    assert_eq!(page_ids(&reviewer), ids_of(&[r1]));
    let b = h.list_chats("", &ctx_b()).await.json();
    assert_eq!(page_ids(&b), ids_of(&[b1]));
    let a_other_tenant = h.list_chats("", &ctx(USER_A, TENANT_B)).await.json();
    assert_eq!(page_ids(&a_other_tenant), ids_of(&[a_in_b]));

    for (page, forbidden) in [(&a, "b1"), (&reviewer, "a1"), (&b, "a1")] {
        assert!(
            !page.to_string().contains(forbidden),
            "foreign titles never appear: {page}"
        );
    }
}

#[tokio::test]
async fn a_chat_created_by_another_tenant_is_invisible_and_owned_by_its_creator() {
    let h = chat_harness().await;
    let created = h.create_chat(&ctx_b(), &json!({"title": "B only"})).await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let row = h.chat_row(id).await.unwrap();
    assert_eq!((row.tenant_id, row.user_id), (TENANT_B, USER_B));
    assert_not_found(&h.get(&h.chat_uri(id), &ctx_a()).await, "chat");
    assert_not_found(&h.get(&h.chat_uri(id), &ctx_a2()).await, "chat");
    assert!(page_items(&h.list_chats("", &ctx_a()).await.json()).is_empty());
    assert_eq!(h.get(&h.chat_uri(id), &ctx_b()).await.status.as_u16(), 200);
}

#[tokio::test]
async fn a_foreign_message_id_cannot_be_reached_through_the_callers_own_chat() {
    let h = chat_harness().await;
    let mine = h.seed_chat(ChatSeed::of_a().with_id(1)).await;
    let theirs = h
        .seed_chat(ChatSeed::new(TENANT_B, USER_B).with_id(2))
        .await;
    let their_msg = h
        .seed_message(
            MessageSeed::assistant(theirs)
                .with_id(0x2001)
                .tenant(TENANT_B)
                .at(1),
        )
        .await;

    // Reaction through the caller's own chat id with a message of another tenant.
    let resp = h
        .put_json(
            &h.reaction_uri(mine, their_msg),
            &ctx_a(),
            &json!({"reaction": "like"}),
        )
        .await;
    assert_not_found(&resp, "message");
    let resp = h.delete(&h.reaction_uri(mine, their_msg), &ctx_a()).await;
    assert_not_found(&resp, "message");
    assert!(h.reactions_of(their_msg).await.is_empty());

    // And the message list of the caller's chat does not contain it.
    let page = h.list_messages(mine, "", &ctx_a()).await.json();
    assert!(page_items(&page).is_empty());
}
