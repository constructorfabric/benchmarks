//! Models API and Reactions API.

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

/// Model list/get reflects only enabled catalog entries without internal fields.
#[tokio::test]
async fn models_reflect_enabled_catalog_without_internal_fields() {
    let h = Harness::new().await;
    let r = h.call(U1, "GET", "/models", None).await;
    assert_eq!(r.status, 200);
    let items = r.json()["items"].as_array().unwrap().clone();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 4);
    assert!(!ids.contains(&"disabled-1"));
    for id in ["premium-1", "standard-1", "novision", "tiny"] {
        assert!(ids.contains(&id), "{ids:?}");
    }
    let p = items.iter().find(|m| m["model_id"] == "premium-1").unwrap();
    assert_eq!(p["display_name"], "PREMIUM-1");
    assert_eq!(p["tier"], "premium");
    assert_eq!(p["multiplier_display"], "1x");
    assert_eq!(p["description"], "premium-1 model");
    assert_eq!(p["multimodal_capabilities"], json!(["VISION_INPUT"]));
    assert_eq!(p["context_window"], 128_000);
    let text = r.text();
    for internal in [
        "provider_model_id", "premium-1-provider", "provider_id", "credit_multiplier", "system_prompt",
        "estimation_budgets", "general_config", "max_input_tokens", "You are premium-1",
    ] {
        assert!(!text.contains(internal), "models expose {internal}");
    }

    let g = h.call(U1, "GET", "/models/standard-1", None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["model_id"], "standard-1");
    assert_eq!(g.json()["tier"], "standard");
    for id in ["disabled-1", "nope"] {
        let r = h.call(U1, "GET", &format!("/models/{id}"), None).await;
        assert_eq!(r.status, 404, "{id}");
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.model.v1~");
    }
    // Read-only.
    let r = h.call(U1, "POST", "/models", Some(json!({"model_id": "x"}))).await;
    assert_eq!(r.status, 405);
}

/// Set/remove reactions on assistant messages only, idempotently.
#[tokio::test]
async fn reactions_on_assistant_messages_only_idempotent() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    h.send_message(U1, chat, json!({"content": "q"})).await;
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    let user_msg = items[0]["id"].as_str().unwrap().to_owned();
    let asst = items[1]["id"].as_str().unwrap().to_owned();
    let path = format!("/chats/{chat}/messages/{asst}/reaction");

    let r = h.call(U1, "PUT", &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["message_id"], asst);
    assert_eq!(r.json()["reaction"], "like");
    assert!(r.json()["created_at"].is_string());
    // Same value again: idempotent.
    let r = h.call(U1, "PUT", &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200);
    // Change value: replaces.
    let r = h.call(U1, "PUT", &path, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["reaction"], "dislike");
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert_eq!(items[1]["my_reaction"], "dislike");

    // Validation and targets.
    let r = h.call(U1, "PUT", &path, Some(json!({"reaction": "love"}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "INVALID_REACTION");
    let r = h.call(U1, "PUT", &path, Some(json!({}))).await;
    assert_eq!(r.status, 422);
    let r = h.call(U1, "PUT", &format!("/chats/{chat}/messages/{user_msg}/reaction"), Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["type"], "STATE");
    let r = h.call(U1, "PUT", &format!("/chats/{chat}/messages/{}/reaction", Uuid::new_v4()), Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.message.v1~");

    // Remove: idempotent.
    assert_eq!(h.call(U1, "DELETE", &path, None).await.status, 204);
    assert_eq!(h.call(U1, "DELETE", &path, None).await.status, 204);
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert!(items[1]["my_reaction"].is_null());
    let r = h.call(U1, "DELETE", &format!("/chats/{chat}/messages/{user_msg}/reaction"), None).await;
    assert_eq!(r.status, 400);
    // Foreign users cannot react.
    let r = h.call(U2, "PUT", &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 404);
    // Reactions on messages of a deleted turn are rejected.
    let rid = items[1]["request_id"].as_str().unwrap();
    h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await;
    let r = h.call(U1, "PUT", &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 404);
}
