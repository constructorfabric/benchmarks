//! Turn mutations and the turn lifecycle.

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

fn rid_of(r: &Resp) -> Uuid {
    Uuid::parse_str(r.event("stream_started").unwrap()["request_id"].as_str().unwrap()).unwrap()
}

/// Retry / edit / delete act only on the latest terminal turn.
#[tokio::test]
async fn mutations_only_on_latest_terminal_turn() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let first = rid_of(&h.send_message(U1, chat, json!({"content": "one"})).await);
    let second = rid_of(&h.send_message(U1, chat, json!({"content": "two"})).await);
    for (m, p, body) in [
        ("POST", format!("/chats/{chat}/turns/{first}/retry"), None),
        ("PATCH", format!("/chats/{chat}/turns/{first}"), Some(json!({"content": "edited"}))),
        ("DELETE", format!("/chats/{chat}/turns/{first}"), None),
    ] {
        let r = h.call(U1, m, &p, body).await;
        assert_eq!(r.status, 409, "{m} {p}: {}", r.text());
        assert_eq!(r.reason(), "NOT_LATEST_TURN");
    }
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{}/retry", Uuid::new_v4()), None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.turn.v1~");
    let r = h.call(U1, "PATCH", &format!("/chats/{chat}/turns/{second}"), Some(json!({"content": "  "}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "EMPTY_CONTENT");
    // Turn status endpoint.
    let t = h.call(U1, "GET", &format!("/chats/{chat}/turns/{second}"), None).await;
    assert_eq!(t.status, 200);
    assert_eq!(t.json()["state"], "done", "completed is exposed as done");
    assert!(t.json()["assistant_message_id"].is_string());
    // Delete latest, then the previous one becomes latest.
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{second}"), None).await.status, 204);
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}/turns/{second}"), None).await.status, 404);
    let r = h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{second}"), None).await;
    assert_eq!(r.status, 409, "deleted turn is not the latest: {}", r.text());
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{first}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // A failed latest turn can be retried.
    h.provider.push(Script::Http(500, json!({"error": {"message": "x"}}), vec![]));
    let failed = rid_of(&h.send_message(U1, chat, json!({"content": "boom"})).await);
    let t = h.call(U1, "GET", &format!("/chats/{chat}/turns/{failed}"), None).await.json();
    assert_eq!(t["state"], "error");
    assert_eq!(t["error_code"], "provider_error");
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{failed}/retry"), None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.event_names().last().unwrap(), "done");
}

/// A mutation goes through the full send pipeline and gets a new request id.
#[tokio::test]
async fn mutation_runs_full_pipeline_with_new_request_id() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("tiny")).await;
    let original = rid_of(&h.send_message(U1, chat, json!({"content": "question"})).await);
    h.provider.push(completed(&["Retried"], 10, 5));
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{original}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let started = r.event("stream_started").unwrap();
    let retried = rid_of(&r);
    assert_ne!(retried, original);
    assert_eq!(started["is_new_turn"], true);
    let old = h.turn(chat, original).await;
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(retried));
    // The retried request repeats the original user content.
    let req = h.provider.chat_requests().pop().unwrap();
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["content"][0]["text"], "question");
    let visible = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert_eq!(visible.as_array().unwrap().len(), 2);
    assert_eq!(visible[1]["content"], "Retried");
    assert_eq!(visible[0]["request_id"], retried.to_string());

    // Edit: new content, context budget enforced.
    let r = h.call(U1, "PATCH", &format!("/chats/{chat}/turns/{retried}"), Some(json!({"content": "x".repeat(20_000)}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "INPUT_TOO_LONG");
    assert!(h.turn(chat, retried).await.deleted_at.is_none(), "rejected edit leaves the turn untouched");
    let r = h.call(U1, "PATCH", &format!("/chats/{chat}/turns/{retried}"), Some(json!({"content": "better question"}))).await;
    assert_eq!(r.status, 200);
    let edited = rid_of(&r);
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(req["input"].as_array().unwrap().last().unwrap()["content"][0]["text"], "better question");

    // Audit events for mutations.
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{edited}"), None).await.status, 204);
    let events = h.wait_audit(7).await;
    let kinds: Vec<String> = events.iter().map(|e| e.event_type().to_owned()).collect();
    for k in ["turn_retry", "turn_edit", "turn_delete"] {
        assert!(kinds.contains(&k.to_owned()), "{kinds:?}");
    }

    // Quota is checked: an exhausted user cannot retry.
    let h2 = Harness::with(Options { standard_limits: (2_000_000, 100_000_000), ..Options::default() }).await;
    let chat2 = h2.create_chat(U1, Some("standard-1")).await;
    let rid = rid_of(&h2.send_message(U1, chat2, json!({"content": "q"})).await);
    h2.seed_spent(TENANT_A, USER_1, "total", "daily", 2_000_000).await;
    let calls = h2.provider.chat_requests().len();
    let r = h2.call(U1, "POST", &format!("/chats/{chat2}/turns/{rid}/retry"), None).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(h2.provider.chat_requests().len(), calls);
    assert!(h2.turn(chat2, rid).await.deleted_at.is_none());
}

/// Concurrent mutations resolve deterministically: exactly one wins.
#[tokio::test]
async fn concurrent_mutations_resolve_deterministically() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let rid = rid_of(&h.send_message(U1, chat, json!({"content": "q"})).await);
    let retry = format!("/chats/{chat}/turns/{rid}/retry");
    let edit = format!("/chats/{chat}/turns/{rid}");
    let (a, b, c) = tokio::join!(
        h.call(U1, "POST", &retry, None),
        h.call(U1, "PATCH", &edit, Some(json!({"content": "e"}))),
        h.call(U1, "DELETE", &edit, None)
    );
    let statuses = [a.status, b.status, c.status];
    let winners = statuses.iter().filter(|s| **s == 200 || **s == 204).count();
    assert_eq!(winners, 1, "{statuses:?}: {} {} {}", a.text(), b.text(), c.text());
    for r in [&a, &b, &c] {
        if r.status == 409 {
            assert!(["NOT_LATEST_TURN", "GENERATION_IN_PROGRESS"].contains(&r.reason().as_str()), "{}", r.text());
        } else if r.status != 200 && r.status != 204 {
            panic!("unexpected {}: {}", r.status, r.text());
        }
    }
    let live: Vec<_> = h.turns(chat).await.into_iter().filter(|t| t.deleted_at.is_none()).collect();
    assert!(live.len() <= 1);
}

/// Mutated turns carry forward attachments and tool usage of the original turn.
#[tokio::test]
async fn mutations_carry_forward_attachments_and_tools() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let doc = h.upload(U1, chat, "q3.txt", "text/plain", b"report").await.json()["id"].as_str().unwrap().to_owned();
    let png = tiny_png();
    let img = h.upload(U1, chat, "p.png", "image/png", &png).await;
    assert_eq!(img.status, 201, "{}", img.text());
    let img = img.json()["id"].as_str().unwrap().to_owned();
    let rid = rid_of(
        &h.send_message(U1, chat, json!({"content": "analyze", "attachment_ids": [doc, img], "web_search": {"enabled": true}})).await,
    );
    let first = h.provider.chat_requests().pop().unwrap();
    let r = h.call(U1, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let retried = rid_of(&r);
    let req = h.provider.chat_requests().pop().unwrap();
    let tools = |v: &serde_json::Value| -> Vec<String> {
        v["tools"].as_array().map(|t| t.iter().map(|x| x["type"].as_str().unwrap().to_owned()).collect()).unwrap_or_default()
    };
    assert!(tools(&req).contains(&"web_search".to_owned()), "{req}");
    assert!(tools(&req).contains(&"file_search".to_owned()));
    assert_eq!(tools(&req), tools(&first));
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "input_image"), "{last}");
    assert!(h.turn(chat, retried).await.web_search_enabled);
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    let ids: Vec<&str> = msgs[0]["attachments"].as_array().unwrap().iter().map(|a| a["attachment_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&doc.as_str()) && ids.contains(&img.as_str()), "{msgs}");
    // Edit carries them forward too.
    let r = h.call(U1, "PATCH", &format!("/chats/{chat}/turns/{retried}"), Some(json!({"content": "again"}))).await;
    assert_eq!(r.status, 200);
    let msgs = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert_eq!(msgs[0]["content"], "again");
    assert_eq!(msgs[0]["attachments"].as_array().unwrap().len(), 2);
    // Referenced attachments stay locked after mutations.
    let r = h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{img}"), None).await;
    assert_eq!(r.status, 409);
}

/// Turn state machine is consistent end to end; CAS loser gets stream_interrupted.
#[tokio::test]
async fn turn_state_machine_consistency() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // running -> completed
    let (tx, _d) = h.provider.push_channel();
    let rid = Uuid::new_v4();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "a", "request_id": rid}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    let t = h.call(U1, "GET", &format!("/chats/{chat}/turns/{rid}"), None).await.json();
    assert_eq!(t["state"], "running");
    assert!(t["assistant_message_id"].is_null());
    tx.send(frame("response.output_text.delta", &json!({"delta": "ok"})).into()).unwrap();
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "done").await);
    let turn = h.turn(chat, rid).await;
    assert_eq!(turn.state, "completed");
    assert!(turn.completed_at.is_some());

    // CAS loser: the watchdog finalizes a stale running turn first.
    let (tx, _d) = h.provider.push_channel();
    let rid = Uuid::new_v4();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "b", "request_id": rid}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    tx.send(frame("response.output_text.delta", &json!({"delta": "partial"})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "delta").await);
    let turn = h.turn(chat, rid).await;
    h.age_turn(turn.id, 400).await;
    assert_eq!(h.app.orphan_scan().await.unwrap(), 1);
    let turn = h.turn(chat, rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    tx.send(frame("response.completed", &json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}})).into()).unwrap();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "error").await);
    let events = parse_sse(&buf);
    let (name, err) = events.last().unwrap();
    assert_eq!(name, "error");
    assert_eq!(err["code"], "stream_interrupted");
    assert!(!events.iter().any(|(n, _)| n == "done"));
    let turn = h.turn(chat, rid).await;
    assert_eq!(turn.state, "failed", "the loser does not overwrite the winner");
    // Settled exactly once: two turns, two usage events.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let published = h.wait_published(2).await;
    assert_eq!(published.len(), 2);
    assert_eq!(published[1].terminal_state, "failed");
    assert_eq!(published[1].billing_outcome, "aborted");
    assert_eq!(published[1].settlement_method, "estimated");
    // A new turn is accepted after the orphan finalization.
    assert_eq!(h.send_message(U1, chat, json!({"content": "c"})).await.status, 200);
}

/// Partial and null-content cases on cancellation or failure.
#[tokio::test]
async fn partial_and_null_content_on_cancel_or_failure() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let wait_terminal = |rid: Uuid| {
        let h = &h;
        async move {
            for _ in 0..250 {
                let t = h.turn(chat, rid).await;
                if t.state != "running" {
                    return t;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("turn still running");
        }
    };
    // Cancel after partial content: partial message persisted.
    let (tx, dropped) = h.provider.push_channel();
    let rid = Uuid::new_v4();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "a", "request_id": rid}))).await;
    tx.send(frame("response.output_text.delta", &json!({"delta": "Partial ans"})).into()).unwrap();
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "delta").await);
    drop(body);
    let t = wait_terminal(rid).await;
    assert_eq!(t.state, "cancelled");
    let msg_id = t.assistant_message_id.expect("partial message");
    let msg = h.messages(chat).await.into_iter().find(|m| m.id == msg_id).unwrap();
    assert_eq!(msg.content, "Partial ans");
    assert!(h.wait_until(|| dropped.load(std::sync::atomic::Ordering::SeqCst)).await, "provider request aborted");
    let api = h.call(U1, "GET", &format!("/chats/{chat}/turns/{rid}"), None).await.json();
    assert_eq!(api["state"], "cancelled");

    // Cancel before any content: no assistant message.
    let (_tx, _d) = h.provider.push_channel();
    let rid = Uuid::new_v4();
    let (_, mut body) = h.open(U1, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "b", "request_id": rid}))).await;
    let mut buf = String::new();
    assert!(read_until(&mut body, &mut buf, |n, _| n == "stream_started").await);
    drop(body);
    let t = wait_terminal(rid).await;
    assert_eq!(t.state, "cancelled");
    assert!(t.assistant_message_id.is_none());
    assert_eq!(h.messages(chat).await.iter().filter(|m| m.request_id == Some(rid)).count(), 1, "only the user message");

    // Failure after partial content: no assistant message, error code recorded.
    h.provider.push(Script::Sse(vec![
        ("response.output_text.delta".into(), json!({"delta": "half"})),
        ("error".into(), json!({"error": {"message": "overloaded"}})),
    ]));
    let rid = Uuid::new_v4();
    let r = h.send_message(U1, chat, json!({"content": "c", "request_id": rid})).await;
    assert_eq!(r.event_names().last().unwrap(), "error");
    let t = h.turn(chat, rid).await;
    assert_eq!(t.state, "failed");
    assert!(t.assistant_message_id.is_none());
    assert_eq!(t.error_code.as_deref(), Some("provider_error"));
    let visible = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert!(visible.as_array().unwrap().iter().all(|m| m["content"] != "half"));
    // Settlement: cancelled and failed turns are billed via estimates.
    let published = h.wait_published(3).await;
    let states: Vec<&str> = published.iter().map(|p| p.terminal_state.as_str()).collect();
    assert_eq!(states.iter().filter(|s| **s == "cancelled").count(), 2);
    assert_eq!(states.iter().filter(|s| **s == "failed").count(), 1);
    assert!(published.iter().all(|p| p.settlement_method == "estimated"));
    let q = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(q.reserved_credits_micro, 0);
}

/// 1x1 PNG.
pub fn tiny_png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}
