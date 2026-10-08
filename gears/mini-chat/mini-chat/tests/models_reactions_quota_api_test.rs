//! Models API (enabled entries only, no internal fields), reactions
//! (assistant messages only, idempotent), quota status API.
#![allow(clippy::many_single_char_names)]

mod common;

use common::*;
use http::StatusCode;
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn models_list_and_get_show_enabled_entries_only() {
    let h = Harness::new().await;
    h.policy.update(|s| {
        let mut m = s.model_catalog[1].clone();
        m.id = "hidden".to_owned();
        m.enabled = false;
        s.model_catalog.push(m);
    });
    let a = user_a();
    let r = h.get("/mini-chat/v1/models", &a).await;
    assert_eq!(r.status, StatusCode::OK);
    let items = r.body["items"].as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["gpt-premium", "gpt-standard"]);
    let m = &items[0];
    assert_eq!(m["display_name"], "GPT-PREMIUM");
    assert_eq!(m["tier"], "premium");
    assert_eq!(m["multiplier_display"], "1x");
    assert_eq!(m["description"], "gpt-premium model");
    assert_eq!(m["context_window"], 128_000);
    assert_eq!(m["multimodal_capabilities"], json!(["VISION_INPUT", "RAG"]));
    let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
    for k in &keys {
        assert!(
            [
                "model_id",
                "display_name",
                "tier",
                "multiplier_display",
                "description",
                "multimodal_capabilities",
                "context_window"
            ]
            .contains(&k.as_str()),
            "unexpected field {k}"
        );
    }
    assert!(!r.text.contains("provider"), "no provider identifiers: {}", r.text);
    assert!(!r.text.contains("credit_multiplier"));

    let g = h.get("/mini-chat/v1/models/gpt-standard", &a).await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body["tier"], "standard");
    for missing in ["hidden", "nope"] {
        let g = h.get(&format!("/mini-chat/v1/models/{missing}"), &a).await;
        assert_eq!(g.status, StatusCode::NOT_FOUND, "{missing}");
        assert_eq!(g.body["context"]["resource_type"], "gts.cf.core.mini_chat.model.v1~");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_catalog_lists_nothing() {
    let h = Harness::with(Options {
        catalog: vec![],
        ..Options::default()
    })
    .await;
    let r = h.get("/mini-chat/v1/models", &user_a()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["items"], json!([]));
    let c = h.create_chat_with(&user_a(), json!({})).await;
    assert_eq!(c.status, StatusCode::BAD_REQUEST);
    assert_eq!(c.body["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
}

#[tokio::test(flavor = "multi_thread")]
async fn reactions_on_assistant_messages_only() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, "hi").await;
    let assistant = s.message_id();
    let user_msg = h.messages(&a, chat).await[0]["id"].as_str().unwrap().to_owned();
    let uri = |m: &str| format!("/mini-chat/v1/chats/{chat}/messages/{m}/reaction");

    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["message_id"], assistant.to_string());
    assert_eq!(r.body["reaction"], "like");
    assert!(r.body["created_at"].is_string());
    // idempotent upsert / change
    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(r.body["reaction"], "dislike");
    assert_eq!(h.scalar("SELECT COUNT(*) FROM message_reactions").await, 1);
    assert_eq!(h.messages(&a, chat).await[1]["my_reaction"], "dislike");

    // invalid value → 400 before authorization; missing field → 422
    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({"reaction": "love"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_REACTION");
    h.set_pdp(PDP_DENY);
    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({"reaction": "love"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "validated before authorization");
    h.set_pdp(PDP_ALLOW);
    let r = h.req("PUT", &uri(&assistant.to_string()), &a, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    // user messages cannot be reacted to
    for (m, b) in [("PUT", Some(json!({"reaction": "like"}))), ("DELETE", None)] {
        let r = h.req(m, &uri(&user_msg), &a, b).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{m}");
        assert_eq!(r.body["context"]["violations"][0]["subject"], "reaction_target");
        assert_eq!(r.body["context"]["violations"][0]["type"], "STATE");
    }
    // unknown message
    let r = h.req("PUT", &uri(&Uuid::new_v4().to_string()), &a, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.mini_chat.message.v1~");

    // delete is idempotent
    let r = h.req("DELETE", &uri(&assistant.to_string()), &a, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = h.req("DELETE", &uri(&assistant.to_string()), &a, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(h.scalar("SELECT COUNT(*) FROM message_reactions").await, 0);
    assert!(h.messages(&a, chat).await[1]["my_reaction"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_status_reports_usage_and_warnings() {
    let h = Harness::new().await;
    let a = user_a();
    h.policy.set_limits((100_000, 1_000_000), (3_300, 100_000));
    let r = h.get("/mini-chat/v1/quota/status", &a).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["warning_threshold_pct"], 80);
    let tiers = r.body["tiers"].as_array().unwrap();
    let names: Vec<&str> = tiers.iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["premium", "total"]);
    for t in tiers {
        let periods: Vec<&str> = t["periods"].as_array().unwrap().iter().map(|p| p["period"].as_str().unwrap()).collect();
        assert_eq!(periods, vec!["daily", "monthly"]);
        for p in t["periods"].as_array().unwrap() {
            assert_eq!(p["used_credits_micro"], 0);
            assert_eq!(p["remaining_percentage"], 100);
            assert_eq!(p["warning"], false);
            assert_eq!(p["exhausted"], false);
            assert!(p["next_reset"].as_str().unwrap().ends_with('Z'));
        }
    }
    // Use 2900 of the premium daily 3300 (the ~3.1k reserve still fits) → warning.
    let chat = h.create_chat(&a).await;
    h.gw.push(text_reply(&["x"], 200, 900));
    let s = h.send(&a, chat, "hi").await;
    let done = s.first("done").unwrap_or_else(|| panic!("{}", s.raw));
    let warnings = done["quota_warnings"].as_array().unwrap();
    let pd = warnings.iter().find(|w| w["tier"] == "premium" && w["period"] == "daily").unwrap();
    assert_eq!(done["quota_decision"], "allow");
    assert_eq!(pd["remaining_percentage"], 12, "{done}");
    assert_eq!(pd["warning"], true);
    assert_eq!(pd["exhausted"], false);
    assert!(pd["next_reset"].is_string());

    let r = h.get("/mini-chat/v1/quota/status", &a).await;
    let premium = &r.body["tiers"][0];
    let daily = &premium["periods"][0];
    assert_eq!(daily["limit_credits_micro"], 3_300);
    assert_eq!(daily["used_credits_micro"], 2_900);
    assert_eq!(daily["remaining_credits_micro"], 400);
    assert_eq!(daily["remaining_percentage"], 12);
    assert_eq!(daily["warning"], true);
    // another user is unaffected
    let other = h.get("/mini-chat/v1/quota/status", &user_a2()).await;
    assert_eq!(other.body["tiers"][0]["periods"][0]["used_credits_micro"], 0);
}
