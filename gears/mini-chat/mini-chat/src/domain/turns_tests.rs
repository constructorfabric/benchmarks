//! Turn mutation tests: retry / edit / delete of the latest terminal turn (DESIGN §3.9).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use http::StatusCode;
use mini_chat_sdk::TierLimits;
use uuid::Uuid;

use crate::domain::stream::tests::{insert_attachment, messages, turns};
use crate::testing::{STANDARD, StreamScript, TestApp, USER_A1, USER_A2, ctx, ctx_a1, event_names, text_stream, TENANT_A};

async fn send_ok(t: &TestApp, chat_id: Uuid, content: &str) -> Uuid {
    let (st, events, err) = t.send(&ctx_a1(), chat_id, serde_json::json!({"content": content})).await;
    assert_eq!(st, StatusCode::OK, "{err}");
    events[0].data["request_id"].as_str().unwrap().parse().unwrap()
}

fn turn_uri(chat_id: Uuid, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}")
}

#[tokio::test]
async fn retry_latest_turn_regenerates_with_new_request_id() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = send_ok(&t, chat_id, "question").await;
    t.provider.push_events(text_stream(&["Second", " answer"], 12, 6));
    let (st, events, err) = t.call_sse(&c, "POST", &format!("{}/retry", turn_uri(chat_id, rid)), None).await;
    assert_eq!(st, StatusCode::OK, "{err}");
    assert_eq!(event_names(&events), vec!["stream_started", "delta", "delta", "done"]);
    let new_rid: Uuid = events[0].data["request_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new_rid, rid);
    assert_eq!(new_rid.get_version_num(), 4);
    assert_eq!(events[0].data["is_new_turn"], true);

    let ts = turns(&t, chat_id).await;
    let old = ts.iter().find(|x| x.request_id == rid).unwrap();
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(new_rid));
    let new = ts.iter().find(|x| x.request_id == new_rid).unwrap();
    assert_eq!(new.state, "completed");
    assert!(new.reserve_tokens.is_some(), "retry turn gets preflight columns");

    // The provider got the original question again.
    let body = t.provider.chat_bodies().last().cloned().unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "the replaced turn is not part of the history");
    assert_eq!(input[0]["content"][0]["text"], "question");

    // Listing shows only the new pair; old turn status 404; old request id conflicts.
    let (_, _, list) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/messages"), None).await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|m| m["request_id"] == new_rid.to_string()));
    assert_eq!(items[1]["content"], "Second answer");
    let (st, _, _) = t.call(&c, "GET", &turn_uri(chat_id, rid), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, err) = t.send(&c, chat_id, serde_json::json!({"content": "x", "request_id": rid})).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "request_id_conflict");
    t.eventually("audit turn_retry", || async {
        t.audit.events.lock().unwrap().iter().any(|e| e.event_type() == "turn_retry")
    })
    .await;
}

#[tokio::test]
async fn edit_replaces_content() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = send_ok(&t, chat_id, "typo questoin").await;
    let (st, events, _) =
        t.call_sse(&c, "PATCH", &turn_uri(chat_id, rid), Some(serde_json::json!({"content": "fixed question"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(event_names(&events).last().unwrap(), "done");
    let live: Vec<_> = messages(&t, chat_id).await.into_iter().filter(|m| m.deleted_at.is_none()).collect();
    assert_eq!(live[0].content, "fixed question");
    assert_eq!(t.provider.chat_bodies().last().unwrap()["input"][0]["content"][0]["text"], "fixed question");
    let (st, _, err) =
        t.call_sse(&c, "PATCH", &turn_uri(chat_id, Uuid::new_v4()), Some(serde_json::json!({"content": "  "}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["field_violations"][0]["reason"], "EMPTY_CONTENT");
    t.eventually("audit turn_edit", || async { t.audit.events.lock().unwrap().iter().any(|e| e.event_type() == "turn_edit") })
        .await;
}

#[tokio::test]
async fn delete_latest_turn() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let first = send_ok(&t, chat_id, "one").await;
    let second = send_ok(&t, chat_id, "two").await;
    // Not latest → 409 NOT_LATEST_TURN.
    let (st, _, err) = t.call(&c, "DELETE", &turn_uri(chat_id, first), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "NOT_LATEST_TURN");
    let (st, _, _) = t.call(&c, "DELETE", &turn_uri(chat_id, second), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, _) = t.call(&c, "GET", &turn_uri(chat_id, second), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Deleted turn → NOT_LATEST_TURN on further mutations.
    let (st, _, err) = t.call(&c, "DELETE", &turn_uri(chat_id, second), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "NOT_LATEST_TURN");
    // The previous turn is latest again.
    let (st, _, _) = t.call(&c, "DELETE", &turn_uri(chat_id, first), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, _, chat) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}"), None).await;
    assert_eq!(chat["message_count"], 0);
    let (st, _, _) = t.call(&c, "DELETE", &turn_uri(chat_id, Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    t.eventually("audit turn_delete", || async {
        t.audit.events.lock().unwrap().iter().filter(|e| e.event_type() == "turn_delete").count() == 2
    })
    .await;
}

#[tokio::test]
async fn retry_of_non_latest_or_foreign_turn_is_rejected() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let first = send_ok(&t, chat_id, "one").await;
    let _second = send_ok(&t, chat_id, "two").await;
    let (st, _, err) = t.call_sse(&c, "POST", &format!("{}/retry", turn_uri(chat_id, first)), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err["context"]["reason"], "NOT_LATEST_TURN");
    let (st, _, _) = t.call_sse(&ctx(USER_A2, TENANT_A), "POST", &format!("{}/retry", turn_uri(chat_id, first)), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "another user's chat is invisible");
}

#[tokio::test]
async fn mutation_of_running_turn_is_failed_precondition() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    t.provider.push_stream(StreamScript::EventsThenHang { events: vec![], delay: std::time::Duration::ZERO });
    let rid = Uuid::new_v4();
    let router = t.router();
    let c2 = c.clone();
    let handle = tokio::spawn(async move {
        let mut req = http::Request::builder()
            .method("POST")
            .uri(format!("/mini-chat/v1/chats/{chat_id}/messages:stream"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::json!({"content": "x", "request_id": rid}).to_string()))
            .unwrap();
        req.extensions_mut().insert(c2);
        let resp = tower::ServiceExt::oneshot(router, req).await.unwrap();
        let mut body = resp.into_body();
        while http_body_util::BodyExt::frame(&mut body).await.is_some() {}
    });
    t.eventually("running", || async { !turns(&t, chat_id).await.is_empty() }).await;
    for (method, suffix) in [("POST", "/retry"), ("DELETE", "")] {
        let (st, _, err) = t.call(&c, method, &format!("{}{suffix}", turn_uri(chat_id, rid)), None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(err["context"]["violations"][0]["subject"], "turn_state");
        assert_eq!(err["context"]["violations"][0]["type"], "STATE");
    }
    handle.abort();
    t.eventually("cancelled", || async { turns(&t, chat_id).await[0].state == "cancelled" }).await;
    // A cancelled turn can be retried.
    let (st, _, _) = t.call_sse(&c, "POST", &format!("{}/retry", turn_uri(chat_id, rid)), None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn preflight_rejection_keeps_previous_turn() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = send_ok(&t, chat_id, "q").await;
    t.policy.set_limits(
        TierLimits { limit_daily_credits_micro: 1, limit_monthly_credits_micro: 1 },
        TierLimits { limit_daily_credits_micro: 1, limit_monthly_credits_micro: 1 },
    );
    let (st, _, err) = t.call_sse(&c, "POST", &format!("{}/retry", turn_uri(chat_id, rid)), None).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{err}");
    let ts = turns(&t, chat_id).await;
    assert_eq!(ts.len(), 1);
    assert!(ts[0].deleted_at.is_none());
    assert_eq!(ts[0].state, "completed");
}

#[tokio::test]
async fn concurrent_retries_resolve_deterministically() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let rid = send_ok(&t, chat_id, "q").await;
    let uri = format!("{}/retry", turn_uri(chat_id, rid));
    let (a, b) = tokio::join!(t.call_sse(&c, "POST", &uri, None), t.call_sse(&c, "POST", &uri, None));
    let statuses = [a.0, b.0];
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 1, "{statuses:?}");
    let loser = if a.0 == StatusCode::OK { &b.2 } else { &a.2 };
    let reason = loser["context"]["reason"].as_str().unwrap_or_else(|| panic!("{loser} {statuses:?}"));
    assert!(["GENERATION_IN_PROGRESS", "NOT_LATEST_TURN"].contains(&reason), "{reason}");
    let live: Vec<_> = turns(&t, chat_id).await.into_iter().filter(|x| x.deleted_at.is_none()).collect();
    assert_eq!(live.len(), 1);
}

#[tokio::test]
async fn retry_carries_attachments_and_web_search_forward() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let img = insert_attachment(&t, chat_id, USER_A1, "image", "ready", Some("file-img0000000000123"), false, false).await;
    let (st, events, _) = t
        .send(&c, chat_id, serde_json::json!({"content": "look", "attachment_ids": [img], "web_search": {"enabled": true}}))
        .await;
    assert_eq!(st, StatusCode::OK);
    let rid: Uuid = events[0].data["request_id"].as_str().unwrap().parse().unwrap();
    let (st, events, _) = t.call_sse(&c, "POST", &format!("{}/retry", turn_uri(chat_id, rid)), None).await;
    assert_eq!(st, StatusCode::OK);
    let new_rid = events[0].data["request_id"].as_str().unwrap().to_owned();
    let body = t.provider.chat_bodies().last().cloned().unwrap();
    assert!(body["input"].to_string().contains("file-img0000000000123"), "image re-sent");
    assert!(body["tools"].to_string().contains("web_search"), "web_search flag reused");
    let (_, _, list) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat_id}/messages"), None).await;
    let user_msg = list["items"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap().clone();
    assert_eq!(user_msg["request_id"], new_rid);
    assert_eq!(user_msg["attachments"][0]["attachment_id"], img.to_string());
}

#[tokio::test]
async fn retry_with_images_respects_image_guards() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat_id = t.create_chat(&c, Some(STANDARD)).await;
    let img = insert_attachment(&t, chat_id, USER_A1, "image", "ready", Some("file-img0000000000124"), false, false).await;
    let (_, events, _) = t.send(&c, chat_id, serde_json::json!({"content": "look", "attachment_ids": [img]})).await;
    let rid = events[0].data["request_id"].as_str().unwrap().to_owned();
    t.policy.with_snapshot(|s| s.kill_switches.disable_images = true);
    let (st, _, err) = t.call_sse(&c, "POST", &format!("/mini-chat/v1/chats/{chat_id}/turns/{rid}/retry"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(err["context"]["violations"][0]["subject"], "images");
}
