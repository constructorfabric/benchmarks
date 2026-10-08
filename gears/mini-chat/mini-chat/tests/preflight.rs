//! T030: preflight validation happens before any provider call and leaves no trace.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

async fn assert_untouched(h: &Harness, chat: Uuid) {
    assert!(
        h.provider.chat_requests().is_empty(),
        "no provider call expected"
    );
    assert!(db::turns(h, chat).await.is_empty(), "no turn row expected");
    assert!(h.messages(chat).await.is_empty(), "no message expected");
}

fn policy_with(f: impl FnOnce(&mut Value)) -> Value {
    let mut p = default_policy();
    f(&mut p);
    p
}

#[tokio::test]
async fn empty_content_rejected() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    for c in ["", "   \n\t"] {
        let r = h.send(chat, c).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_field_reason(&r.error, "content", "EMPTY_CONTENT");
    }
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn attachment_id_validation() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let other_chat = h.create_chat().await;
    let foreign = h
        .upload_ok(other_chat, "o.txt", "text/plain", b"other")
        .await;
    let own = h.upload_ok(chat, "a.txt", "text/plain", b"mine").await;
    // unknown id
    let r = h
        .send_body(
            chat,
            json!({"content": "x", "attachment_ids": [Uuid::new_v4()]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "attachment", "invalid_attachment");
    // attachment of another chat
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [foreign]}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "attachment", "invalid_attachment");
    // duplicates
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [own, own]}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "attachment", "invalid_attachment");
    // too many ids (> max_documents_per_chat + max_images_per_message)
    let ids: Vec<Uuid> = (0..60).map(|_| Uuid::new_v4()).collect();
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": ids}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "attachment", "invalid_attachment");
    // uploaded by another user of the same tenant
    let peer = ctx_for(h.tenant, Uuid::new_v4());
    let (s, _) = h.upload(&peer, chat, "p.txt", "text/plain", b"x").await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "peer cannot even upload into a foreign chat"
    );
    assert!(h.provider.chat_requests().is_empty());
    assert!(db::turns(&h, chat).await.is_empty());
    assert!(
        h.messages(chat).await.is_empty(),
        "rolled back user message"
    );
    // the valid id is accepted
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [own]}))
        .await;
    assert_eq!(r.names().last(), Some(&"done"));
}

#[tokio::test]
async fn not_ready_attachment_rejected() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    *h.provider.indexing_status.lock().unwrap() = "in_progress".into();
    let id = h
        .upload_ok(chat, "slow.txt", "text/plain", b"pending doc")
        .await;
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [id]}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_field_reason(&r.error, "attachment", "invalid_attachment");
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn too_many_images() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            h.upload_ok(chat, &format!("i{i}.png"), "image/png", &png(4, 4))
                .await,
        );
    }
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": ids}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "image_count", "TOO_MANY_IMAGES");
    assert_untouched(&h, chat).await;
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": &ids[..4]}))
        .await;
    assert_eq!(r.names().last(), Some(&"done"));
}

#[tokio::test]
async fn input_too_long_and_context_budget() {
    let h = Harness::with(Opts {
        policy: policy_with(|p| {
            p["model_catalog"][0]["max_input_tokens"] = json!(50);
            let mut tiny = model("tiny", "standard", true, false, (false, false, false));
            tiny["context_window"] = json!(1100);
            tiny["max_input_tokens"] = json!(0);
            p["model_catalog"].as_array_mut().unwrap().push(tiny);
        }),
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let r = h.send(chat, &"word ".repeat(100)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_field_reason(&r.error, "content", "INPUT_TOO_LONG");
    assert_untouched(&h, chat).await;

    let tiny = h.create_chat_as(&h.ctx(), json!({"model": "tiny"})).await;
    let r = h.send(tiny, &"word ".repeat(120)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_eq!(r.error["status"], 400);
    assert!(
        r.error.to_string().contains("CONTEXT_BUDGET_EXCEEDED"),
        "{}",
        r.error
    );
    assert_untouched(&h, tiny).await;
    // short messages fit
    let r = h.send(tiny, "short").await;
    assert_eq!(r.names().last(), Some(&"done"), "{r:?}");
}

#[tokio::test]
async fn kill_switches_web_search_and_images() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let img = h.upload_ok(chat, "i.png", "image/png", &png(8, 8)).await;
    h.set_policy(policy_with(|p| {
        p["kill_switches"] = json!({"disable_web_search": true, "disable_images": true});
    }));
    let r = h
        .send_body(
            chat,
            json!({"content": "x", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_violation(&r.error, "web_search");
    assert!(r.error.to_string().contains("FEATURE_DISABLED"));
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [img]}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_violation(&r.error, "images");
    assert_untouched(&h, chat).await;
    // plain messages still work
    let r = h.send(chat, "x").await;
    assert_eq!(r.names().last(), Some(&"done"));
}

#[tokio::test]
async fn vision_not_supported_after_downgrade() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let img = h.upload_ok(chat, "i.png", "image/png", &png(8, 8)).await;
    // premium exhausted -> downgrade to the standard model without vision
    h.set_policy(policy_with(|p| {
        p["default_premium_limits"] =
            json!({"limit_daily_credits_micro": 10, "limit_monthly_credits_micro": 10});
    }));
    let r = h
        .send_body(chat, json!({"content": "x", "attachment_ids": [img]}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_field_reason(&r.error, "content_type", "VISION_NOT_SUPPORTED");
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn quota_exhausted_is_429_before_provider_call() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.set_policy(policy_with(|p| {
        p["default_standard_limits"] =
            json!({"limit_daily_credits_micro": 10, "limit_monthly_credits_micro": 10});
        p["default_premium_limits"] =
            json!({"limit_daily_credits_micro": 10, "limit_monthly_credits_micro": 10});
    }));
    let r = h.send(chat, "x").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{r:?}");
    assert_violation(&r.error, "tokens");
    assert!(r.error.to_string().contains("quota_exceeded"));
    assert_untouched(&h, chat).await;
    assert!(
        db::quota_rows(&h)
            .await
            .iter()
            .all(|q| q.reserved_credits_micro == 0 && q.spent_credits_micro == 0)
    );
}

#[tokio::test]
async fn chat_model_removed_from_catalog() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.set_policy(policy_with(|p| {
        p["model_catalog"].as_array_mut().unwrap().remove(0);
    }));
    let r = h.send(chat, "x").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert_field_reason(&r.error, "model", "INVALID_MODEL");
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn policy_plugin_unavailable_is_internal_error() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.policy
        .fail_snapshot
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = h.send(chat, "x").await;
    assert!(r.status.is_server_error(), "{r:?}");
    assert!(
        r.error.get("code").is_none(),
        "problem has no top-level code"
    );
    assert_untouched(&h, chat).await;
}
