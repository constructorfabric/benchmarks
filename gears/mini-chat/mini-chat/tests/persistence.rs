//! Schema, constraints, CAS finalization and streaming principles.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn schema_has_design_tables_and_columns() {
    let h = Harness::new().await;
    for (table, cols) in [
        ("chats", vec!["id", "tenant_id", "user_id", "model", "title", "is_temporary", "created_at", "updated_at", "deleted_at"]),
        ("messages", vec!["request_id", "role", "content", "content_type", "token_estimate", "provider_response_id", "request_kind", "features_used", "input_tokens", "output_tokens", "cache_read_input_tokens", "cache_write_input_tokens", "reasoning_tokens", "model", "is_compressed"]),
        ("chat_turns", vec!["request_id", "requester_type", "requester_user_id", "state", "provider_name", "provider_response_id", "assistant_message_id", "error_code", "reserve_tokens", "max_output_tokens_applied", "reserved_credits_micro", "policy_version_applied", "effective_model", "minimal_generation_floor_applied", "error_detail", "deleted_at", "replaced_by_request_id", "started_at", "last_progress_at", "web_search_enabled", "web_search_completed_count", "code_interpreter_completed_count", "file_search_completed_count", "completed_at", "updated_at"]),
        ("attachments", vec!["uploaded_by_user_id", "filename", "content_type", "size_bytes", "storage_backend", "provider_file_id", "status", "error_code", "attachment_kind", "for_file_search", "for_code_interpreter", "doc_summary", "img_thumbnail", "img_thumbnail_width", "img_thumbnail_height", "summary_model", "summary_updated_at", "cleanup_status", "cleanup_attempts", "last_cleanup_error", "cleanup_updated_at", "secondary_file_id", "secondary_status", "secondary_provider_kind"]),
        ("message_attachments", vec!["tenant_id", "chat_id", "message_id", "attachment_id", "created_at"]),
        ("thread_summaries", vec!["summary_text", "summarized_up_to_created_at", "summarized_up_to_message_id", "token_estimate"]),
        ("chat_vector_stores", vec!["vector_store_id", "provider", "file_count"]),
        ("quota_usage", vec!["period_type", "period_start", "bucket", "spent_credits_micro", "reserved_credits_micro", "calls", "input_tokens", "output_tokens", "file_search_calls", "web_search_calls", "code_interpreter_calls", "rag_retrieval_calls", "image_inputs", "image_upload_bytes"]),
        ("message_reactions", vec!["message_id", "user_id", "tenant_id", "reaction", "created_at"]),
    ] {
        let rows = h.query(&format!("SELECT name FROM pragma_table_info('{table}')")).await;
        let names: Vec<String> = rows.iter().map(|r| r.try_get_by_index::<String>(0).unwrap()).collect();
        for c in cols {
            assert!(names.iter().any(|n| n == c), "{table}.{c} missing");
        }
    }
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_running_turn_per_chat_and_cas_finalization() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    h.provider.push(Reply::Hang(vec![delta("p")]));
    let start = h
        .svc
        .send_message(&u, chat, SendRequest { content: "x".into(), request_id: None, attachment_ids: vec![], web_search: false })
        .await
        .unwrap();
    let StreamStart::Live { mut events, cancel } = start else { panic!() };
    next_n(&mut events, 2).await;
    // a second running row for the same chat violates the partial unique index
    let insert = format!(
        "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, state, started_at, last_progress_at, updated_at) \
         VALUES (X'{}', X'{}', X'{}', X'{}', 'user', 'running', '2026-01-01', '2026-01-01', '2026-01-01')",
        hex_uuid(Uuid::new_v4()),
        hex_uuid(u.subject_tenant_id()),
        hex_uuid(chat),
        hex_uuid(Uuid::new_v4())
    );
    {
        use sea_orm::{ConnectionTrait, Database};
        let conn = Database::connect(format!("sqlite://{}", h.path.display())).await.unwrap();
        assert!(conn.execute_unprepared(&insert).await.is_err(), "UNIQUE(chat_id) WHERE running");
        assert!(conn.execute_unprepared("UPDATE chat_turns SET state = 'bogus'").await.is_err(), "state CHECK");
    }
    drop(cancel.drop_guard());
    drop(events);
    h.eventually(|| async { h.scalar_str("SELECT state FROM chat_turns").await.as_deref() == Some("cancelled") }).await;
    // CAS: a terminal turn is never finalized again (watchdog loses)
    h.exec("UPDATE chat_turns SET last_progress_at = '2000-01-01 00:00:00+00:00', started_at = '2000-01-01 00:00:00+00:00'").await;
    assert_eq!(mini_chat::infra::workers::orphan_scan(&h.svc).await.unwrap(), 0);
    assert_eq!(h.scalar_str("SELECT state FROM chat_turns").await.as_deref(), Some("cancelled"));
    h.eventually(|| async { h.policy.published.lock().len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.policy.published.lock().len(), 1, "settled exactly once");
    assert_eq!(h.scalar_i64("SELECT count(*) FROM chat_turns WHERE completed_at IS NULL AND state != 'running'").await, 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deltas_are_relayed_without_buffering() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    *h.provider.delay_ms.lock() = 400;
    h.provider.push(Reply::Events(vec![delta("a"), delta("b"), delta("c"), completed(1, 1)]));
    let start = h
        .svc
        .send_message(&u, chat, SendRequest { content: "x".into(), request_id: None, attachment_ids: vec![], web_search: false })
        .await
        .unwrap();
    let StreamStart::Live { mut events, cancel } = start else { panic!() };
    let _g = cancel.drop_guard();
    let t0 = Instant::now();
    let mut arrivals = Vec::new();
    while let Some(e) = events.recv().await {
        if let StreamEvent::Delta { content, .. } = &e {
            arrivals.push((content.clone(), t0.elapsed()));
        }
        if e.is_terminal() {
            break;
        }
    }
    assert_eq!(arrivals.len(), 3);
    // the first delta arrives well before the provider finishes (~1.6 s total)
    assert!(arrivals[0].1 < Duration::from_millis(1000), "{arrivals:?}");
    assert!(arrivals[2].1 - arrivals[0].1 >= Duration::from_millis(600), "{arrivals:?}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_stream_interrupted_when_task_ends_without_terminal() {
    // A CAS loser ends without a terminal event; the relay closes with
    // error{stream_interrupted} (ADR-0010).
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    h.provider.push(Reply::Events(vec![delta("x"), completed(1, 1)]));
    *h.provider.delay_ms.lock() = 300;
    let start = h
        .svc
        .send_message(&u, chat, SendRequest { content: "x".into(), request_id: None, attachment_ids: vec![], web_search: false })
        .await
        .unwrap();
    // the watchdog wins the CAS while the provider is still streaming
    h.exec("UPDATE chat_turns SET last_progress_at = '2000-01-01 00:00:00+00:00', started_at = '2000-01-01 00:00:00+00:00'").await;
    assert_eq!(mini_chat::infra::workers::orphan_scan(&h.svc).await.unwrap(), 1);
    let resp = mini_chat::api::rest::sse::sse_response(start);
    let text = body_text(resp.into_body()).await;
    assert!(text.contains("event: stream_started"));
    assert!(text.contains("stream_interrupted"), "{text}");
    assert!(!text.contains("event: done"));
    assert_eq!(h.scalar_str("SELECT error_code FROM chat_turns").await.as_deref(), Some("orphan_timeout"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_do_not_surface_lock_errors() {
    // Read-then-write transactions racing on SQLite must wait for the write
    // lock instead of failing with SQLITE_BUSY_SNAPSHOT (500).
    let h = Harness::new().await;
    let u = user();
    let mut chats = Vec::new();
    for _ in 0..12 {
        chats.push(h.create_chat(&u, json!({"model": "std"})).await);
    }
    let tasks: Vec<_> = chats
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (h, u, c) = (&h, &u, *c);
            async move {
                let a = h.call(u, "PATCH", &format!("/mini-chat/v1/chats/{c}"), Some(json!({"title": format!("t{i}")}))).await;
                let b = h.call(u, "DELETE", &format!("/mini-chat/v1/chats/{c}"), None).await;
                (a.status, b.status)
            }
        })
        .collect();
    for (patch, delete) in futures::future::join_all(tasks).await {
        assert_eq!((patch, delete), (200, 204));
    }
    h.shutdown().await;
}
