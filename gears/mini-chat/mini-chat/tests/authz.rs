//! T024: tenant / owner isolation and PDP failure modes on every resource.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn other_user_and_other_tenant_get_404_everywhere() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let r = h.send(chat, "hello").await;
    let rid = r.request_id();
    let msgs = h.messages(chat).await;
    let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let att = h.upload_ok(chat, "a.txt", "text/plain", b"hello doc").await;

    let same_tenant_other_user = ctx_for(h.tenant, Uuid::new_v4());
    let other_tenant = ctx_for(Uuid::new_v4(), h.user);
    for ctx in [&same_tenant_other_user, &other_tenant] {
        let base = format!("/mini-chat/v1/chats/{chat}");
        for (m, u, b) in [
            ("GET", base.clone(), None),
            ("PATCH", base.clone(), Some(json!({"title": "x"}))),
            ("GET", format!("{base}/messages"), None),
            ("GET", format!("{base}/turns/{rid}"), None),
            ("GET", format!("{base}/attachments/{att}"), None),
            ("DELETE", format!("{base}/attachments/{att}"), None),
            (
                "PUT",
                format!("{base}/messages/{asst}/reaction"),
                Some(json!({"reaction": "like"})),
            ),
            ("DELETE", format!("{base}/messages/{asst}/reaction"), None),
            ("DELETE", format!("{base}/turns/{rid}"), None),
            ("DELETE", base.clone(), None),
        ] {
            let (s, v, _) = h.req(ctx, m, &u, b).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{m} {u}: {v}");
        }
        let r = h
            .sse(
                ctx,
                "POST",
                &format!("{base}/messages:stream"),
                Some(json!({"content": "x"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let r = h
            .sse(ctx, "POST", &format!("{base}/turns/{rid}/retry"), None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let (s, _) = h.upload(ctx, chat, "b.txt", "text/plain", b"x").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, v, _) = h.req(ctx, "GET", "/mini-chat/v1/chats", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            v["items"].as_array().unwrap().is_empty(),
            "foreign list must be empty: {v}"
        );
    }
    // owner still sees everything
    let (s, _) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn pdp_deny_is_403_and_failure_is_503() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.authz.mode.store(PDP_DENY, Ordering::SeqCst);
    let (s, v) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_reason(&v, "AUTHZ_DENIED");
    let (s, v, _) = h
        .req(&h.ctx(), "POST", "/mini-chat/v1/chats", Some(json!({})))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let r = h.send(chat, "x").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(h.provider.chat_requests().is_empty());

    h.authz.mode.store(PDP_FAIL, Ordering::SeqCst);
    let (s, v, hdr) = h
        .req(
            &h.ctx(),
            "GET",
            &format!("/mini-chat/v1/chats/{chat}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(hdr.get("retry-after").unwrap(), "5");
    let (s, _, _) = h.req(&h.ctx(), "GET", "/mini-chat/v1/chats", None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _, _) = h
        .req(&h.ctx(), "GET", "/mini-chat/v1/quota/status", None)
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}
