//! Chat CRUD, list (`OData` filter/order/pagination), validation, ordering by
//! activity and model immutability.
#![allow(clippy::many_single_char_names)]

mod common;

use common::*;
use http::StatusCode;
use serde_json::json;
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";

#[tokio::test(flavor = "multi_thread")]
async fn create_get_update_delete_lifecycle() {
    let h = Harness::new().await;
    let a = user_a();

    let r = h.create_chat_with(&a, json!({"title": "  My chat  "})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let id = r.body["id"].as_str().unwrap().to_owned();
    assert_eq!(r.headers["location"], format!("{CHATS}/{id}"));
    assert_eq!(r.body["title"], "My chat");
    assert_eq!(r.body["model"], "gpt-premium", "default model is the is_default entry");
    assert_eq!(r.body["is_temporary"], false);
    assert_eq!(r.body["message_count"], 0);
    assert!(r.body.get("user_id").is_none() && r.body.get("tenant_id").is_none());

    let g = h.get(&format!("{CHATS}/{id}"), &a).await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body["id"], id.as_str());

    let u = h
        .req("PATCH", &format!("{CHATS}/{id}"), &a, Some(json!({"title": "Renamed"})))
        .await;
    assert_eq!(u.status, StatusCode::OK, "{}", u.text);
    assert_eq!(u.body["title"], "Renamed");

    let d = h.req("DELETE", &format!("{CHATS}/{id}"), &a, None).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    let g = h.get(&format!("{CHATS}/{id}"), &a).await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    assert_eq!(g.body["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    let d2 = h.req("DELETE", &format!("{CHATS}/{id}"), &a, None).await;
    assert_eq!(d2.status, StatusCode::NOT_FOUND, "second delete is 404");

    // Soft delete only.
    let n = h
        .scalar(&format!(
            "SELECT COUNT(*) FROM chats WHERE id = {} AND deleted_at IS NOT NULL",
            blob(Uuid::parse_str(&id).unwrap())
        ))
        .await;
    assert_eq!(n, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn untitled_chat_omits_title() {
    let h = Harness::new().await;
    let r = h.create_chat_with(&user_a(), json!({"title": null})).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert!(r.body.get("title").is_none(), "title omitted when absent: {}", r.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn title_validation() {
    let h = Harness::new().await;
    let a = user_a();
    for bad in [json!(""), json!("   "), json!("x".repeat(256))] {
        let r = h.create_chat_with(&a, json!({"title": bad})).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
        assert_eq!(r.body["context"]["field_violations"][0]["field"], "title");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_TITLE");
    }
    let ok = h.create_chat_with(&a, json!({"title": "x".repeat(255)})).await;
    assert_eq!(ok.status, StatusCode::CREATED);
    let id = ok.body["id"].as_str().unwrap();
    let r = h
        .req("PATCH", &format!("{CHATS}/{id}"), &a, Some(json!({"title": " "})))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_TITLE");
    // Missing title / null title: schema violation (422).
    let r = h.req("PATCH", &format!("{CHATS}/{id}"), &a, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    let r = h
        .req("PATCH", &format!("{CHATS}/{id}"), &a, Some(json!({"title": null})))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread")]
async fn model_validation_on_create() {
    let h = Harness::new().await;
    h.policy.update(|s| {
        let mut m = s.model_catalog[1].clone();
        m.id = "gpt-disabled".to_owned();
        m.enabled = false;
        s.model_catalog.push(m);
    });
    let a = user_a();
    for bad in ["nope", "gpt-disabled"] {
        let r = h.create_chat_with(&a, json!({"model": bad})).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
        assert_eq!(r.body["context"]["field_violations"][0]["field"], "model");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
    }
    let r = h.create_chat_with(&a, json!({"model": "gpt-standard"})).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.body["model"], "gpt-standard");
}

#[tokio::test(flavor = "multi_thread")]
async fn default_model_falls_back_to_first_enabled() {
    let h = Harness::with(Options {
        catalog: vec![
            model("m-disabled", "premium", json!({"enabled": false})),
            model("m-first", "standard", json!({})),
            model("m-second", "standard", json!({})),
        ],
        ..Options::default()
    })
    .await;
    let r = h.create_chat_with(&user_a(), json!({})).await;
    assert_eq!(r.body["model"], "m-first");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_ignores_model() {
    let h = Harness::new().await;
    let a = user_a();
    let id = h.create_chat(&a).await;
    let r = h
        .req(
            "PATCH",
            &format!("{CHATS}/{id}"),
            &a,
            Some(json!({"title": "Renamed", "model": "gpt-standard"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["title"], "Renamed");
    assert_eq!(r.body["model"], "gpt-premium", "a chat's model is immutable");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_orders_by_recent_activity() {
    let h = Harness::new().await;
    let a = user_a();
    let c1 = h.create_chat(&a).await;
    let c2 = h.create_chat(&a).await;
    let c3 = h.create_chat(&a).await;
    let ids = |r: &Resp| -> Vec<String> {
        r.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let r = h.get(CHATS, &a).await;
    assert_eq!(ids(&r), vec![c3.to_string(), c2.to_string(), c1.to_string()]);
    // Activity (a message) moves the oldest chat to the top.
    let s = h.send(&a, c1, "hi").await;
    assert_eq!(s.names().last(), Some(&"done"));
    let r = h.get(CHATS, &a).await;
    assert_eq!(ids(&r)[0], c1.to_string());
    // Rename bumps too.
    h.req("PATCH", &format!("{CHATS}/{c2}"), &a, Some(json!({"title": "t"})))
        .await;
    let r = h.get(CHATS, &a).await;
    assert_eq!(ids(&r)[0], c2.to_string());
    assert_eq!(r.body["items"][1]["message_count"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_pagination_filter_orderby() {
    let h = Harness::new().await;
    let a = user_a();
    let mut all = Vec::new();
    for i in 0..5 {
        let r = h.create_chat_with(&a, json!({"title": format!("chat-{i}")})).await;
        all.push(r.body["id"].as_str().unwrap().to_owned());
    }
    let p1 = h.get(&format!("{CHATS}?limit=2"), &a).await;
    assert_eq!(p1.status, StatusCode::OK);
    assert_eq!(p1.body["items"].as_array().unwrap().len(), 2);
    assert_eq!(p1.body["page_info"]["limit"], 2);
    let cursor = p1.body["page_info"]["next_cursor"].as_str().expect("next cursor").to_owned();
    let p2 = h.get(&format!("{CHATS}?limit=2&cursor={cursor}"), &a).await;
    assert_eq!(p2.status, StatusCode::OK, "{}", p2.text);
    let p3c = p2.body["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let p3 = h.get(&format!("{CHATS}?limit=2&cursor={p3c}"), &a).await;
    let mut seen: Vec<String> = [&p1, &p2, &p3]
        .iter()
        .flat_map(|p| p.body["items"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(seen.len(), 5);
    seen.sort();
    let mut expected = all.clone();
    expected.sort();
    assert_eq!(seen, expected);

    // $filter
    let f = h
        .get(&format!("{CHATS}?$filter=title%20eq%20'chat-3'"), &a)
        .await;
    assert_eq!(f.status, StatusCode::OK, "{}", f.text);
    assert_eq!(f.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(f.body["items"][0]["title"], "chat-3");
    let f = h
        .get(&format!("{CHATS}?$filter=contains(title,'chat')"), &a)
        .await;
    assert_eq!(f.body["items"].as_array().unwrap().len(), 5);

    // $orderby
    let o = h.get(&format!("{CHATS}?$orderby=title%20asc"), &a).await;
    assert_eq!(o.status, StatusCode::OK, "{}", o.text);
    assert_eq!(o.body["items"][0]["title"], "chat-0");
    assert_eq!(o.body["items"][4]["title"], "chat-4");

    // limit above 100 is clamped
    let c = h.get(&format!("{CHATS}?limit=1000"), &a).await;
    assert_eq!(c.status, StatusCode::OK);
    assert_eq!(c.body["page_info"]["limit"], 100);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_rejects_malformed_queries() {
    let h = Harness::new().await;
    let a = user_a();
    h.create_chat(&a).await;
    let cases = [
        ("$filter=unknown%20eq%201", "INVALID_FILTER"),
        ("$filter=title%20eq", "INVALID_FILTER"),
        ("$orderby=nope%20asc", "INVALID_ORDERBY_FIELD"),
        ("limit=0", "INVALID_LIMIT"),
        ("cursor=not-a-cursor", "INVALID_CURSOR"),
        ("$skip=1", "UNSUPPORTED_QUERY_PARAM"),
    ];
    for (q, reason) in cases {
        let r = h.get(&format!("{CHATS}?{q}"), &a).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}: {}", r.text);
        assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.odata.query.v1~", "{q}");
        let reasons: Vec<&str> = r.body["context"]["field_violations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["reason"].as_str())
            .collect();
        assert!(reasons.contains(&reason), "{q}: {reasons:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_chats_are_hidden_from_list_and_operations() {
    let h = Harness::new().await;
    let a = user_a();
    let id = h.create_chat(&a).await;
    h.req("DELETE", &format!("{CHATS}/{id}"), &a, None).await;
    let r = h.get(CHATS, &a).await;
    assert!(r.body["items"].as_array().unwrap().is_empty());
    for (m, uri) in [
        ("PATCH", format!("{CHATS}/{id}")),
        ("GET", format!("{CHATS}/{id}/messages")),
    ] {
        let body = (m == "PATCH").then(|| json!({"title": "x"}));
        let r = h.req(m, &uri, &a, body).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {uri}");
    }
    let s = h.send(&a, id, "hello").await;
    assert_eq!(s.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_chat_enqueues_cleanup_and_marks_attachments() {
    let h = Harness::new().await;
    let a = user_a();
    let id = h.create_chat(&a).await;
    let up = h.upload(&a, id, "doc.pdf", "application/pdf", b"%PDF-1.4 hello").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.text);
    let d = h.req("DELETE", &format!("{CHATS}/{id}"), &a, None).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    h.drain_outbox().await;
    h.eventually("vector store deleted", || async {
        let n = h
            .gw
            .calls("/vector_stores/")
            .iter()
            .filter(|r| r.method == "DELETE")
            .count();
        (n == 1).then_some(())
    })
    .await;
    let deletes = h
        .gw
        .calls("/files/")
        .iter()
        .filter(|r| r.method == "DELETE" && !r.uri.contains("vector_stores"))
        .count();
    assert_eq!(deletes, 1, "provider file deleted");
    let rows = h
        .rows(&format!("SELECT cleanup_status FROM attachments WHERE chat_id = {}", blob(id)))
        .await;
    assert_eq!(rows[0][0].as_deref(), Some("done"));
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM chat_vector_stores WHERE chat_id = {}", blob(id)))
            .await,
        0
    );
}
