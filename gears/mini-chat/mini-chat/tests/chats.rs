//! Chat CRUD, listing, ordering, models API, quota status, error contract and
//! isolation (acceptance criteria: Chat CRUD, Models API, Quota Status API,
//! Authorization, Principles: tenant/owner isolation, model immutability).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn create_get_update_delete_lifecycle() {
    let h = Harness::new().await;
    let u = user();
    let r = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({"title": "  Q3 report  "}))).await;
    assert_eq!(r.status, 201, "{}", r.text);
    let body = r.json();
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(r.headers["location"], format!("/mini-chat/v1/chats/{id}").as_str());
    assert_eq!(body["title"], "Q3 report");
    assert_eq!(body["model"], "prem", "default model is the first enabled is_default entry");
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["message_count"], 0);
    assert!(body.get("user_id").is_none());

    let g = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["id"], id.as_str());

    // title omitted when absent
    let untitled = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({"model": "std"}))).await.json();
    assert!(untitled.get("title").is_none());
    assert_eq!(untitled["model"], "std");

    // rename ignores model and bumps updated_at
    let before = g.json()["updated_at"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let p = h
        .call(&u, "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": "Renamed", "model": "std"})))
        .await;
    assert_eq!(p.status, 200, "{}", p.text);
    assert_eq!(p.json()["title"], "Renamed");
    assert_eq!(p.json()["model"], "prem", "model is immutable");
    assert!(p.json()["updated_at"].as_str().unwrap() > before.as_str());

    let d = h.call(&u, "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(d.status, 204);
    let g = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(g.status, 404);
    assert_eq!(g.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    let d2 = h.call(&u, "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(d2.status, 404);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn title_and_model_validation() {
    let h = Harness::new().await;
    let u = user();
    for t in ["", "   ", &"x".repeat(256)] {
        let r = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({"title": t}))).await;
        assert_eq!(r.status, 400, "title {t:?}");
        assert_eq!(reason(&r.json()), "INVALID_TITLE");
    }
    let ok = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({"title": "y".repeat(255)}))).await;
    assert_eq!(ok.status, 201);
    for m in ["unknown-model", "off"] {
        let r = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({"model": m}))).await;
        assert_eq!(r.status, 400);
        assert_eq!(reason(&r.json()), "INVALID_MODEL");
        assert_eq!(r.json()["context"]["field_violations"][0]["field"], "model");
    }
    let id = h.create_chat(&u, json!({})).await;
    let r = h.call(&u, "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": "  "}))).await;
    assert_eq!((r.status, reason(&r.json())), (400, "INVALID_TITLE".to_owned()));
    let r = h.call(&u, "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": null}))).await;
    assert_eq!(r.status, 422);
    let r = h.call(&u, "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({}))).await;
    assert_eq!(r.status, 422);
    let req = http::Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{not json"))
        .unwrap();
    assert_eq!(h.raw(&u, req).await.status, 400);
    let r = h.call(&u, "GET", "/mini-chat/v1/chats/not-a-uuid", None).await;
    assert_eq!(r.status, 400);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_pagination_filter_order_and_errors() {
    let h = Harness::new().await;
    let u = user();
    let mut ids = Vec::new();
    for t in ["alpha", "beta", "gamma"] {
        ids.push(h.create_chat(&u, json!({"title": t})).await);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let r = h.call(&u, "GET", "/mini-chat/v1/chats?limit=2", None).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let page = r.json();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["title"], "gamma", "default order updated_at desc");
    assert_eq!(page["page_info"]["limit"], 2);
    let cursor = page["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let r2 = h.call(&u, "GET", &format!("/mini-chat/v1/chats?limit=2&cursor={cursor}"), None).await;
    assert_eq!(r2.json()["items"][0]["title"], "alpha");
    assert!(r2.json()["page_info"]["next_cursor"].is_null());

    let r = h.call(&u, "GET", "/mini-chat/v1/chats?$filter=contains(title,'et')", None).await;
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "beta");
    let r = h.call(&u, "GET", "/mini-chat/v1/chats?$orderby=title%20asc", None).await;
    assert_eq!(r.json()["items"][0]["title"], "alpha");
    let r = h.call(&u, "GET", "/mini-chat/v1/chats?limit=1000", None).await;
    assert_eq!(r.json()["page_info"]["limit"], 100, "limit clamped");

    for (q, field_reason) in [
        ("limit=0", "INVALID_LIMIT"),
        ("$filter=nonexistent%20eq%20'x'", ""),
        ("$filter=title%20eq", ""),
        ("cursor=garbage", "INVALID_CURSOR"),
        ("$orderby=bogus%20asc", ""),
        ("$skip=1", ""),
    ] {
        let r = h.call(&u, "GET", &format!("/mini-chat/v1/chats?{q}"), None).await;
        assert_eq!(r.status, 400, "{q}: {}", r.text);
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.odata.query.v1~", "{q}");
        if !field_reason.is_empty() {
            assert_eq!(reason(&r.json()), field_reason, "{q}");
        }
    }

    // deleted chats are not listed
    h.call(&u, "DELETE", &format!("/mini-chat/v1/chats/{}", ids[1]), None).await;
    let r = h.call(&u, "GET", "/mini-chat/v1/chats", None).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 2);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ordering_reflects_latest_activity_and_message_count() {
    let h = Harness::new().await;
    let u = user();
    let older = h.create_chat(&u, json!({"title": "older", "model": "std"})).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let _newer = h.create_chat(&u, json!({"title": "newer", "model": "std"})).await;
    let r = h.send(&u, older, json!({"content": "hello"})).await;
    assert_eq!(r.status, 200);
    let list = h.call(&u, "GET", "/mini-chat/v1/chats", None).await.json();
    assert_eq!(list["items"][0]["title"], "older");
    assert_eq!(list["items"][0]["message_count"], 2);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn isolation_between_users_and_tenants() {
    let h = Harness::new().await;
    let owner = user();
    let same_tenant_other_user = ctx(Uuid::new_v4(), owner.subject_tenant_id());
    let other_tenant = user();
    let chat = h.create_chat(&owner, json!({})).await;
    let s = h.send(&owner, chat, json!({"content": "secret"})).await;
    assert_eq!(s.status, 200);
    let rid = s.sse()[0].1["request_id"].as_str().unwrap().to_owned();
    let msgs = h.call(&owner, "GET", &format!("/mini-chat/v1/chats/{chat}/messages"), None).await.json();
    let asst = msgs["items"][1]["id"].as_str().unwrap().to_owned();
    let png = png(10, 10);
    let att = h.upload(&owner, chat, "a.png", "image/png", &png).await.json();
    let aid = att["id"].as_str().unwrap().to_owned();
    for intruder in [&same_tenant_other_user, &other_tenant] {
        for (m, uri, body) in [
            ("GET", format!("/mini-chat/v1/chats/{chat}"), None),
            ("PATCH", format!("/mini-chat/v1/chats/{chat}"), Some(json!({"title": "x"}))),
            ("DELETE", format!("/mini-chat/v1/chats/{chat}"), None),
            ("GET", format!("/mini-chat/v1/chats/{chat}/messages"), None),
            ("POST", format!("/mini-chat/v1/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))),
            ("GET", format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None),
            ("POST", format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry"), None),
            ("DELETE", format!("/mini-chat/v1/chats/{chat}/turns/{rid}"), None),
            ("GET", format!("/mini-chat/v1/chats/{chat}/attachments/{aid}"), None),
            ("DELETE", format!("/mini-chat/v1/chats/{chat}/attachments/{aid}"), None),
            ("PUT", format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction"), Some(json!({"reaction": "like"}))),
            ("DELETE", format!("/mini-chat/v1/chats/{chat}/messages/{asst}/reaction"), None),
        ] {
            let r = h.call(intruder, m, &uri, body).await;
            assert_eq!(r.status, 404, "{m} {uri}: {}", r.text);
            assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
        }
        let list = h.call(intruder, "GET", "/mini-chat/v1/chats", None).await.json();
        assert_eq!(list["items"].as_array().unwrap().len(), 0);
        let up = h.upload(intruder, chat, "b.png", "image/png", &png).await;
        assert_eq!(up.status, 404);
    }
    // owner still sees everything unchanged
    let g = h.call(&owner, "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["message_count"], 2);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn authz_denial_and_pdp_failure_fail_closed() {
    let h = Harness::new().await;
    let u = user();
    *h.pdp.mode.lock() = PdpMode::Deny;
    let r = h.call(&u, "GET", "/mini-chat/v1/chats", None).await;
    assert_eq!(r.status, 403);
    assert_eq!(reason(&r.json()), "AUTHZ_DENIED");
    *h.pdp.mode.lock() = PdpMode::Fail;
    let r = h.call(&u, "POST", "/mini-chat/v1/chats", Some(json!({}))).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.headers["retry-after"], "5");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn models_api_hides_disabled_and_internal_fields() {
    let h = Harness::new().await;
    let u = user();
    let r = h.call(&u, "GET", "/mini-chat/v1/models", None).await;
    assert_eq!(r.status, 200);
    let items = r.json()["items"].as_array().unwrap().clone();
    let ids: Vec<&str> = items.iter().map(|m| m["model_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"prem") && ids.contains(&"std"));
    assert!(!ids.contains(&"off"));
    for m in &items {
        for k in ["model_id", "display_name", "tier", "multiplier_display", "multimodal_capabilities", "context_window"] {
            assert!(m.get(k).is_some(), "missing {k}");
        }
        for k in ["provider_id", "provider_model_id", "input_tokens_credit_multiplier_micro", "is_default", "preference", "max_output_tokens"] {
            assert!(m.get(k).is_none(), "leaks {k}");
        }
    }
    let prem = items.iter().find(|m| m["model_id"] == "prem").unwrap();
    assert_eq!(prem["tier"], "premium");
    let g = h.call(&u, "GET", "/mini-chat/v1/models/std", None).await;
    assert_eq!((g.status, g.json()["tier"].as_str()), (200, Some("standard")));
    for id in ["off", "nope"] {
        let g = h.call(&u, "GET", &format!("/mini-chat/v1/models/{id}"), None).await;
        assert_eq!(g.status, 404);
        assert_eq!(g.json()["context"]["resource_type"], "gts.cf.core.mini_chat.model.v1~");
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_status_reflects_usage() {
    let h = Harness::new().await;
    let u = user();
    let r = h.call(&u, "GET", "/mini-chat/v1/quota/status", None).await;
    assert_eq!(r.status, 200);
    let s = r.json();
    assert_eq!(s["warning_threshold_pct"], 80);
    let tiers: Vec<&str> = s["tiers"].as_array().unwrap().iter().map(|t| t["tier"].as_str().unwrap()).collect();
    assert_eq!(tiers, vec!["premium", "total"]);
    let total_daily = &s["tiers"][1]["periods"][0];
    assert_eq!(total_daily["period"], "daily");
    assert_eq!(total_daily["used_credits_micro"], 0);
    assert_eq!(total_daily["remaining_percentage"], 100);
    assert!(total_daily["next_reset"].as_str().unwrap().ends_with("T00:00:00Z"));

    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    assert_eq!(h.send(&u, chat, json!({"content": "hi"})).await.status, 200);
    let s = h.call(&u, "GET", "/mini-chat/v1/quota/status", None).await.json();
    // 100 input * 1x + 50 output * 3x = 250 micro-credits, reserve released
    assert_eq!(s["tiers"][1]["periods"][0]["used_credits_micro"], 250);
    assert_eq!(s["tiers"][1]["periods"][1]["used_credits_micro"], 250);
    assert_eq!(s["tiers"][0]["periods"][0]["used_credits_micro"], 0, "standard turn does not touch premium");
    h.shutdown().await;
}
