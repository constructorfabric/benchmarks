//! US3/US4/US6 — message history, turn status, reactions, turn mutations, models and quota
//! status (T048–T050, T054, T064).

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;
use uuid::Uuid;

async fn chat_with_turns(env: &TestEnv, n: usize) -> (Uuid, Vec<Uuid>) {
    let chat = env.create_chat("a1", json!({})).await;
    let mut rids = Vec::new();
    for i in 0..n {
        let rid = Uuid::new_v4();
        let r = env.stream("a1", chat, json!({"content": format!("question {i}"), "request_id": rid})).await;
        assert_eq!(r.names().last(), Some(&"done"));
        rids.push(rid);
    }
    (chat, rids)
}

#[tokio::test]
async fn message_list_order_filter_and_paging() {
    let env = TestEnv::start().await;
    let (chat, _) = chat_with_turns(&env, 3).await;
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(s, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(items[0]["content"], "question 0");
    for m in items {
        assert!(m["attachments"].is_array());
        assert!(m.get("my_reaction").is_some());
        assert!(m["request_id"].is_string());
    }
    assert!(items[0].get("model").is_none());
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages?$filter=role%20eq%20%27user%27"), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages?$orderby=created_at%20desc&limit=2"), None).await;
    assert_eq!(v["items"][0]["role"], "assistant");
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let cursor = v["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let (s, v2) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages?limit=2&cursor={cursor}"), None).await;
    assert_eq!(s, StatusCode::OK, "{v2}");
    // The cursor keeps the order of the first page (created_at desc).
    assert_eq!(v2["items"][0]["content"], "Hello from mock.");
    assert_eq!(v2["items"][1]["content"], "question 1");
}

#[tokio::test]
async fn turn_status_states_and_not_found() {
    let env = TestEnv::start().await;
    let (chat, rids) = chat_with_turns(&env, 1).await;
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{}", rids[0]), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "done");
    assert_eq!(v["request_id"], rids[0].to_string());
    assert!(v["updated_at"].is_string());
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{}", Uuid::new_v4()), None).await;
    assert_problem(s, &v, 404, "not_found");
    let (s, _) = env.json("a2", Method::GET, &format!("/chats/{chat}/turns/{}", rids[0]), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reactions_on_assistant_messages() {
    let env = TestEnv::start().await;
    let (chat, _) = chat_with_turns(&env, 1).await;
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    let user_msg = v["items"][0]["id"].as_str().unwrap().to_owned();
    let asst_msg = v["items"][1]["id"].as_str().unwrap().to_owned();
    let path = format!("/chats/{chat}/messages/{asst_msg}/reaction");
    let (s, r) = env.json("a1", Method::PUT, &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["reaction"], "like");
    let (s, r) = env.json("a1", Method::PUT, &path, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["reaction"], "dislike");
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(v["items"][1]["my_reaction"], "dislike");
    assert!(v["items"][0]["my_reaction"].is_null());
    assert_eq!(env.count("SELECT COUNT(*) FROM message_reactions").await, 1);

    let (s, v) = env.json("a1", Method::PUT, &path, Some(json!({"reaction": "love"}))).await;
    assert_problem(s, &v, 400, "invalid_argument");
    let (s, v) = env.json("a1", Method::PUT, &format!("/chats/{chat}/messages/{user_msg}/reaction"), Some(json!({"reaction": "like"}))).await;
    assert_problem(s, &v, 400, "failed_precondition");
    assert_eq!(v["context"]["violations"][0]["subject"], "reaction_target");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/messages/{user_msg}/reaction"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = env.json("a1", Method::PUT, &format!("/chats/{chat}/messages/{}/reaction", Uuid::new_v4()), Some(json!({"reaction": "like"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _) = env.json("a1", Method::DELETE, &path, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.json("a1", Method::DELETE, &path, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert!(v["items"][1]["my_reaction"].is_null());
    let (s, _) = env.json("b", Method::PUT, &path, Some(json!({"reaction": "like"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retry_replaces_the_latest_turn() {
    let env = TestEnv::start().await;
    let (chat, rids) = chat_with_turns(&env, 2).await;
    let old = rids[1];
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{old}/retry"), Method::POST, json!({})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.error);
    let started = r.event("stream_started").unwrap();
    let new_rid: Uuid = started["request_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new_rid, old);
    assert_eq!(started["is_new_turn"], true);
    assert_eq!(r.names().last(), Some(&"done"));
    let (s, _) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{old}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[2]["content"], "question 1");
    assert_eq!(items[3]["request_id"], new_rid.to_string());
    assert_eq!(
        env.count(&format!("SELECT COUNT(*) FROM chat_turns WHERE request_id = {} AND replaced_by_request_id = {}", blob(old), blob(new_rid))).await,
        1
    );
    // The provider request does not contain the replaced assistant answer twice.
    let last = env.mock.responses_requests().last().unwrap().body["input"].clone();
    assert_eq!(last.as_array().unwrap().len(), 3);
    // Only the latest turn may be retried.
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{}/retry", rids[0]), Method::POST, json!({})).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.error);
    let audit = env
        .eventually(Duration::from_secs(10), || async { env.audit.retries.lock().unwrap().first().cloned() })
        .await;
    assert!(audit.is_some());
}

#[tokio::test]
async fn edit_and_delete_latest_turn() {
    let env = TestEnv::start().await;
    let (chat, rids) = chat_with_turns(&env, 2).await;
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{}", rids[1]), Method::PATCH, json!({"content": "edited question"})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.error);
    let new_rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(v["items"][2]["content"], "edited question");
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{new_rid}"), Method::PATCH, json!({"content": "  "})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");

    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{}", rids[0]), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{new_rid}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let (s, _) = env.json("a1", Method::GET, &format!("/chats/{chat}/turns/{new_rid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // After the delete the previous turn is the latest again and can be deleted.
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{}", rids[0]), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}"), None).await;
    assert_eq!(v["message_count"], 0);
    let audit = env
        .eventually(Duration::from_secs(10), || async {
            let deletes = env.audit.deletes.lock().unwrap().len();
            let edits = env.audit.edits.lock().unwrap().len();
            (deletes == 2 && edits == 1).then_some(())
        })
        .await;
    assert!(audit.is_some(), "edit/delete audit events missing");
}

#[tokio::test]
async fn mutation_of_a_running_turn_is_rejected() {
    let env = std::sync::Arc::new(TestEnv::start().await);
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let e2 = env.clone();
    let h = tokio::spawn(async move { e2.stream("a1", chat, json!({"content": "[[slow]]", "request_id": rid})).await });
    env.eventually(Duration::from_secs(5), || async {
        (env.count("SELECT COUNT(*) FROM chat_turns WHERE state = 'running'").await == 1).then_some(())
    })
    .await
    .unwrap();
    let r = env.stream_path("a1", &format!("/chats/{chat}/turns/{rid}/retry"), Method::POST, json!({})).await;
    assert!(r.status == StatusCode::CONFLICT || r.status == StatusCode::BAD_REQUEST, "{:?}", r.status);
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert!(s.is_client_error());
    h.await.unwrap();
}

#[tokio::test]
async fn models_api_lists_enabled_models_without_internal_fields() {
    let env = TestEnv::start().await;
    let (s, v) = env.json("a1", Method::GET, "/models", None).await;
    assert_eq!(s, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 3);
    assert!(!ids.contains(&"disabled-model"));
    for m in items {
        assert!(m.get("provider_model_id").is_none());
        assert!(m.get("system_prompt").is_none());
        assert!(m.get("provider_id").is_none());
        assert!(m["tier"] == "premium" || m["tier"] == "standard");
    }
    let (s, v) = env.json("a1", Method::GET, "/models/gpt-4.1-mini", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["model_id"], "gpt-4.1-mini");
    let (s, v) = env.json("a1", Method::GET, "/models/disabled-model", None).await;
    assert_problem(s, &v, 404, "not_found");
    let (s, _) = env.json("a1", Method::GET, "/models/unknown", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn quota_status_reflects_usage() {
    let env = TestEnv::start().await;
    let (s, v) = env.json("a1", Method::GET, "/quota/status", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let tiers = v["tiers"].as_array().unwrap();
    assert!(tiers.iter().any(|t| t["tier"] == "premium"));
    assert!(tiers.iter().any(|t| t["tier"] == "total"));
    for t in tiers {
        for p in t["periods"].as_array().unwrap() {
            assert_eq!(p["used_credits_micro"], 0);
            assert_eq!(p["remaining_percentage"], 100);
        }
    }
    chat_with_turns(&env, 1).await;
    let (_, v) = env.json("a1", Method::GET, "/quota/status", None).await;
    let premium = v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "premium").unwrap().clone();
    let daily = premium["periods"].as_array().unwrap().iter().find(|p| p["period"] == "daily").unwrap().clone();
    assert_eq!(daily["used_credits_micro"], 210);
    assert_eq!(daily["limit_credits_micro"], 50_000_000);
    assert_eq!(daily["remaining_credits_micro"], 50_000_000 - 210);
    assert!(daily["next_reset"].as_str().unwrap().ends_with("T00:00:00Z"));
    // Another user sees their own (empty) usage.
    let (_, v) = env.json("a2", Method::GET, "/quota/status", None).await;
    let premium = v["tiers"].as_array().unwrap().iter().find(|t| t["tier"] == "premium").unwrap().clone();
    assert_eq!(premium["periods"][0]["used_credits_micro"], 0);
}

#[tokio::test]
async fn premium_exhaustion_downgrades_and_total_exhaustion_rejects() {
    let env = TestEnv::start().await;
    *env.policy.premium.write().unwrap() = mini_chat_sdk::TierLimits { limit_daily_credits_micro: 10, limit_monthly_credits_micro: 10 };
    let chat = env.create_chat("a1", json!({})).await;
    let rid = Uuid::new_v4();
    let r = env.stream("a1", chat, json!({"content": "hi", "request_id": rid})).await;
    let done = r.event("done").expect("done").clone();
    assert_eq!(done["selected_model"], "gpt-4.1");
    assert_eq!(done["effective_model"], "gpt-4.1-mini");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(env.mock.responses_requests()[0].body["model"], "gpt-4.1-mini-provider");
    let audit = env
        .eventually(Duration::from_secs(10), || async {
            env.audit.turns.lock().unwrap().iter().find(|a| a.request_id == rid).cloned()
        })
        .await
        .unwrap();
    assert_eq!(audit.policy_decisions.quota.decision, "downgrade");
    assert_eq!(audit.policy_decisions.quota.downgrade_from.as_deref(), Some("gpt-4.1"));
    assert_eq!(audit.policy_decisions.quota.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));

    *env.policy.standard.write().unwrap() = mini_chat_sdk::TierLimits { limit_daily_credits_micro: 10, limit_monthly_credits_micro: 10 };
    let r = env.stream("a1", chat, json!({"content": "hi again"})).await;
    assert_problem(r.status, &r.error, 429, "resource_exhausted");
    assert_eq!(env.mock.responses_requests().len(), 1);
}

#[tokio::test]
async fn kill_switches_force_standard_tier() {
    let env = TestEnv::start().await;
    env.policy.kill_switches(|k| k.force_standard_tier = true);
    let chat = env.create_chat("a1", json!({})).await;
    let r = env.stream("a1", chat, json!({"content": "hi"})).await;
    let done = r.event("done").unwrap();
    assert_eq!(done["effective_model"], "gpt-4.1-mini");
    assert_eq!(done["quota_decision"], "downgrade");
}

#[tokio::test]
async fn chat_model_removed_from_catalog_is_invalid_model() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({"model": "gpt-4.1-mini"})).await;
    env.policy.catalog(|c| c.retain(|m| m.id != "gpt-4.1-mini"));
    let r = env.stream("a1", chat, json!({"content": "hi"})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    assert_eq!(violation_reason(&r.error).as_deref(), Some("INVALID_MODEL"));
}
