//! T022/T023: chat CRUD, validation, listing (filter/order/pagination), activity ordering.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

const CHATS: &str = "/mini-chat/v1/chats";

#[tokio::test]
async fn create_chat_default_model_trimmed_title_location() {
    let h = Harness::new().await;
    let (s, v, hdr) = h
        .req(&h.ctx(), "POST", CHATS, Some(json!({"title": "  Hello  "})))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["title"], "Hello");
    assert_eq!(v["model"], "prem");
    assert_eq!(v["is_temporary"], false);
    assert_eq!(v["message_count"], 0);
    let id = v["id"].as_str().unwrap();
    assert_eq!(
        hdr.get("location").unwrap().to_str().unwrap(),
        format!("/mini-chat/v1/chats/{id}")
    );
}

#[tokio::test]
async fn create_chat_explicit_model_and_untitled() {
    let h = Harness::new().await;
    let (s, v, _) = h
        .req(&h.ctx(), "POST", CHATS, Some(json!({"model": "std"})))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["model"], "std");
    assert!(
        v.get("title").is_none(),
        "title must be omitted when null: {v}"
    );
    let (s, v, _) = h
        .req(&h.ctx(), "POST", CHATS, Some(json!({"title": null})))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(v.get("title").is_none());
}

#[tokio::test]
async fn create_chat_invalid_model() {
    let h = Harness::new().await;
    for m in ["nope", "off"] {
        let (s, v, _) = h
            .req(&h.ctx(), "POST", CHATS, Some(json!({"model": m})))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert_field_reason(&v, "model", "INVALID_MODEL");
    }
}

#[tokio::test]
async fn create_chat_invalid_title_checked_before_model() {
    let h = Harness::new().await;
    for t in ["   ".to_owned(), "x".repeat(256)] {
        let (s, v, _) = h
            .req(
                &h.ctx(),
                "POST",
                CHATS,
                Some(json!({"title": t, "model": "nope"})),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert_field_reason(&v, "title", "INVALID_TITLE");
    }
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "POST",
            CHATS,
            Some(json!({"title": "y".repeat(255)})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // title validated before authz: a PDP denial does not mask it
    h.authz
        .mode
        .store(PDP_DENY, std::sync::atomic::Ordering::SeqCst);
    let (s, v, _) = h
        .req(&h.ctx(), "POST", CHATS, Some(json!({"title": " "})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn get_patch_delete_lifecycle() {
    let h = Harness::new().await;
    let id = h
        .create_chat_as(&h.ctx(), json!({"title": "first", "model": "std"}))
        .await;
    let url = format!("{CHATS}/{id}");
    let (s, v) = h.get(&url).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["title"], "first");
    let created_updated = v["updated_at"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // unknown fields ignored, model immutable
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "PATCH",
            &url,
            Some(json!({"title": "  renamed ", "model": "prem", "x": 1})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["title"], "renamed");
    assert_eq!(v["model"], "std");
    assert_ne!(v["updated_at"].as_str().unwrap(), created_updated);

    // title required
    let (s, _, _) = h.req(&h.ctx(), "PATCH", &url, Some(json!({}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _, _) = h
        .req(&h.ctx(), "PATCH", &url, Some(json!({"title": null})))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, v, _) = h
        .req(&h.ctx(), "PATCH", &url, Some(json!({"title": "  "})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_field_reason(&v, "title", "INVALID_TITLE");

    let (s, _, _) = h.req(&h.ctx(), "DELETE", &url, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, v) = h.get(&url).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.chat"),
        "{v}"
    );
    let (s, _, _) = h.req(&h.ctx(), "DELETE", &url, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h.get(&format!("{url}/messages")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h
        .req(&h.ctx(), "PATCH", &url, Some(json!({"title": "z"})))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let r = h.send(id, "hi").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // deleted chats disappear from the list
    let (_, v) = h.get(CHATS).await;
    assert!(v["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bad_path_params_and_body() {
    let h = Harness::new().await;
    let (s, v) = h.get(&format!("{CHATS}/not-a-uuid")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let mut r = axum::http::Request::builder()
        .method("POST")
        .uri(CHATS)
        .header("content-type", "application/json");
    r = r.header("x", "y");
    let (s, _) = h
        .raw(&h.ctx(), r.body(axum::body::Body::from("{bad")).unwrap())
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_ordering_by_activity_and_pagination() {
    let h = Harness::new().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            h.create_chat_as(&h.ctx(), json!({"title": format!("chat {i}")}))
                .await,
        );
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let (s, v) = h.get(CHATS).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let got: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let want: Vec<String> = ids.iter().rev().map(ToString::to_string).collect();
    assert_eq!(got, want);
    assert_eq!(v["page_info"]["limit"], 20);

    // sending a message moves the oldest chat to the top
    let r = h.send(ids[0], "hello").await;
    assert_eq!(r.names().last().copied(), Some("done"), "{r:?}");
    let (_, v) = h.get(CHATS).await;
    assert_eq!(v["items"][0]["id"], ids[0].to_string());
    assert_eq!(v["items"][0]["message_count"], 2);

    // renaming also bumps activity
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "PATCH",
            &format!("{CHATS}/{}", ids[2]),
            Some(json!({"title": "chat 2b"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = h.get(CHATS).await;
    assert_eq!(v["items"][0]["id"], ids[2].to_string());

    // cursor pagination walks all pages without duplicates
    let mut seen = Vec::new();
    let mut url = format!("{CHATS}?limit=2");
    loop {
        let (s, v) = h.get(&url).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let items = v["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(items.iter().map(|c| c["id"].as_str().unwrap().to_owned()));
        match v["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{CHATS}?limit=2&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 5);
}

#[tokio::test]
async fn list_filter_orderby_and_limits() {
    let h = Harness::new().await;
    for t in ["alpha", "beta", "gamma"] {
        h.create_chat_as(&h.ctx(), json!({"title": t})).await;
    }
    let (s, v) = h
        .get(&format!("{CHATS}?$filter=contains(title,'et')"))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "beta");

    let (s, v) = h.get(&format!("{CHATS}?$orderby=title%20asc")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let titles: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["alpha", "beta", "gamma"]);

    let (s, v) = h.get(&format!("{CHATS}?$select=id,title")).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let (s, v) = h.get(&format!("{CHATS}?limit=1000")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["page_info"]["limit"], 100);

    let (s, v) = h.get(&format!("{CHATS}?limit=0")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("INVALID_LIMIT"), "{v}");

    let (s, v) = h.get(&format!("{CHATS}?cursor=garbage")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("INVALID_CURSOR"), "{v}");

    let (s, v) = h
        .get(&format!("{CHATS}?$filter=nosuchfield%20eq%201"))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = h.get(&format!("{CHATS}?$filter=title%20eq")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = h.get(&format!("{CHATS}?$orderby=bogus%20desc")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = h.get(&format!("{CHATS}?$skip=1")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}
