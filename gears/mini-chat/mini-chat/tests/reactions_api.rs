//! Message reactions: `PUT` / `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.
//!
//! Source of truth: DESIGN 3.3 "Message Reaction API" (set / remove rules and
//! errors), the REST error table (INVALID_REACTION is checked before
//! authorization; a body without `reaction` is 422; `reaction_target` is a
//! `failed_precondition` for `PUT` and `DELETE`), 3.7 `message_reactions`
//! (unique `(message_id, user_id)`, tenant and user taken from the security
//! context), 3.8 (`set_reaction` / `delete_reaction` on the chat resource) and
//! the OpenAPI schemas `SetReactionReq` / `MiniChatReactionDto`.

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

use chat_support::{
    CHAT_PDP_TYPE, ChatHarness, ChatHarnessOptions, ChatSeed, FakePdp, MessageSeed, TENANT_A,
    USER_A, USER_A2, assert_field_violation, assert_invalid_argument, assert_not_found,
    assert_precondition, assert_problem, assert_violation_reason, chat_harness, chat_harness_with,
    ctx_a, key_set, keys, page_items, parse_ts,
};
use serde_json::{Value, json};
use uuid::Uuid;

struct Fixture {
    h: ChatHarness,
    chat: Uuid,
    user_msg: Uuid,
    assistant_msg: Uuid,
    system_msg: Uuid,
}

async fn fixture_with(h: ChatHarness) -> Fixture {
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x500).titled("r"))
        .await;
    let user_msg = h
        .seed_message(MessageSeed::user(chat).with_id(0x5001).at(1))
        .await;
    let assistant_msg = h
        .seed_message(MessageSeed::assistant(chat).with_id(0x5002).at(2))
        .await;
    let system_msg = h
        .seed_message(MessageSeed::system(chat).with_id(0x5003).at(3))
        .await;
    Fixture {
        h,
        chat,
        user_msg,
        assistant_msg,
        system_msg,
    }
}

async fn fixture() -> Fixture {
    fixture_with(chat_harness().await).await
}

impl Fixture {
    fn uri(&self, message: Uuid) -> String {
        self.h.reaction_uri(self.chat, message)
    }

    async fn put(&self, message: Uuid, body: Value) -> chat_support::Resp {
        self.h.put_json(&self.uri(message), &ctx_a(), &body).await
    }

    async fn delete(&self, message: Uuid) -> chat_support::Resp {
        self.h.delete(&self.uri(message), &ctx_a()).await
    }

    async fn my_reaction(&self, message: Uuid) -> Value {
        let page = self.h.list_messages(self.chat, "", &ctx_a()).await.json();
        page_items(&page)
            .into_iter()
            .find(|m| m["id"] == json!(message.to_string()))
            .expect("message is listed")["my_reaction"]
            .clone()
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// PUT
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn put_like_on_an_assistant_message_returns_the_reaction() {
    let f = fixture().await;
    let resp = f.put(f.assistant_msg, json!({"reaction": "like"})).await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(
        key_set(&body),
        keys(&["message_id", "reaction", "created_at"])
    );
    assert_eq!(body["message_id"], json!(f.assistant_msg.to_string()));
    assert_eq!(body["reaction"], json!("like"));
    parse_ts(&body["created_at"]);
}

#[tokio::test]
async fn put_dislike_is_accepted_too() {
    let f = fixture().await;
    let resp = f.put(f.assistant_msg, json!({"reaction": "dislike"})).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert_eq!(resp.json()["reaction"], json!("dislike"));
}

#[tokio::test]
async fn put_stores_one_reaction_row_for_the_caller() {
    let f = fixture().await;
    f.put(f.assistant_msg, json!({"reaction": "like"})).await;

    let rows = f.h.reactions_of(f.assistant_msg).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].user_id, USER_A,
        "user_id comes from the security context"
    );
    assert_eq!(
        rows[0].tenant_id, TENANT_A,
        "tenant_id comes from the security context"
    );
    assert_eq!(rows[0].reaction, "like");
}

#[tokio::test]
async fn put_is_visible_as_my_reaction_in_the_message_list() {
    let f = fixture().await;
    assert_eq!(f.my_reaction(f.assistant_msg).await, Value::Null);
    f.put(f.assistant_msg, json!({"reaction": "dislike"})).await;
    assert_eq!(f.my_reaction(f.assistant_msg).await, json!("dislike"));
}

#[tokio::test]
async fn put_replaces_an_existing_reaction_upsert() {
    let f = fixture().await;
    f.put(f.assistant_msg, json!({"reaction": "like"})).await;

    let resp = f.put(f.assistant_msg, json!({"reaction": "dislike"})).await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert_eq!(resp.json()["reaction"], json!("dislike"));
    let rows = f.h.reactions_of(f.assistant_msg).await;
    assert_eq!(
        rows.len(),
        1,
        "unique (message_id, user_id): replaced, not added"
    );
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(f.my_reaction(f.assistant_msg).await, json!("dislike"));
}

#[tokio::test]
async fn put_is_idempotent_for_the_same_value() {
    let f = fixture().await;
    let first = f.put(f.assistant_msg, json!({"reaction": "like"})).await;
    let second = f.put(f.assistant_msg, json!({"reaction": "like"})).await;
    assert_eq!(first.status.as_u16(), 200);
    assert_eq!(second.status.as_u16(), 200, "{}", second.raw);
    assert_eq!(second.json()["reaction"], json!("like"));
    assert_eq!(f.h.reactions_of(f.assistant_msg).await.len(), 1);
}

#[tokio::test]
async fn put_on_a_user_or_system_message_is_400_failed_precondition_reaction_target() {
    let f = fixture().await;
    for message in [f.user_msg, f.system_msg] {
        let resp = f.put(message, json!({"reaction": "like"})).await;
        let problem = assert_problem(&resp, 400, "failed_precondition");
        assert_precondition(&problem, "reaction_target", "STATE");
    }
    assert!(f.h.all_reactions().await.is_empty(), "nothing was stored");
}

// ── reaction value validation ───────────────────────────────────────────────

#[tokio::test]
async fn put_with_an_invalid_reaction_value_is_400_invalid_reaction() {
    let f = fixture().await;
    for value in ["love", "", "LIKE", "Like", "thumbs_up"] {
        let resp = f.put(f.assistant_msg, json!({"reaction": value})).await;
        let problem = assert_invalid_argument(&resp, "reaction", "INVALID_REACTION");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.message.v1~"),
            "value {value:?}"
        );
    }
    assert!(f.h.all_reactions().await.is_empty());
}

#[tokio::test]
async fn the_reaction_value_is_validated_before_authorization_and_any_lookup() {
    let h = chat_harness_with(ChatHarnessOptions {
        pdp: FakePdp::deny("no grant"),
        ..Default::default()
    })
    .await;
    // Chat and message do not exist either: the value check comes first.
    let resp = h
        .put_json(
            &h.reaction_uri(Uuid::new_v4(), Uuid::new_v4()),
            &ctx_a(),
            &json!({"reaction": "love"}),
        )
        .await;

    assert_invalid_argument(&resp, "reaction", "INVALID_REACTION");
    assert!(
        h.pdp.recorded().is_empty(),
        "an invalid reaction never reaches the PDP"
    );
}

#[tokio::test]
async fn put_body_that_does_not_match_the_schema_is_422() {
    let f = fixture().await;
    let uri = f.uri(f.assistant_msg);
    for payload in [
        "{}",
        r#"{"reaction": null}"#,
        r#"{"reaction": 5}"#,
        r#"{"reaction": true}"#,
        r#"{"reaction": ["like"]}"#,
        r#"{"reaction": {"kind": "like"}}"#,
        r#"{"Reaction": "like"}"#,
        "null",
        "[]",
    ] {
        let resp =
            f.h.raw("PUT", &uri, &ctx_a(), Some("application/json"), payload)
                .await;
        let problem = assert_problem(&resp, 422, "invalid_argument");
        assert_field_violation(&problem, "body", "invalid_json_body");
    }
    assert!(f.h.all_reactions().await.is_empty());
}

#[tokio::test]
async fn put_malformed_json_is_400_and_a_non_json_content_type_is_415() {
    let f = fixture().await;
    let uri = f.uri(f.assistant_msg);
    let resp =
        f.h.raw(
            "PUT",
            &uri,
            &ctx_a(),
            Some("application/json"),
            r#"{"reaction": "li"#,
        )
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "body", "json_syntax_error");

    for content_type in [None, Some("text/plain")] {
        let resp =
            f.h.raw(
                "PUT",
                &uri,
                &ctx_a(),
                content_type,
                r#"{"reaction": "like"}"#,
            )
            .await;
        let problem = assert_problem(&resp, 415, "invalid_argument");
        assert_field_violation(&problem, "body", "missing_json_content_type");
    }
    assert!(f.h.all_reactions().await.is_empty());
}

// ── not found ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_on_an_unknown_message_is_404_for_the_message_resource() {
    let f = fixture().await;
    let resp = f.put(Uuid::new_v4(), json!({"reaction": "like"})).await;
    assert_not_found(&resp, "message");
}

#[tokio::test]
async fn put_on_a_soft_deleted_message_is_404_for_the_message_resource() {
    let f = fixture().await;
    let deleted =
        f.h.seed_message(
            MessageSeed::assistant(f.chat)
                .with_id(0x5010)
                .at(9)
                .deleted(),
        )
        .await;
    assert_not_found(
        &f.put(deleted, json!({"reaction": "like"})).await,
        "message",
    );
}

#[tokio::test]
async fn put_on_a_message_of_another_chat_is_404_for_the_message_resource() {
    let f = fixture().await;
    let other_chat = f.h.seed_chat(ChatSeed::of_a().with_id(0x501)).await;
    let elsewhere =
        f.h.seed_message(MessageSeed::assistant(other_chat).with_id(0x5011).at(1))
            .await;

    // The message exists, but not in the chat of the URL.
    let resp = f.put(elsewhere, json!({"reaction": "like"})).await;

    assert_not_found(&resp, "message");
    assert!(f.h.reactions_of(elsewhere).await.is_empty());
}

#[tokio::test]
async fn put_in_an_unknown_or_deleted_chat_is_404_for_the_chat_resource() {
    let f = fixture().await;
    let deleted_chat =
        f.h.seed_chat(ChatSeed::of_a().with_id(0x502).deleted())
            .await;
    let deleted_msg =
        f.h.seed_message(MessageSeed::assistant(deleted_chat).with_id(0x5020).at(1))
            .await;
    for (chat, message) in [
        (Uuid::new_v4(), f.assistant_msg),
        (deleted_chat, deleted_msg),
    ] {
        let resp =
            f.h.put_json(
                &f.h.reaction_uri(chat, message),
                &ctx_a(),
                &json!({"reaction": "like"}),
            )
            .await;
        assert_not_found(&resp, "chat");
    }
    assert!(f.h.all_reactions().await.is_empty());
}

// ═════════════════════════════════════════════════════════════════════════════
// DELETE
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delete_removes_the_reaction_and_returns_204_without_a_body() {
    let f = fixture().await;
    f.put(f.assistant_msg, json!({"reaction": "like"})).await;

    let resp = f.delete(f.assistant_msg).await;

    assert_eq!(resp.status.as_u16(), 204, "{}", resp.raw);
    assert!(resp.raw.is_empty(), "204 has no body: {:?}", resp.raw);
    assert!(f.h.reactions_of(f.assistant_msg).await.is_empty());
    assert_eq!(f.my_reaction(f.assistant_msg).await, Value::Null);
}

#[tokio::test]
async fn delete_is_idempotent_whether_or_not_a_reaction_existed() {
    let f = fixture().await;
    assert_eq!(
        f.delete(f.assistant_msg).await.status.as_u16(),
        204,
        "nothing to remove"
    );
    f.put(f.assistant_msg, json!({"reaction": "dislike"})).await;
    assert_eq!(f.delete(f.assistant_msg).await.status.as_u16(), 204);
    assert_eq!(f.delete(f.assistant_msg).await.status.as_u16(), 204);
}

#[tokio::test]
async fn delete_on_a_user_or_system_message_is_400_failed_precondition_reaction_target() {
    let f = fixture().await;
    for message in [f.user_msg, f.system_msg] {
        let resp = f.delete(message).await;
        let problem = assert_problem(&resp, 400, "failed_precondition");
        assert_precondition(&problem, "reaction_target", "STATE");
    }
}

#[tokio::test]
async fn delete_on_an_unknown_message_is_404_for_the_message_resource() {
    let f = fixture().await;
    assert_not_found(&f.delete(Uuid::new_v4()).await, "message");
}

#[tokio::test]
async fn delete_on_a_message_of_another_chat_is_404_for_the_message_resource() {
    let f = fixture().await;
    let other_chat = f.h.seed_chat(ChatSeed::of_a().with_id(0x503)).await;
    let elsewhere =
        f.h.seed_message(MessageSeed::assistant(other_chat).with_id(0x5030).at(1))
            .await;
    f.h.seed_reaction(TENANT_A, USER_A, elsewhere, "like").await;

    assert_not_found(&f.delete(elsewhere).await, "message");
    assert_eq!(
        f.h.reactions_of(elsewhere).await.len(),
        1,
        "the reaction of the other chat is untouched"
    );
}

#[tokio::test]
async fn delete_in_an_unknown_chat_is_404_for_the_chat_resource() {
    let f = fixture().await;
    let resp =
        f.h.delete(&f.h.reaction_uri(Uuid::new_v4(), f.assistant_msg), &ctx_a())
            .await;
    assert_not_found(&resp, "chat");
}

#[tokio::test]
async fn delete_removes_only_the_callers_reaction() {
    let f = fixture().await;
    // Another user's row on the same message (e.g. created while they could see the chat).
    f.h.seed_reaction(TENANT_A, USER_A2, f.assistant_msg, "dislike")
        .await;
    f.put(f.assistant_msg, json!({"reaction": "like"})).await;
    assert_eq!(f.h.reactions_of(f.assistant_msg).await.len(), 2);

    assert_eq!(f.delete(f.assistant_msg).await.status.as_u16(), 204);

    let rows = f.h.reactions_of(f.assistant_msg).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, USER_A2);
    assert_eq!(rows[0].reaction, "dislike");
}

#[tokio::test]
async fn put_replaces_only_the_callers_reaction() {
    let f = fixture().await;
    f.h.seed_reaction(TENANT_A, USER_A2, f.assistant_msg, "dislike")
        .await;
    f.put(f.assistant_msg, json!({"reaction": "like"})).await;

    let rows = f.h.reactions_of(f.assistant_msg).await;
    assert_eq!(rows.len(), 2, "one row per (message, user)");
    let mine = rows.iter().find(|r| r.user_id == USER_A).unwrap();
    let theirs = rows.iter().find(|r| r.user_id == USER_A2).unwrap();
    assert_eq!(mine.reaction, "like");
    assert_eq!(theirs.reaction, "dislike");
    assert_eq!(f.my_reaction(f.assistant_msg).await, json!("like"));
}

// ═════════════════════════════════════════════════════════════════════════════
// authorization
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn reactions_ask_the_pdp_for_the_chat_scoped_actions() {
    let f = fixture().await;

    f.put(f.assistant_msg, json!({"reaction": "like"})).await;
    let req = f.h.pdp.last();
    assert_eq!(req.action.name, "set_reaction");
    assert_eq!(req.resource.resource_type, CHAT_PDP_TYPE);
    assert_eq!(
        req.resource.id,
        Some(f.chat),
        "authorization is anchored to the chat"
    );
    assert!(req.context.require_constraints);

    f.delete(f.assistant_msg).await;
    let req = f.h.pdp.last();
    assert_eq!(req.action.name, "delete_reaction");
    assert_eq!(req.resource.resource_type, CHAT_PDP_TYPE);
    assert_eq!(req.resource.id, Some(f.chat));
}

#[tokio::test]
async fn a_denied_reaction_is_403_and_stores_nothing() {
    let f = fixture_with(
        chat_harness_with(ChatHarnessOptions {
            pdp: FakePdp::deny("no grant"),
            ..Default::default()
        })
        .await,
    )
    .await;

    let put = f.put(f.assistant_msg, json!({"reaction": "like"})).await;
    let problem = assert_problem(&put, 403, "permission_denied");
    assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    let delete = f.delete(f.assistant_msg).await;
    assert_problem(&delete, 403, "permission_denied");
    assert!(f.h.all_reactions().await.is_empty());
}

#[tokio::test]
async fn non_uuid_path_parameters_are_400_invalid_path_params() {
    let f = fixture().await;
    let prefix = &f.h.prefix;
    for uri in [
        format!(
            "{prefix}/v1/chats/not-a-uuid/messages/{}/reaction",
            f.assistant_msg
        ),
        format!("{prefix}/v1/chats/{}/messages/not-a-uuid/reaction", f.chat),
    ] {
        let put =
            f.h.put_json(&uri, &ctx_a(), &json!({"reaction": "like"}))
                .await;
        let problem = assert_problem(&put, 400, "invalid_argument");
        assert_violation_reason(&problem, "invalid_path_params");
        let delete = f.h.delete(&uri, &ctx_a()).await;
        let problem = assert_problem(&delete, 400, "invalid_argument");
        assert_violation_reason(&problem, "invalid_path_params");
    }
}

#[tokio::test]
async fn reaction_operations_are_registered_with_stable_ids() {
    let f = fixture().await;
    let key = "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction";
    for (method, operation_id, ok) in [
        ("PUT", "mini_chat.put_reaction", 200u16),
        ("DELETE", "mini_chat.delete_reaction", 204),
    ] {
        let spec =
            f.h.openapi
                .operation_specs
                .get(&format!("{method}:{key}"))
                .unwrap_or_else(|| panic!("{method} {key} is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id));
        assert!(spec.authenticated);
        let statuses: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
        for status in [ok, 400, 403, 404, 500, 503] {
            assert!(
                statuses.contains(&status),
                "{method} declares {status}: {statuses:?}"
            );
        }
        if method == "PUT" {
            assert!(statuses.contains(&422), "PUT declares 422: {statuses:?}");
        }
    }
}
