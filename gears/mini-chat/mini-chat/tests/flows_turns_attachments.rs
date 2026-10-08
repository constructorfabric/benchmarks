//! Turn mutations, attachments, cleanup, thread summary and the background
//! workers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::*;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::events::ThreadSummaryTask;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use toolkit_odata::ODataQuery;
use uuid::Uuid;

fn req(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: None,
        attachment_ids: Vec::new(),
        web_search: false,
    }
}

fn rid_of(ev: &[StreamEvent]) -> Uuid {
    match &ev[0] {
        StreamEvent::Started { request_id, .. } => *request_id,
        other => panic!("{other:?}"),
    }
}

async fn wait_rows(
    h: &Harness,
    sql: &str,
    pred: impl Fn(&[serde_json::Value]) -> bool,
) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = h.rows(sql).await;
        if pred(&rows) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    rows
}

#[tokio::test]
async fn retry_edit_delete_latest_turn() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    let r1 =
        rid_of(&Harness::collect(h.svc.send_message(&a, chat.id, req("one")).await.unwrap()).await);
    let r2 =
        rid_of(&Harness::collect(h.svc.send_message(&a, chat.id, req("two")).await.unwrap()).await);
    // only the latest turn
    for res in [
        h.svc.regenerate_turn(&a, chat.id, r1, None).await.err(),
        h.svc
            .regenerate_turn(&a, chat.id, r1, Some("x".into()))
            .await
            .err(),
    ] {
        assert!(matches!(res.unwrap(), DomainError::NotLatestTurn));
    }
    assert!(matches!(
        h.svc.delete_turn(&a, chat.id, r1).await.unwrap_err(),
        DomainError::NotLatestTurn
    ));
    // retry: new request id, old turn replaced
    let ev = Harness::collect(h.svc.regenerate_turn(&a, chat.id, r2, None).await.unwrap()).await;
    let r3 = rid_of(&ev);
    assert_ne!(r3, r2);
    assert!(matches!(ev.last(), Some(StreamEvent::Done(_))));
    let old = h
        .rows(&format!(
            "select * from chat_turns where hex(request_id) = upper('{}')",
            uhex(r2)
        ))
        .await
        .remove(0);
    assert!(old["deleted_at"].is_string());
    assert_eq!(old["replaced_by_request_id"], uhex(r3));
    let body = h.gw.requests_to("/responses").last().unwrap().json();
    let texts: Vec<_> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"][0]["text"].clone())
        .collect();
    assert_eq!(
        texts,
        vec![
            serde_json::json!("one"),
            serde_json::json!("Hello"),
            serde_json::json!("two")
        ]
    );
    // edit replaces the content
    let ev = Harness::collect(
        h.svc
            .regenerate_turn(&a, chat.id, r3, Some("edited".into()))
            .await
            .unwrap(),
    )
    .await;
    let r4 = rid_of(&ev);
    let page = h
        .svc
        .list_messages(&a, chat.id, &ODataQuery::default())
        .await
        .unwrap();
    let users: Vec<_> = page
        .items
        .iter()
        .filter(|m| m.message.role == "user")
        .map(|m| m.message.content.clone())
        .collect();
    assert_eq!(users, ["one", "edited"]);
    assert!(matches!(
        h.svc
            .regenerate_turn(&a, chat.id, r4, Some(" ".into()))
            .await
            .err()
            .unwrap(),
        DomainError::EmptyContent
    ));
    // delete
    h.svc.delete_turn(&a, chat.id, r4).await.unwrap();
    assert!(matches!(
        h.svc.get_turn(&a, chat.id, r4).await.unwrap_err(),
        DomainError::TurnNotFound { .. }
    ));
    assert!(matches!(
        h.svc.delete_turn(&a, chat.id, r4).await.unwrap_err(),
        DomainError::NotLatestTurn
    ));
    assert_eq!(h.svc.get_chat(&a, chat.id).await.unwrap().message_count, 2);
    // running turns cannot be mutated
    h.gw.push(Reply::Hang);
    let StreamStart::Live(mut live) = h.svc.send_message(&a, chat.id, req("slow")).await.unwrap()
    else {
        panic!("live")
    };
    let r5 = match live.events.recv().await.unwrap() {
        StreamEvent::Started { request_id, .. } => request_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        h.svc.delete_turn(&a, chat.id, r5).await.unwrap_err(),
        DomainError::TurnNotTerminal
    ));
    assert_eq!(
        h.svc.get_turn(&a, chat.id, r5).await.unwrap().state,
        "running"
    );
    live.cancel.cancel();
    h.stop().await;
}

#[tokio::test]
async fn edit_setup_failure_marks_new_turn_failed() {
    let h = Harness::new().await;
    {
        let mut tiny = model("tiny", "standard", true, false);
        tiny.context_window = 2600;
        tiny.max_output_tokens = 1024;
        tiny.max_input_tokens = 1500;
        h.policy.snapshot.lock().model_catalog.push(tiny);
    }
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("tiny".into()))
        .await
        .unwrap()
        .chat;
    let r1 = rid_of(
        &Harness::collect(h.svc.send_message(&a, chat.id, req("short")).await.unwrap()).await,
    );
    let e = h
        .svc
        .regenerate_turn(&a, chat.id, r1, Some("x".repeat(5000)))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, DomainError::ContextBudgetExceeded));
    let rows = h
        .rows("select state, error_code, deleted_at from chat_turns order by started_at")
        .await;
    assert_eq!(rows.len(), 2);
    assert!(rows[0]["deleted_at"].is_string());
    assert_eq!(rows[1]["state"], "failed");
    assert_eq!(rows[1]["error_code"], "context_length_exceeded");
    h.stop().await;
}

#[tokio::test]
async fn attachments_upload_get_delete_and_cleanup() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    let plan = h.svc.plan_upload(&t, "text/plain; charset=utf-8").unwrap();
    assert!(plan.for_file_search && !plan.for_code_interpreter);
    let doc = h
        .svc
        .store_upload(
            &a,
            t,
            plan,
            "notes.txt".into(),
            Bytes::from_static(b"hello"),
        )
        .await
        .unwrap();
    assert_eq!(doc.status, "ready");
    assert!(
        doc.provider_file_id
            .as_deref()
            .unwrap()
            .starts_with("file-")
    );
    assert_eq!(
        h.rows("select count(*) n from chat_vector_stores").await[0]["n"],
        1
    );
    // unsupported / xlsx rules
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    assert!(matches!(
        h.svc
            .plan_upload(&t, "application/octet-stream")
            .unwrap_err(),
        DomainError::UnsupportedContentType { .. }
    ));
    h.policy
        .snapshot
        .lock()
        .kill_switches
        .disable_code_interpreter = true;
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    assert!(matches!(
        h.svc
            .plan_upload(&t, mini_chat::domain::mime::XLSX)
            .unwrap_err(),
        DomainError::CodeInterpreterUnavailable
    ));
    h.policy
        .snapshot
        .lock()
        .kill_switches
        .disable_code_interpreter = false;
    // file_search tool is sent with the vector store; citations resolve to the attachment
    let fid = doc.provider_file_id.clone().unwrap();
    h.gw.push(Reply::Events(vec![
        serde_json::json!({"type": "response.output_text.delta", "item_id": "m", "delta": "doc"}),
        serde_json::json!({"type": "response.output_text.annotation.added", "item_id": "m",
                           "annotation": {"type": "file_citation", "file_id": fid}}),
        serde_json::json!({"type": "response.completed", "response": {}}),
    ]));
    let mut r = req("about the doc");
    r.attachment_ids = vec![doc.id];
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, r).await.unwrap()).await;
    let body = h.gw.requests_to("/responses").last().unwrap().json();
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "file_search")
    );
    let cits = ev.iter().find_map(|e| match e {
        StreamEvent::Citations(c) => Some(c.clone()),
        _ => None,
    });
    let cits = cits.unwrap();
    assert_eq!(cits[0].attachment_id, Some(doc.id));
    assert_eq!(cits[0].title, "notes.txt");
    // referenced attachment is locked
    assert!(matches!(
        h.svc
            .delete_attachment(&a, chat.id, doc.id)
            .await
            .unwrap_err(),
        DomainError::AttachmentLocked
    ));
    // an unreferenced attachment is deleted and cleaned up asynchronously
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    let plan = h.svc.plan_upload(&t, "text/markdown").unwrap();
    let doc2 = h
        .svc
        .store_upload(&a, t, plan, "b.md".into(), Bytes::from_static(b"#"))
        .await
        .unwrap();
    h.svc.delete_attachment(&a, chat.id, doc2.id).await.unwrap();
    h.svc.delete_attachment(&a, chat.id, doc2.id).await.unwrap();
    assert!(matches!(
        h.svc
            .get_attachment(&a, chat.id, doc2.id)
            .await
            .unwrap_err(),
        DomainError::AttachmentNotFound { .. }
    ));
    let rows = wait_rows(
        &h,
        &format!(
            "select cleanup_status from attachments where hex(id) = upper('{}')",
            uhex(doc2.id)
        ),
        |r| r[0]["cleanup_status"] == "done",
    )
    .await;
    assert_eq!(rows[0]["cleanup_status"], "done");
    let fid2 = doc2.provider_file_id.unwrap();
    assert!(
        h.gw.requests
            .lock()
            .iter()
            .any(|r| r.method == "DELETE" && r.uri.ends_with(&fid2))
    );
    // chat deletion removes provider files and the vector store
    h.svc.delete_chat(&a, chat.id).await.unwrap();
    wait_rows(&h, "select count(*) n from chat_vector_stores", |r| {
        r[0]["n"] == 0
    })
    .await;
    assert_eq!(
        h.rows("select count(*) n from chat_vector_stores").await[0]["n"],
        0
    );
    let pending = h
        .rows("select count(*) n from attachments where cleanup_status = 'pending'")
        .await;
    assert_eq!(pending[0]["n"], 0);
    h.stop().await;
}

#[tokio::test]
async fn indexing_failure_marks_attachment_failed() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    *h.gw.index_status.lock() = "failed".into();
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    let plan = h.svc.plan_upload(&t, "application/pdf").unwrap();
    let e = h
        .svc
        .store_upload(&a, t, plan, "x.pdf".into(), Bytes::from_static(b"%PDF"))
        .await
        .unwrap_err();
    assert!(matches!(e, DomainError::StorageUnavailable { .. }));
    let rows = h.rows("select status, error_code from attachments").await;
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(rows[0]["error_code"], "indexing_failed");
    h.stop().await;
}

#[tokio::test]
async fn orphan_watchdog_and_upload_reaper() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Hang);
    let StreamStart::Live(mut live) = h.svc.send_message(&a, chat.id, req("stuck")).await.unwrap()
    else {
        panic!("live")
    };
    let rid = match live.events.recv().await.unwrap() {
        StreamEvent::Started { request_id, .. } => request_id,
        other => panic!("{other:?}"),
    };
    // fresh turns are left alone
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 0);
    h.exec("update chat_turns set last_progress_at = '2020-01-01T00:00:00.000000001Z'")
        .await;
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 1);
    assert_eq!(h.svc.orphan_scan().await.unwrap(), 0);
    let st = h.svc.get_turn(&a, chat.id, rid).await.unwrap();
    assert_eq!(st.state, "error");
    assert_eq!(st.error_code.as_deref(), Some("orphan_timeout"));
    let published = h.wait_published(1).await;
    assert_eq!(published[0].billing_outcome, "aborted");
    assert_eq!(published[0].terminal_state, "failed");
    let q = h.rows("select reserved_credits_micro r, spent_credits_micro s from quota_usage where bucket = 'total'").await;
    assert!(
        q.iter()
            .all(|r| r["r"] == 0 && r["s"].as_i64().unwrap() > 0)
    );
    live.cancel.cancel();
    // upload reaper
    let t = h.svc.prepare_upload(&a, chat.id).await.unwrap();
    let plan = h.svc.plan_upload(&t, "text/plain").unwrap();
    let doc = h
        .svc
        .store_upload(&a, t, plan, "r.txt".into(), Bytes::from_static(b"x"))
        .await
        .unwrap();
    h.exec(
        "update attachments set status = 'uploaded', updated_at = '2020-01-01T00:00:00.000000001Z'",
    )
    .await;
    assert_eq!(h.svc.reaper_scan().await.unwrap(), 1);
    let row = h.svc.get_attachment(&a, chat.id, doc.id).await.unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    h.stop().await;
}

#[tokio::test]
async fn thread_summary_commit_and_invalidation() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    let mut rids = Vec::new();
    for i in 0..3 {
        rids.push(rid_of(
            &Harness::collect(
                h.svc
                    .send_message(&a, chat.id, req(&format!("m{i}")))
                    .await
                    .unwrap(),
            )
            .await,
        ));
    }
    // summary task up to the last message of the second turn
    let msgs = h
        .svc
        .list_messages(&a, chat.id, &ODataQuery::default())
        .await
        .unwrap()
        .items;
    let target = &msgs[3].message;
    let task = ThreadSummaryTask {
        tenant_id: TENANT_A,
        chat_id: chat.id,
        system_request_id: Uuid::new_v4(),
        system_task_type: "thread_summary_update".into(),
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
    };
    let msg = toolkit_db::outbox::OutboxMessage {
        partition_id: 0,
        seq: 0,
        payload: serde_json::to_vec(&task).unwrap(),
        payload_type: mini_chat::domain::events::PAYLOAD_THREAD_SUMMARY.into(),
        created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        attempts: 0,
    };
    let handler = mini_chat::domain::service::summary::ThreadSummaryHandler(h.svc.clone());
    let r = mini_chat::infra::outbox::QueueHandler::handle(&handler, &msg).await;
    assert!(matches!(r, toolkit_db::outbox::MessageResult::Ok));
    let s = h
        .rows("select summary_text, token_estimate from thread_summaries")
        .await;
    assert_eq!(s[0]["summary_text"], "The summary.");
    assert_eq!(s[0]["token_estimate"], 7);
    assert_eq!(
        h.rows("select count(*) n from messages where is_compressed = 1")
            .await[0]["n"],
        4
    );
    // replaying the same task is a CAS no-op
    let r = mini_chat::infra::outbox::QueueHandler::handle(&handler, &msg).await;
    assert!(matches!(r, toolkit_db::outbox::MessageResult::Ok));
    let summaries =
        h.gw.requests_to("/responses")
            .iter()
            .filter(|r| r.json()["stream"] == false)
            .count();
    assert_eq!(summaries, 1);
    // the next turn sends the summary and only the uncompressed history
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, req("next")).await.unwrap()).await;
    assert!(matches!(
        &ev[0],
        StreamEvent::Started {
            thread_summary_applied: Some(7),
            ..
        }
    ));
    let body = h.gw.requests_to("/responses").last().unwrap().json();
    let first = body["input"][0]["content"][0]["text"].as_str().unwrap();
    assert!(first.contains("summarized") && first.contains("The summary."));
    // deleting turns back into the summarized range drops the summary
    let next = rid_of(&ev);
    h.svc.delete_turn(&a, chat.id, next).await.unwrap();
    h.svc.delete_turn(&a, chat.id, rids[2]).await.unwrap();
    assert_eq!(
        h.rows("select count(*) n from thread_summaries").await[0]["n"],
        1
    );
    h.svc.delete_turn(&a, chat.id, rids[1]).await.unwrap();
    assert_eq!(
        h.rows("select count(*) n from thread_summaries").await[0]["n"],
        0
    );
    assert_eq!(
        h.rows("select count(*) n from messages where is_compressed = 1")
            .await[0]["n"],
        0
    );
    h.stop().await;
}

#[tokio::test]
async fn quota_status_reports_usage() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Events(text_events("x", 1000, 0)));
    Harness::collect(h.svc.send_message(&a, chat.id, req("x")).await.unwrap()).await;
    let tiers = h.svc.quota_status(&a).await.unwrap();
    assert_eq!(tiers[0].tier, "premium");
    assert_eq!(tiers[1].tier, "total");
    let daily = &tiers[1].periods[0];
    assert_eq!(daily.period, "daily");
    assert_eq!(daily.limit, 100_000_000);
    assert_eq!(daily.used, 1000);
    assert_eq!(daily.remaining_percentage, 99);
    let other = h.svc.quota_status(&ctx(TENANT_A, USER_A2)).await.unwrap();
    assert_eq!(other[1].periods[0].used, 0);
    h.stop().await;
}
