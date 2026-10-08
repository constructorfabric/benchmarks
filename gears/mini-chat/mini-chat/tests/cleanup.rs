//! US7: background reliability — outbox handlers, chat cleanup, thread summary.
//!
//! AC: Cleanup & Recovery (chat deletion cleanup, thread summary generation / failure /
//! invalidation), Settlement & Finalization (usage published reliably, exactly once).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use mini_chat::domain::ports::{AuditDelivery, AuditPort, PolicyPort};
use mini_chat::domain::service::cleanup::HandlerOutcome;
use mini_chat::infra::outbox::ThreadSummaryTask;
use mini_chat::infra::outbox_handlers::{QueueHandler, QueueKind};
use mini_chat::infra::policy::{AuditGateway, FixedSource, NoSource, PolicyGateway};
use mini_chat_sdk::{AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

async fn summaries(h: &Harness, chat: Uuid) -> Vec<ent::thread_summaries::Model> {
    ent::thread_summaries::Entity::find()
        .filter(ent::thread_summaries::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&h.db.conn().unwrap())
        .await
        .unwrap()
}

async fn vector_stores(h: &Harness, chat: Uuid) -> Vec<ent::chat_vector_stores::Model> {
    ent::chat_vector_stores::Entity::find()
        .filter(ent::chat_vector_stores::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&h.db.conn().unwrap())
        .await
        .unwrap()
}

async fn wait_for<F, Fut>(what: &str, secs: u64, f: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..(secs * 20) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_deletion_cleans_up_provider_files_and_vector_store() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let a = h.upload(ALICE, chat, "a.txt", "text/plain", b"one").await;
    let b = h.upload(ALICE, chat, "b.png", "image/png", &png(2, 2)).await;
    let ids = [a.json()["id"].as_str().unwrap().to_owned(), b.json()["id"].as_str().unwrap().to_owned()];
    assert_eq!(vector_stores(&h, chat).await.len(), 1);
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.status, 204);
    // Marked pending in the deletion transaction, then cleaned up by the outbox.
    wait_for("attachment cleanup", 10, || async {
        let mut done = true;
        for id in &ids {
            done &= h.attachment(Uuid::parse_str(id).unwrap()).await.cleanup_status.as_deref() == Some("done");
        }
        done
    })
    .await;
    wait_for("vector store removed", 10, || async { vector_stores(&h, chat).await.is_empty() }).await;
    assert_eq!(h.provider.count("DELETE", "/files/"), 2);
    assert_eq!(h.provider.count("DELETE", "/vector_stores/"), 1);
    // The chat row stays soft-deleted.
    let row = ent::chats::Entity::find()
        .filter(ent::chats::Column::Id.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&h.db.conn().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(row.deleted_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_cleanup_retries_failed_deletes_and_stops_at_max_attempts() {
    let mut opts = Opts::default();
    opts.cfg.cleanup_worker.max_attempts = 2;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let id = Uuid::parse_str(h.upload(ALICE, chat, "a.txt", "text/plain", b"one").await.json()["id"].as_str().unwrap()).unwrap();
    *h.provider.delete_status.lock().unwrap() = 500;
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    wait_for("attempt recorded", 10, || async { h.attachment(id).await.cleanup_attempts >= 1 }).await;
    wait_for("max attempts reached", 20, || async { h.attachment(id).await.cleanup_status.as_deref() == Some("failed") }).await;
    let row = h.attachment(id).await;
    assert_eq!(row.cleanup_attempts, 2);
    assert!(row.last_cleanup_error.is_some());

    // Transient failure recovers: next chat, failure then success.
    let chat2 = h.chat(ALICE, Some("standard-m")).await;
    let id2 = Uuid::parse_str(h.upload(ALICE, chat2, "b.txt", "text/plain", b"two").await.json()["id"].as_str().unwrap()).unwrap();
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat2}"), None).await.status, 204);
    wait_for("first failed attempt", 10, || async { h.attachment(id2).await.cleanup_attempts >= 1 }).await;
    *h.provider.delete_status.lock().unwrap() = 200;
    // attempts < max: the outbox redelivers and the next delete succeeds.
    wait_for("cleanup done", 20, || async { h.attachment(id2).await.cleanup_status.as_deref() == Some("done") }).await;
    assert_eq!(h.attachment(id2).await.cleanup_attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_cleanup_handler_direct_semantics() {
    let h = Harness::with(Opts { start_outbox: true, ..Default::default() }).await;
    let handler = QueueHandler::new(Arc::clone(&h.svc), QueueKind::ChatCleanup);
    assert!(matches!(handler.handle_payload(b"not json", 0).await, HandlerOutcome::Reject(_)));
    // A chat that is not soft-deleted is rejected (never cleaned up).
    let chat = h.chat(ALICE, None).await;
    let ev = json!({"tenant_id": ALICE.tenant, "chat_id": chat, "system_request_id": Uuid::new_v4(), "reason": "chat_soft_delete",
        "chat_deleted_at": "2026-01-01T00:00:00Z"});
    assert!(matches!(handler.handle_payload(ev.to_string().as_bytes(), 0).await, HandlerOutcome::Reject(_)));
    assert_eq!(h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachment_cleanup_handler_semantics() {
    let h = Harness::new().await;
    let handler = QueueHandler::new(Arc::clone(&h.svc), QueueKind::AttachmentCleanup);
    assert!(matches!(handler.handle_payload(b"{}", 0).await, HandlerOutcome::Reject(_)));
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let id = Uuid::parse_str(h.upload(ALICE, chat, "a.png", "image/png", &png(2, 2)).await.json()["id"].as_str().unwrap()).unwrap();
    *h.provider.delete_status.lock().unwrap() = 500;
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{id}"), None).await.status, 204);
    wait_for("failed attempt", 10, || async { h.attachment(id).await.cleanup_attempts >= 1 }).await;
    assert_eq!(h.attachment(id).await.cleanup_status.as_deref(), Some("pending"));
    *h.provider.delete_status.lock().unwrap() = 200;
    wait_for("cleanup done", 20, || async { h.attachment(id).await.cleanup_status.as_deref() == Some("done") }).await;
    // A redelivered message after completion is a no-op.
    let file_deletes = h.provider.count("DELETE", "/files/");
    let row = h.attachment(id).await;
    let ev = json!({"event_type": "attachment_deleted", "tenant_id": ALICE.tenant, "chat_id": chat, "attachment_id": id,
        "provider_file_id": row.provider_file_id, "vector_store_id": null, "storage_backend": row.storage_backend,
        "attachment_kind": "image", "deleted_at": "2026-01-01T00:00:00Z", "secondary_ref": null});
    assert_eq!(handler.handle_payload(ev.to_string().as_bytes(), 0).await, HandlerOutcome::Ok);
    assert_eq!(h.provider.count("DELETE", "/files/"), file_deletes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_handler_outcomes() {
    let h = Harness::new().await;
    let handler = QueueHandler::new(Arc::clone(&h.svc), QueueKind::Usage);
    assert!(matches!(handler.handle_payload(b"garbage", 0).await, HandlerOutcome::Reject(_)));
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "x").await;
    h.eventually("usage", || h.usage_events().len() == 1).await;
    let payload = serde_json::to_vec(&h.usage_events()[0]).unwrap();
    h.policy.publish_failures.store(1, Ordering::SeqCst);
    assert!(matches!(handler.handle_payload(&payload, 0).await, HandlerOutcome::Retry(_)));
    assert_eq!(handler.handle_payload(&payload, 1).await, HandlerOutcome::Ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_handler_delivery_drop_retry_reject() {
    let h = Harness::new().await;
    let handler = QueueHandler::new(Arc::clone(&h.svc), QueueKind::Audit);
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "x").await;
    h.eventually("audit", || h.audit_events().len() == 1).await;
    let payload = serde_json::to_vec(&h.audit_events()[0]).unwrap();
    assert!(matches!(handler.handle_payload(b"[]", 0).await, HandlerOutcome::Reject(_)));
    h.audit.mode.store(AUDIT_DROP, Ordering::SeqCst);
    assert_eq!(handler.handle_payload(&payload, 0).await, HandlerOutcome::Ok, "dropped events are acknowledged");
    h.audit.mode.store(AUDIT_RETRY, Ordering::SeqCst);
    assert!(matches!(handler.handle_payload(&payload, 0).await, HandlerOutcome::Retry(_)));
    assert!(matches!(handler.handle_payload(&payload, 119).await, HandlerOutcome::Reject(_)), "max attempts \u{2192} dead letter");
    h.audit.mode.store(AUDIT_REJECT, Ordering::SeqCst);
    assert!(matches!(handler.handle_payload(&payload, 0).await, HandlerOutcome::Reject(_)));
}

struct FlakyAudit {
    result: std::sync::Mutex<Option<MiniChatAuditPluginError>>,
    delay: Duration,
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FlakyAudit {
    async fn emit(&self, _event: AuditEvent) -> Result<(), MiniChatAuditPluginError> {
        tokio::time::sleep(self.delay).await;
        match self.result.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_gateway_maps_plugin_results() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "x").await;
    h.eventually("audit", || h.audit_events().len() == 1).await;
    let ev = h.audit_events()[0].clone();
    // No plugin registered → dropped.
    let gw = AuditGateway::new(Box::new(NoSource));
    assert_eq!(gw.deliver(ev.clone()).await, AuditDelivery::Dropped);
    for (err, expect_retry) in [
        (None, None),
        (Some(MiniChatAuditPluginError::Transient("t".into())), Some(true)),
        (Some(MiniChatAuditPluginError::PluginTimeout), Some(true)),
        (Some(MiniChatAuditPluginError::Permanent("p".into())), Some(false)),
    ] {
        let plugin: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(FlakyAudit { result: std::sync::Mutex::new(err), delay: Duration::ZERO });
        let gw = AuditGateway::new(Box::new(FixedSource(plugin)));
        let out = gw.deliver(ev.clone()).await;
        match expect_retry {
            None => assert_eq!(out, AuditDelivery::Ok),
            Some(true) => assert!(matches!(out, AuditDelivery::Retry(_)), "{out:?}"),
            Some(false) => assert!(matches!(out, AuditDelivery::Reject(_)), "{out:?}"),
        }
    }
    // The bundled static audit plugin accepts every event.
    let plugin: Arc<dyn MiniChatAuditPluginClientV1> = Arc::new(mini_chat::infra::plugins::static_audit::StaticAuditService::new(true));
    assert_eq!(AuditGateway::new(Box::new(FixedSource(plugin))).deliver(ev).await, AuditDelivery::Ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_gateway_serves_static_plugin() {
    let cfg = policy_cfg(catalog(), json!({}));
    let plugin: Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1> =
        Arc::new(mini_chat::infra::plugins::static_model_policy::StaticModelPolicyService::from_config(&cfg));
    let gw = PolicyGateway::new(Box::new(FixedSource(plugin)));
    let snap = gw.current_snapshot(USER_1).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    assert!(snap.find_enabled("premium-m").is_some());
    assert!(snap.find_enabled("disabled-m").is_none());
    let limits = gw.user_limits(USER_1, 1).await.unwrap();
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 50_000_000);
    // No plugin → policy resolution failure.
    let gw = PolicyGateway::new(Box::new(NoSource));
    assert!(gw.current_snapshot(USER_1).await.is_err());
}

/// Three ~1000-byte turns on `tiny-m` truncate the history and schedule a summary.
async fn summarized_chat(h: &Harness) -> Uuid {
    let chat = h.chat(ALICE, Some("tiny-m")).await;
    for i in 0..3 {
        h.say(ALICE, chat, &format!("{i}{}", "m".repeat(999))).await;
    }
    chat
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_summary_is_triggered_generated_and_used() {
    let h = Harness::new().await;
    let chat = summarized_chat(&h).await;
    wait_for("summary row", 10, || async { !summaries(&h, chat).await.is_empty() }).await;
    let s = summaries(&h, chat).await.remove(0);
    assert_eq!(s.summary_text, "Summary of the chat.", "only the <summary> block is stored");
    assert_eq!(s.token_estimate, 20);
    // Summary request: non-streaming, summary model, system prompt, summary metadata.
    let req = h.provider.summary_requests().pop().expect("summary request");
    assert_eq!(req["model"], json!("prov-standard-m"));
    assert_eq!(req["metadata"]["request_type"], json!("summary"));
    assert_eq!(req["metadata"]["chat_id"], json!(chat.to_string()));
    assert!(req["instructions"].as_str().unwrap().contains("conversation summarizer"));
    assert!(req["input"][0].to_string().contains("Summarize the following conversation"));
    // Messages up to the frontier are compressed; later ones are not.
    let msgs = h.messages(chat).await;
    let frontier = msgs.iter().position(|m| m.id == s.summarized_up_to_message_id).expect("frontier is a message");
    assert!(msgs[..=frontier].iter().all(|m| m.is_compressed));
    assert!(msgs[frontier + 1..].iter().all(|m| !m.is_compressed));
    // The summary is billed as a system task.
    h.eventually("system usage", || h.usage_events().iter().any(|u| u.billing_outcome == "system_task")).await;
    let u = h.usage_events().into_iter().find(|u| u.billing_outcome == "system_task").unwrap();
    assert_eq!(u.system_task_type.as_deref(), Some("thread_summary_update"));
    assert_eq!(u.requester_type, "system");
    assert!(u.user_id.is_none());

    // The next turn uses the summary instead of the compressed messages.
    let ev = h.say(ALICE, chat, "follow-up").await;
    assert!(find(&ev, "stream_started")["thread_summary_applied"].is_object());
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req["input"][0].to_string().contains("Summary of the chat."));
}

fn task_for(chat: Uuid, base: Option<&ent::thread_summaries::Model>, target: &ent::messages::Model) -> Vec<u8> {
    serde_json::to_vec(&ThreadSummaryTask {
        tenant_id: ALICE.tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|s| s.summarized_up_to_created_at),
        base_frontier_message_id: base.map(|s| s.summarized_up_to_message_id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: "thread_summary_update".into(),
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_summary_failure_cas_and_deleted_frontier() {
    let h = Harness::new().await;
    let chat = summarized_chat(&h).await;
    wait_for("summary row", 10, || async { !summaries(&h, chat).await.is_empty() }).await;
    // Let every queued summary task finish so the frontier is stable.
    let mut before = summaries(&h, chat).await.remove(0);
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = summaries(&h, chat).await.remove(0);
        if now == before {
            break;
        }
        before = now;
    }
    let handler = QueueHandler::new(Arc::clone(&h.svc), QueueKind::ThreadSummary);
    let msgs = h.messages(chat).await;
    let last = msgs.last().unwrap().clone();

    // Provider failure → retry, previous summary kept; last attempt → reject.
    *h.provider.summary_reply.lock().unwrap() = Some(Reply::Status { status: 500, body: json!({"error": {"message": "x"}}), retry_after: None });
    let payload = task_for(chat, Some(&before), &last);
    assert!(matches!(handler.handle_payload(&payload, 0).await, HandlerOutcome::Retry(_)));
    let max = h.cfg.thread_summary_worker.max_attempts;
    assert!(matches!(handler.handle_payload(&payload, max - 1).await, HandlerOutcome::Reject(_)));
    assert_eq!(summaries(&h, chat).await, vec![before.clone()], "previous summary kept");
    *h.provider.summary_reply.lock().unwrap() = None;

    // CAS: a stale base frontier changes nothing.
    let stale = ent::thread_summaries::Model { summarized_up_to_message_id: Uuid::new_v4(), ..before.clone() };
    assert_eq!(handler.handle_payload(&task_for(chat, Some(&stale), &last), 0).await, HandlerOutcome::Ok);
    assert_eq!(summaries(&h, chat).await, vec![before.clone()]);

    // Frozen target deleted (turn deleted) → no change.
    let rid = h.turns(chat).await.into_iter().rfind(|t| t.deleted_at.is_none()).unwrap().request_id;
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await.status, 204);
    assert_eq!(handler.handle_payload(&task_for(chat, Some(&before), &last), 0).await, HandlerOutcome::Ok);
    assert_eq!(summaries(&h, chat).await, vec![before.clone()]);

    // Valid advance from the current frontier succeeds.
    let live: Vec<_> = h.messages(chat).await.into_iter().filter(|m| m.deleted_at.is_none()).collect();
    let target = live.last().unwrap().clone();
    if (target.created_at, target.id) > (before.summarized_up_to_created_at, before.summarized_up_to_message_id) {
        assert_eq!(handler.handle_payload(&task_for(chat, Some(&before), &target), 0).await, HandlerOutcome::Ok);
        let after = summaries(&h, chat).await.remove(0);
        assert_eq!(after.summarized_up_to_message_id, target.id);
    }

    // Deleted chat → acknowledged without work.
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    let calls = h.provider.summary_requests().len();
    assert_eq!(handler.handle_payload(&task_for(chat, None, &last), 0).await, HandlerOutcome::Ok);
    assert_eq!(h.provider.summary_requests().len(), calls);
    assert!(matches!(handler.handle_payload(b"x", 0).await, HandlerOutcome::Reject(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_summary_trigger_for_short_chats_or_failed_turns() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "short").await;
    h.provider.push(Reply::Status { status: 500, body: json!({}), retry_after: None });
    h.say(ALICE, chat, "fails").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(summaries(&h, chat).await.is_empty());
    assert!(h.provider.summary_requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_retries_through_the_outbox_until_delivered() {
    let h = Harness::new().await;
    h.audit.mode.store(AUDIT_RETRY, Ordering::SeqCst);
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "x").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.audit_events().is_empty());
    h.audit.mode.store(AUDIT_OK, Ordering::SeqCst);
    wait_for("audit delivered", 40, || async { h.audit_events().len() == 1 }).await;
}
