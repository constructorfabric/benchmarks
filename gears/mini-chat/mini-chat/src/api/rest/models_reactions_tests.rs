//! Router tests: models API and message reactions (acceptance: Models API,
//! Reactions API, Messages API "reaction field consistency").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use uuid::Uuid;

use crate::test_support::{AuthzMode, TestEnv, user_a};

#[tokio::test]
async fn models_list_and_get_show_only_enabled_without_internals() {
    let env = TestEnv::new().await;
    let r = env.get("/mini-chat/v1/models").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = r.json()["items"].as_array().unwrap().clone();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 4);
    assert!(!ids.contains(&"old-model"), "disabled models are hidden");
    let m = items.iter().find(|m| m["model_id"] == "gpt-4.1").unwrap();
    assert_eq!(m["display_name"], "GPT-4.1");
    assert_eq!(m["tier"], "premium");
    assert_eq!(m["multiplier_display"], "1x");
    assert_eq!(m["description"], "gpt-4.1 model");
    assert_eq!(m["context_window"], 1_047_576);
    let caps = m["multimodal_capabilities"].as_array().unwrap();
    assert!(caps.contains(&json!("VISION_INPUT")));
    let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
    let mut sorted: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec!["context_window", "description", "display_name", "model_id", "multimodal_capabilities", "multiplier_display", "tier"]
    );
    let text = r.text();
    for internal in ["credit_multiplier", "provider_model_id", "provider_id", "system_prompt", "max_output_tokens", "estimation_budgets"] {
        assert!(!text.contains(internal), "models API leaks {internal}");
    }
    let r = env.get("/mini-chat/v1/models/gpt-4.1-mini").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["tier"], "standard");
    env.get("/mini-chat/v1/models/old-model").await.assert_problem(404, "model");
    env.get("/mini-chat/v1/models/nope").await.assert_problem(404, "model");
    env.set_authz(AuthzMode::Deny);
    env.get("/mini-chat/v1/models").await.assert_problem(403, "AUTHZ_DENIED");
    env.set_authz(AuthzMode::Fail);
    assert_eq!(env.get("/mini-chat/v1/models/gpt-4.1").await.status, 503);
}

async fn chat_with_answer(env: &TestEnv) -> (String, String, String) {
    let chat = env.chat(None).await;
    env.send_msg(&chat, "hello").await;
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let user = msgs["items"][0]["id"].as_str().unwrap().to_owned();
    let assistant = msgs["items"][1]["id"].as_str().unwrap().to_owned();
    (chat, user, assistant)
}

#[tokio::test]
async fn reactions_set_replace_remove_idempotently() {
    let env = TestEnv::new().await;
    let (chat, user_msg, asst) = chat_with_answer(&env).await;
    let uri = format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction");
    let r = env.call(&user_a(), "PUT", &uri, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["message_id"], asst.as_str());
    assert_eq!(v["reaction"], "like");
    assert!(v["created_at"].is_string());
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"][1]["my_reaction"], "like");
    assert!(msgs["items"][0]["my_reaction"].is_null());
    // upsert
    let r = env.call(&user_a(), "PUT", &uri, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(r.json()["reaction"], "dislike");
    assert_eq!(env.count("SELECT COUNT(*) FROM message_reactions").await, 1);
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"][1]["my_reaction"], "dislike");
    // delete, twice
    assert_eq!(env.call(&user_a(), "DELETE", &uri, None).await.status, 204);
    assert_eq!(env.call(&user_a(), "DELETE", &uri, None).await.status, 204);
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert!(msgs["items"][1]["my_reaction"].is_null());
    // user message → 400 reaction_target / STATE, for PUT and DELETE
    let user_uri = format!("/mini-chat/v1/chats/{chat}/messages/{user_msg}/reaction");
    for (m, body) in [("PUT", Some(json!({"reaction": "like"}))), ("DELETE", None)] {
        let r = env.call(&user_a(), m, &user_uri, body).await;
        assert_eq!(r.status, 400, "{}", r.text());
        assert_eq!(r.json()["context"]["violations"][0]["subject"], "reaction_target");
        assert_eq!(r.json()["context"]["violations"][0]["type"], "STATE");
    }
    // unknown message / unknown chat
    let r = env
        .call(&user_a(), "PUT", &format!("/mini-chat/v1/chats/{chat}/messages/{}/reaction", Uuid::new_v4()), Some(json!({"reaction": "like"})))
        .await;
    r.assert_problem(404, "message");
    let r = env
        .call(&user_a(), "PUT", &format!("/mini-chat/v1/chats/{}/messages/{asst}/reaction", Uuid::new_v4()), Some(json!({"reaction": "like"})))
        .await;
    r.assert_problem(404, "gts.cf.core.mini_chat.chat.v1~");
}

#[tokio::test]
async fn reaction_value_is_validated_before_authorization() {
    let env = TestEnv::new().await;
    let (chat, _, asst) = chat_with_answer(&env).await;
    let uri = format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction");
    env.set_authz(AuthzMode::Deny);
    let r = env.call(&user_a(), "PUT", &uri, Some(json!({"reaction": "love"}))).await;
    r.assert_problem(400, "INVALID_REACTION");
    assert_eq!(r.json()["context"]["field_violations"][0]["field"], "reaction");
    let r = env.call(&user_a(), "PUT", &uri, Some(json!({}))).await;
    assert_eq!(r.status, 422);
    env.set_authz(AuthzMode::Allow);
    let r = env.call(&user_a(), "PUT", &uri, Some(json!({"reaction": "LIKE"}))).await;
    r.assert_problem(400, "INVALID_REACTION");
}
