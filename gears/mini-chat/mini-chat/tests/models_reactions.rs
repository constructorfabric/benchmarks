//! US6: models API and reactions API.
//!
//! AC: Models API (enabled entries only, no internal fields), Reactions API (assistant only,
//! idempotent set/remove).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::*;
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_list_and_get_show_enabled_entries_only() {
    let h = Harness::new().await;
    let r = h.send(ALICE, "GET", "/models", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = r.json()["items"].as_array().unwrap().clone();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"premium-m") && ids.contains(&"standard-m"));
    assert!(!ids.contains(&"disabled-m"), "disabled models are hidden");
    let p = items.iter().find(|m| m["model_id"] == json!("premium-m")).unwrap();
    assert_eq!(p["display_name"], json!("PREMIUM-M"));
    assert_eq!(p["tier"], json!("premium"));
    assert_eq!(p["multiplier_display"], json!("3x"));
    assert_eq!(p["description"], json!("premium-m model"));
    assert_eq!(p["multimodal_capabilities"], json!(["VISION_INPUT"]));
    assert_eq!(p["context_window"], json!(128_000));
    let s = r.text();
    for internal in ["provider_model_id", "prov-", "system_prompt", "credit_multiplier", "api_params", "provider_id", "estimation_budgets", "max_tool_calls"] {
        assert!(!s.contains(internal), "{internal} leaked: {s}");
    }

    let r = h.send(ALICE, "GET", "/models/standard-m", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["model_id"], json!("standard-m"));
    assert_eq!(r.json()["tier"], json!("standard"));
    let p = h.send(ALICE, "GET", "/models/disabled-m", None).await.problem(404);
    assert_eq!(p["context"]["resource_type"], json!("gts.cf.core.mini_chat.model.v1~"));
    h.send(ALICE, "GET", "/models/nope", None).await.problem(404);

    // Catalog changes are reflected (read-only view of the policy snapshot).
    h.policy.set(&policy_cfg(json!([model("only-m", "Standard", true, true, json!({}))]), json!({})));
    let r = h.send(CAROL, "GET", "/models", None).await;
    let ids: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|m| m["model_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, vec!["only-m"]);
}

async fn chat_with_turn(h: &Harness) -> (Uuid, Uuid, Uuid) {
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "hello").await;
    let msgs = h.messages(chat).await;
    let user = msgs.iter().find(|m| m.role == "user").unwrap().id;
    let assistant = msgs.iter().find(|m| m.role == "assistant").unwrap().id;
    (chat, user, assistant)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reactions_upsert_and_delete_idempotently() {
    let h = Harness::new().await;
    let (chat, _, assistant) = chat_with_turn(&h).await;
    let path = format!("/chats/{chat}/messages/{assistant}/reaction");

    let r = h.send(ALICE, "PUT", &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["message_id"], json!(assistant.to_string()));
    assert_eq!(v["reaction"], json!("like"));
    assert!(v["created_at"].is_string());
    let msgs = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    let a = msgs["items"].as_array().unwrap().iter().find(|m| m["role"] == json!("assistant")).unwrap().clone();
    assert_eq!(a["my_reaction"], json!("like"));

    // Same value again: idempotent; other value: replaces.
    assert_eq!(h.send(ALICE, "PUT", &path, Some(json!({"reaction": "like"}))).await.status, 200);
    let r = h.send(ALICE, "PUT", &path, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(r.json()["reaction"], json!("dislike"));
    let rows = {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use toolkit_db::secure::SecureEntityExt;
        ent::message_reactions::Entity::find()
            .filter(ent::message_reactions::Column::MessageId.eq(assistant))
            .secure()
            .scope_with(&toolkit_security::AccessScope::allow_all())
            .all(&h.db.conn().unwrap())
            .await
            .unwrap()
    };
    assert_eq!(rows.len(), 1, "one reaction per user and message");
    assert_eq!(rows[0].reaction, "dislike");

    assert_eq!(h.send(ALICE, "DELETE", &path, None).await.status, 204);
    assert_eq!(h.send(ALICE, "DELETE", &path, None).await.status, 204, "idempotent delete");
    let msgs = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    assert!(msgs["items"].as_array().unwrap().iter().all(|m| m["my_reaction"].is_null()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reactions_validation() {
    let h = Harness::new().await;
    let (chat, user, assistant) = chat_with_turn(&h).await;
    // user message → 400 STATE (PUT and DELETE)
    for (m, b) in [("PUT", Some(json!({"reaction": "like"}))), ("DELETE", None)] {
        let p = h.send(ALICE, m, &format!("/chats/{chat}/messages/{user}/reaction"), b).await.problem(400);
        assert_eq!(p["context"]["violations"][0]["type"], json!("STATE"), "{p}");
        assert_eq!(p["context"]["violations"][0]["subject"], json!("reaction_target"));
    }
    // invalid value
    let p = h.send(ALICE, "PUT", &format!("/chats/{chat}/messages/{assistant}/reaction"), Some(json!({"reaction": "love"}))).await.problem(400);
    assert_eq!(fv_reason(&p), "INVALID_REACTION");
    // unknown message, foreign chat
    h.send(ALICE, "PUT", &format!("/chats/{chat}/messages/{}/reaction", Uuid::new_v4()), Some(json!({"reaction": "like"}))).await.problem(404);
    h.send(BOB, "PUT", &format!("/chats/{chat}/messages/{assistant}/reaction"), Some(json!({"reaction": "like"}))).await.problem(404);
    // message of a deleted turn
    let rid = h.turns(chat).await[0].request_id;
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await.status, 204);
    h.send(ALICE, "PUT", &format!("/chats/{chat}/messages/{assistant}/reaction"), Some(json!({"reaction": "like"}))).await.problem(404);
}
