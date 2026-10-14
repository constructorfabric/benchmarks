//! Owner and tenant isolation of the five chat operations: two tenants, two users in one tenant
//! and one user id present in both tenants, against a PDP that returns only a tenant predicate
//! and one that returns tenant and owner predicates.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithm
//! `cpt-cf-mini-chat-algo-chat-management-chat-isolation`; flows `get-chat`, `list-chats`,
//! `update-chat-title`, `delete-chat`, `create-chat`; definition of done
//! `cpt-cf-mini-chat-dod-chat-management-isolation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{
    CHATS, ChatRig, a1, a2, assert_chat_not_found, assert_problem, at, attachment_rows, b1, caller,
    chat_row, chat_uri, item_ids, rig, seed_attachment,
};
use crate::test_support::{Pdp, TENANT_A, TENANT_B, USER_A1, USER_A2, USER_B1};

/// A chat for each of the three callers, each with one attachment.
struct World {
    rig: ChatRig,
    a1_chat: ChatRecord,
    a2_chat: ChatRecord,
    b1_chat: ChatRecord,
}

async fn world(behavior: Pdp) -> World {
    let rig = rig(behavior).await;
    let a1_chat = rig.seed(&a1(), Some("a1 chat"), "gpt-standard").await;
    let a2_chat = rig.seed(&a2(), Some("a2 chat"), "gpt-standard").await;
    let b1_chat = rig.seed(&b1(), Some("b1 chat"), "gpt-standard").await;
    for chat in [&a1_chat, &a2_chat, &b1_chat] {
        seed_attachment(&rig.db, chat, "document", None).await;
    }
    World {
        rig,
        a1_chat,
        a2_chat,
        b1_chat,
    }
}

const PDPS: [Pdp; 2] = [Pdp::TenantOnly, Pdp::TenantAndOwner];

fn blank(mut body: Value) -> Value {
    let object = body.as_object_mut().unwrap();
    object.remove("instance");
    body["context"]
        .as_object_mut()
        .unwrap()
        .remove("resource_name");
    body
}

// ── list ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn each_user_lists_only_their_own_chats() {
    for behavior in PDPS {
        let w = world(behavior).await;
        for (who, own) in [
            (a1(), w.a1_chat.id),
            (a2(), w.a2_chat.id),
            (b1(), w.b1_chat.id),
        ] {
            let reply = w.rig.get(CHATS, &who).await;
            assert_eq!(reply.status, StatusCode::OK, "{behavior:?}: {}", reply.text);
            assert_eq!(item_ids(&reply.json()), vec![own], "{behavior:?}");
        }
    }
}

#[tokio::test]
async fn the_same_user_id_in_another_tenant_sees_nothing_of_the_first_tenant() {
    for behavior in PDPS {
        let w = world(behavior).await;
        let twin = caller(USER_A1, TENANT_B);
        assert!(
            item_ids(&w.rig.get(CHATS, &twin).await.json()).is_empty(),
            "{behavior:?}"
        );
        assert_chat_not_found(&w.rig.get(&chat_uri(w.a1_chat.id), &twin).await);
        assert_chat_not_found(&w.rig.delete(&chat_uri(w.a1_chat.id), &twin).await);
        assert_chat_not_found(
            &w.rig
                .patch_json(&chat_uri(w.a1_chat.id), &twin, r#"{"title":"x"}"#)
                .await,
        );
        assert_eq!(
            chat_row(&w.rig.db, w.a1_chat.id).await.unwrap().deleted_at,
            None
        );
    }
}

#[tokio::test]
async fn no_filter_or_cursor_can_reach_a_foreign_chat() {
    for behavior in PDPS {
        let w = world(behavior).await;
        // a filter on a foreign id finds nothing
        for foreign in [w.a2_chat.id, w.b1_chat.id] {
            let eq = format!("$filter=id%20eq%20{foreign}");
            let reply = w.rig.get(&format!("{CHATS}?{eq}"), &a1()).await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
            assert!(item_ids(&reply.json()).is_empty(), "{behavior:?}");
        }
        // `in` over an own and a foreign id returns only the own chat
        let mixed = format!("$filter=id%20in%20({}%2C%20{})", w.a1_chat.id, w.a2_chat.id);
        let reply = w.rig.get(&format!("{CHATS}?{mixed}"), &a1()).await;
        assert_eq!(item_ids(&reply.json()), vec![w.a1_chat.id], "{behavior:?}");
        // a title filter matching a foreign chat finds nothing
        let by_title = "$filter=title%20eq%20%27a2%20chat%27";
        let reply = w.rig.get(&format!("{CHATS}?{by_title}"), &a1()).await;
        assert!(item_ids(&reply.json()).is_empty(), "{behavior:?}");
    }
}

#[tokio::test]
async fn a_cursor_issued_to_one_user_does_not_leak_another_users_chats() {
    let rig = rig(Pdp::TenantOnly).await;
    for i in 0..4 {
        rig.seed_at(&a1(), Uuid::new_v4(), Some("a1"), "m", at(i))
            .await;
        rig.seed_at(&a2(), Uuid::new_v4(), Some("a2"), "m", at(i))
            .await;
    }
    let first = rig.get(&format!("{CHATS}?limit=1"), &a1()).await.json();
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = rig
        .get(&format!("{CHATS}?limit=100&cursor={cursor}"), &a2())
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let own: BTreeSet<String> = ["a2"].into_iter().map(ToOwned::to_owned).collect();
    let titles: BTreeSet<String> = reply.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        titles.is_subset(&own),
        "only the caller's own chats: {titles:?}"
    );
}

// ── get, update, delete ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_user_reads_only_their_own_chat_and_a_foreign_chat_is_a_404_never_a_403() {
    for behavior in PDPS {
        let w = world(behavior).await;
        for (who, own, foreign) in [
            (a1(), w.a1_chat.id, [w.a2_chat.id, w.b1_chat.id]),
            (a2(), w.a2_chat.id, [w.a1_chat.id, w.b1_chat.id]),
            (b1(), w.b1_chat.id, [w.a1_chat.id, w.a2_chat.id]),
        ] {
            let reply = w.rig.get(&chat_uri(own), &who).await;
            assert_eq!(reply.status, StatusCode::OK, "{behavior:?}: {}", reply.text);
            for id in foreign {
                let reply = w.rig.get(&chat_uri(id), &who).await;
                assert_eq!(reply.status, StatusCode::NOT_FOUND, "{behavior:?}");
                assert_chat_not_found(&reply);
            }
        }
    }
}

#[tokio::test]
async fn a_foreign_missing_and_deleted_chat_give_indistinguishable_404s() {
    let w = world(Pdp::TenantOnly).await;
    let deleted = w.rig.seed(&a1(), Some("deleted"), "m").await;
    assert_eq!(
        w.rig.delete(&chat_uri(deleted.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    // the same caller asks for: another user's chat, another tenant's chat, an unknown id and
    // an own soft-deleted chat
    let mut bodies = Vec::new();
    for id in [w.a2_chat.id, w.b1_chat.id, Uuid::new_v4(), deleted.id] {
        let reply = w.rig.get(&chat_uri(id), &a1()).await;
        bodies.push(blank(assert_chat_not_found(&reply)));
    }
    for body in &bodies[1..] {
        assert_eq!(body, &bodies[0]);
    }
}

#[tokio::test]
async fn a_user_cannot_rename_a_foreign_chat() {
    for behavior in PDPS {
        let w = world(behavior).await;
        for (who, foreign) in [(a1(), &w.a2_chat), (a2(), &w.a1_chat), (a1(), &w.b1_chat)] {
            let reply = w
                .rig
                .patch_json(&chat_uri(foreign.id), &who, r#"{"title":"hijacked"}"#)
                .await;
            assert_chat_not_found(&reply);
            let row = chat_row(&w.rig.db, foreign.id).await.unwrap();
            assert_eq!(row.title, foreign.title, "{behavior:?}");
            assert_eq!(row.updated_at, foreign.updated_at, "{behavior:?}");
        }
    }
}

#[tokio::test]
async fn a_user_cannot_delete_a_foreign_chat_or_mark_its_attachments() {
    for behavior in PDPS {
        let w = world(behavior).await;
        for (who, foreign) in [(a1(), &w.a2_chat), (a2(), &w.a1_chat), (b1(), &w.a1_chat)] {
            assert_chat_not_found(&w.rig.delete(&chat_uri(foreign.id), &who).await);
            assert_eq!(
                chat_row(&w.rig.db, foreign.id).await.unwrap().deleted_at,
                None,
                "{behavior:?}"
            );
            let rows = attachment_rows(&w.rig.db, foreign.id).await;
            assert!(
                rows.iter().all(|r| r.cleanup_status.is_none()),
                "{behavior:?}"
            );
        }
        assert!(
            w.rig.outbox_rows().await.is_empty(),
            "{behavior:?}: nothing enqueued"
        );
    }
}

#[tokio::test]
async fn a_deleted_chat_is_unreachable_for_every_caller() {
    let w = world(Pdp::TenantOnly).await;
    assert_eq!(
        w.rig.delete(&chat_uri(w.a1_chat.id), &a1()).await.status,
        StatusCode::NO_CONTENT
    );
    for who in [a1(), a2(), b1()] {
        assert_chat_not_found(&w.rig.get(&chat_uri(w.a1_chat.id), &who).await);
        assert_chat_not_found(&w.rig.delete(&chat_uri(w.a1_chat.id), &who).await);
        assert_chat_not_found(
            &w.rig
                .patch_json(&chat_uri(w.a1_chat.id), &who, r#"{"title":"x"}"#)
                .await,
        );
    }
    assert!(item_ids(&w.rig.get(CHATS, &a1()).await.json()).is_empty());
}

// ── create ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_created_chat_belongs_to_its_creator_alone() {
    for behavior in PDPS {
        let rig = rig(behavior).await;
        let mut created = Vec::new();
        for (who, user, tenant) in [
            (a1(), USER_A1, TENANT_A),
            (a2(), USER_A2, TENANT_A),
            (b1(), USER_B1, TENANT_B),
        ] {
            let reply = rig.post_json(CHATS, &who, r#"{"title":"mine"}"#).await;
            assert_eq!(
                reply.status,
                StatusCode::CREATED,
                "{behavior:?}: {}",
                reply.text
            );
            let id = Uuid::parse_str(reply.json()["id"].as_str().unwrap()).unwrap();
            let row = chat_row(&rig.db, id).await.unwrap();
            assert_eq!((row.user_id, row.tenant_id), (user, tenant), "{behavior:?}");
            created.push((who, id));
        }
        for (who, own) in &created {
            assert_eq!(item_ids(&rig.get(CHATS, who).await.json()), vec![*own]);
            for (_, other) in created.iter().filter(|(_, id)| id != own) {
                assert_chat_not_found(&rig.get(&chat_uri(*other), who).await);
            }
        }
    }
}

// ── the authorization matrix ─────────────────────────────────────────────────

#[tokio::test]
async fn the_five_operations_use_the_create_list_read_update_delete_matrix_rows() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("t"), "gpt-standard").await;
    let uri = chat_uri(chat.id);

    assert_eq!(
        rig.post_json(CHATS, &a1(), "{}").await.status,
        StatusCode::CREATED
    );
    assert_eq!(rig.get(CHATS, &a1()).await.status, StatusCode::OK);
    assert_eq!(rig.get(&uri, &a1()).await.status, StatusCode::OK);
    assert_eq!(
        rig.patch_json(&uri, &a1(), r#"{"title":"u"}"#).await.status,
        StatusCode::OK
    );
    assert_eq!(rig.delete(&uri, &a1()).await.status, StatusCode::NO_CONTENT);

    let seen: Vec<(String, Option<Uuid>, bool)> = rig
        .pdp
        .requests()
        .into_iter()
        .map(|r| (r.action.name, r.resource.id, r.context.require_constraints))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("create".to_owned(), None, true),
            ("list".to_owned(), None, true),
            ("read".to_owned(), Some(chat.id), true),
            ("update".to_owned(), Some(chat.id), true),
            ("delete".to_owned(), Some(chat.id), true),
        ]
    );
    for request in rig.pdp.requests() {
        assert_eq!(
            request.resource.resource_type,
            "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
        );
    }
}

/// The five operations as `(label, method, uri, body)` against `chat_id`.
type Operation = (&'static str, &'static str, String, Option<&'static str>);

fn operations(chat_id: Uuid) -> Vec<Operation> {
    vec![
        ("create", "POST", CHATS.to_owned(), Some("{}")),
        ("list", "GET", CHATS.to_owned(), None),
        ("get", "GET", chat_uri(chat_id), None),
        (
            "update",
            "PATCH",
            chat_uri(chat_id),
            Some(r#"{"title":"u"}"#),
        ),
        ("delete", "DELETE", chat_uri(chat_id), None),
    ]
}

async fn run(
    rig: &ChatRig,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> crate::test_support::chats::Reply {
    match body {
        Some(body) => {
            rig.send(crate::test_support::chats::json_request(
                method,
                uri,
                &a1(),
                body,
            ))
            .await
        }
        None => {
            rig.send(crate::test_support::chats::request(method, uri, &a1()))
                .await
        }
    }
}

#[tokio::test]
async fn every_operation_fails_closed_on_a_pdp_deny_an_unusable_decision_and_a_pdp_failure() {
    for (behavior, status) in [
        (Pdp::Deny, 403),
        (Pdp::NoConstraints, 403),
        (Pdp::UnknownProperty, 403),
        (Pdp::UnadvertisedPredicate, 403),
        (Pdp::Unreachable, 503),
    ] {
        let rig = rig(behavior).await;
        let chat = rig.seed(&a1(), Some("keep"), "gpt-standard").await;
        for (label, method, uri, body) in operations(chat.id) {
            let reply = run(&rig, method, &uri, body).await;
            let category = if status == 403 {
                "permission_denied"
            } else {
                "service_unavailable"
            };
            let problem = assert_problem(&reply, status, category);
            if status == 403 {
                assert_eq!(
                    problem["context"]["reason"], "AUTHZ_DENIED",
                    "{label} {behavior:?}"
                );
            } else {
                assert_eq!(reply.retry_after.as_deref(), Some("5"), "{label}");
            }
        }
        let row = chat_row(&rig.db, chat.id).await.unwrap();
        assert_eq!(row.deleted_at, None, "{behavior:?}");
        assert_eq!(row.title.as_deref(), Some("keep"), "{behavior:?}");
    }
}
