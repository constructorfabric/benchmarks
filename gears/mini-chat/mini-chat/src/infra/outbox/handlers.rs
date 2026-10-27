//! Outbox queue handlers: usage, attachment cleanup, chat cleanup, thread
//! summary, audit.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{AttachmentCleanupPayload, ChatCleanupPayload, OutboxEnqueuer, ThreadSummaryPayload};
use crate::domain::services::ServiceContext;
use crate::infra::gateways::{AuditTarget, GatewayError};
use crate::infra::llm::providers::ProviderAdapter;
use crate::infra::llm::resolver::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;
use crate::infra::llm::{InputMessage, LlmRequest};
use crate::infra::metrics::label;

/// `Usage` publication handler (`mini-chat.usage_snapshot`).
pub struct UsageHandler {
    ctx: Arc<ServiceContext>,
}

impl UsageHandler {
    /// Create the handler.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => return MessageResult::Reject(format!("corrupt usage payload: {err}")),
        };
        match self.ctx.policy.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(err) => match err {
                GatewayError::Publish(mini_chat_sdk::PublishError::Permanent(detail)) => {
                    MessageResult::Reject(format!("permanent publish error: {detail}"))
                }
                other => {
                    tracing::warn!("mini-chat: usage publish failed (retrying): {other}");
                    MessageResult::Retry
                }
            },
        }
    }
}

/// Audit delivery handler (`mini-chat.audit`); plugin calls time out after
/// 30 s; the retry budget is 120 attempts.
pub struct AuditHandler {
    ctx: Arc<ServiceContext>,
}

/// Audit retries are bounded: the 120th attempt is dead-lettered.
const AUDIT_MAX_ATTEMPTS: i16 = 120;

impl AuditHandler {
    /// Create the handler.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }

    fn metric(&self, result: &'static str) {
        self.ctx.metrics.audit_emit.add(1, &[label("result", result)]);
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // The payload is checked before the plugin is resolved.
        let event: mini_chat_sdk::TurnAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => {
                self.metric("reject");
                return MessageResult::Reject(format!("corrupt audit payload: {err}"));
            }
        };
        let target = self.ctx.audit.target().await;
        let client = match target {
            AuditTarget::Client(client) => client,
            AuditTarget::NoPlugin => {
                self.metric("dropped");
                return MessageResult::Ok;
            }
            AuditTarget::Retry | AuditTarget::Unavailable => {
                self.metric("retry");
                return self.retry_or_reject(msg, "plugin resolution failed");
            }
        };
        let call = client.record_audit_event(event);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), call).await;
        match outcome {
            Ok(Ok(())) => {
                self.metric("ok");
                MessageResult::Ok
            }
            Ok(Err(mini_chat_sdk::AuditError::Permanent(detail))) => {
                self.metric("reject");
                MessageResult::Reject(format!("permanent audit plugin error: {detail}"))
            }
            Ok(Err(_)) | Err(_) => self.retry_or_reject(msg, "transient audit plugin error"),
        }
    }
}

impl AuditHandler {
    fn retry_or_reject(&self, msg: &OutboxMessage, reason: &str) -> MessageResult {
        if msg.attempts >= AUDIT_MAX_ATTEMPTS - 1 {
            self.metric("reject");
            return MessageResult::Reject(format!("{reason}: max attempts reached"));
        }
        self.metric("retry");
        MessageResult::Retry
    }
}

/// Attachment cleanup handler (`mini-chat.attachment_cleanup`).
pub struct AttachmentCleanupHandler {
    ctx: Arc<ServiceContext>,
    storage: Arc<StorageClient>,
    registry: Arc<ProviderRegistry>,
    s2s: SecurityContext,
}

impl AttachmentCleanupHandler {
    /// Create the handler.
    #[must_use]
    pub fn new(
        ctx: Arc<ServiceContext>,
        storage: Arc<StorageClient>,
        registry: Arc<ProviderRegistry>,
        s2s: SecurityContext,
    ) -> Self {
        Self {
            ctx,
            storage,
            registry,
            s2s,
        }
    }

    fn resolve_storage(&self, backend: &str) -> Option<crate::infra::llm::resolver::ResolvedStorage> {
        // The storage_backend label maps back to the provider id (default:
        // the provider id itself).
        self.registry.resolve_storage(backend, &Uuid::nil().to_string())
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => return MessageResult::Reject(format!("corrupt cleanup payload: {err}")),
        };
        let Ok(conn) = self.ctx.db.conn() else {
                return MessageResult::Retry;
        ;
            };
        // If the parent chat is soft-deleted, chat cleanup owns the files.
        let chat_deleted = match self
            .ctx
            .repos
            .chats
            .get_internal(&conn, payload.tenant_id, payload.chat_id)
            .await
        {
            Ok(Some(chat)) => chat.deleted_at.is_some(),
            Ok(None) => true,
            Err(_) => { return MessageResult::Retry; }
        };
        if chat_deleted {
            return MessageResult::Ok;
        }
        let Some(file_id) = payload.provider_file_id.clone() else {
            // Nothing to delete provider-side.
            drop(self
                .ctx
                .repos
                .attachments
                .cleanup_set_terminal(&conn, payload.attachment_id, "done", None)
                .await);
            return MessageResult::Ok;
        };
        let Some(storage) = self.resolve_storage(&payload.storage_backend) else {
            return MessageResult::Reject(format!(
                "unknown storage backend {}",
                payload.storage_backend
            ));
        };
        let result = self
            .storage
            .delete_file(&self.s2s, &storage, &file_id)
            .await;
        match result {
            Ok(()) => {
                // Secondary copy: best effort.
                if let Some(secondary) = &payload.secondary_ref {
                    if let Err(err) = self
                        .storage
                        .anthropic_delete(&self.s2s, &secondary.upstream_alias, &secondary.file_id)
                        .await
                    {
                        tracing::warn!("mini-chat: secondary file delete failed: {err}");
                    }
                } else if payload.attachment_kind == "image" {
                    self.ctx.metrics.secondary_cleanup_skipped.add(
                        1,
                        &[label("provider_kind", "anthropic")],
                    );
                }
                if let Err(err) = self
                    .ctx
                    .repos
                    .attachments
                    .cleanup_set_terminal(&conn, payload.attachment_id, "done", None)
                    .await
                {
                    tracing::error!("mini-chat: cleanup mark-done failed: {err}");
                    return MessageResult::Retry;
                }
                self.ctx
                    .metrics
                    .cleanup_completed
                    .add(1, &[label("resource_type", "file")]);
                MessageResult::Ok
            }
            Err(err) => {
                drop(self
                    .ctx
                    .repos
                    .attachments
                    .cleanup_record_failure(&conn, payload.attachment_id, &err.to_string())
                    .await);
                let attempts = self
                    .ctx
                    .repos
                    .attachments
                    .get_any_raw(&conn, payload.attachment_id)
                    .await
                    .map_or(0, |r| r.cleanup_attempts);
                self.ctx.metrics.cleanup_retry.add(
                    1,
                    &[
                        label("resource_type", "file"),
                        label("reason", "provider_error"),
                    ],
                );
                if attempts >= self.ctx.config.cleanup_worker.max_attempts as i32 {
                    drop(self
                        .ctx
                        .repos
                        .attachments
                        .cleanup_set_terminal(
                            &conn,
                            payload.attachment_id,
                            "failed",
                            Some("max cleanup attempts reached"),
                        )
                        .await);
                    self.ctx
                        .metrics
                        .cleanup_failed
                        .add(1, &[label("resource_type", "file")]);
                    return MessageResult::Reject("max cleanup attempts reached".to_owned());
                }
                MessageResult::Retry
            }
        }
    }
}

/// Chat cleanup handler (`mini-chat.chat_cleanup`).
pub struct ChatCleanupHandler {
    ctx: Arc<ServiceContext>,
    storage: Arc<StorageClient>,
    registry: Arc<ProviderRegistry>,
    s2s: SecurityContext,
}

impl ChatCleanupHandler {
    /// Create the handler.
    #[must_use]
    pub fn new(
        ctx: Arc<ServiceContext>,
        storage: Arc<StorageClient>,
        registry: Arc<ProviderRegistry>,
        s2s: SecurityContext,
    ) -> Self {
        Self {
            ctx,
            storage,
            registry,
            s2s,
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => return MessageResult::Reject(format!("corrupt chat cleanup payload: {err}")),
        };
        let Ok(conn) = self.ctx.db.conn() else {
                return MessageResult::Retry;
        ;
            };
        // Only soft-deleted chats are processed.
        let chat = match self
            .ctx
            .repos
            .chats
            .get_internal(&conn, payload.tenant_id, payload.chat_id)
            .await
        {
            Ok(Some(chat)) => chat,
            Ok(None) => return MessageResult::Reject("chat is not soft-deleted".to_owned()),
            Err(_) => { return MessageResult::Retry; }
        };
        if chat.deleted_at.is_none() {
            return MessageResult::Reject("chat is not soft-deleted".to_owned());
        }
        // Delete pending attachment files.
        let Ok(pending) = self
            .ctx
            .repos
            .attachments
            .list_cleanup_pending(&conn, payload.tenant_id, payload.chat_id)
            .await
        else {
            return MessageResult::Retry;
        };
        for attachment in pending {
            let Some(file_id) = attachment.provider_file_id.clone() else {
                drop(self
                    .ctx
                    .repos
                    .attachments
                    .cleanup_set_terminal(&conn, attachment.id, "done", None)
                    .await);
                continue;
            };
            let Some(storage) = self
                .registry
                .resolve_storage(&attachment.storage_backend, &payload.tenant_id.to_string())
            else {
                drop(self
                    .ctx
                    .repos
                    .attachments
                    .cleanup_set_terminal(
                        &conn,
                        attachment.id,
                        "failed",
                        Some("unknown storage backend"),
                    )
                    .await);
                continue;
            };
            match self.storage.delete_file(&self.s2s, &storage, &file_id).await {
                Ok(()) => {
                    // Secondary (Anthropic) copy, best effort.
                    if let (Some(secondary_id), Some(_)) =
                        (&attachment.secondary_file_id, &attachment.secondary_provider_kind)
                        && let Some(alias) = self
                            .registry
                            .resolve(&attachment.storage_backend, &payload.tenant_id.to_string())
                            .map(|p| p.alias)
                    {
                        if let Err(err) = self
                            .storage
                            .anthropic_delete(&self.s2s, &alias, secondary_id)
                            .await
                        {
                            tracing::warn!("mini-chat: secondary file delete failed: {err}");
                        }
                    }
                    if let Err(err) = self
                        .ctx
                        .repos
                        .attachments
                        .cleanup_set_terminal(&conn, attachment.id, "done", None)
                        .await
                    {
                        tracing::error!("mini-chat: cleanup mark-done failed: {err}");
                        return MessageResult::Retry;
                    }
                    self.ctx
                        .metrics
                        .cleanup_completed
                        .add(1, &[label("resource_type", "file")]);
                }
                Err(err) => {
                    drop(self
                        .ctx
                        .repos
                        .attachments
                        .cleanup_record_failure(&conn, attachment.id, &err.to_string())
                        .await);
                    self.ctx.metrics.cleanup_retry.add(
                        1,
                        &[
                            label("resource_type", "file"),
                            label("reason", "provider_error"),
                        ],
                    );
                    let attempts = attachment.cleanup_attempts + 1;
                    if attempts >= self.ctx.config.cleanup_worker.max_attempts as i32 {
                        drop(self
                            .ctx
                            .repos
                            .attachments
                            .cleanup_set_terminal(
                                &conn,
                                attachment.id,
                                "failed",
                                Some("max cleanup attempts reached"),
                            )
                            .await);
                        self.ctx
                            .metrics
                            .cleanup_failed
                            .add(1, &[label("resource_type", "file")]);
                    }
                    // Continue with the other attachments; the row stays
                    // pending and the message retries when under the limit.
                }
            }
        }
        // Vector store delete eligibility: all attachment cleanup rows
        // terminal.
        let Ok(pending_left) = self
            .ctx
            .repos
            .attachments
            .list_cleanup_pending(&conn, payload.tenant_id, payload.chat_id)
            .await
        else {
            return MessageResult::Retry;
        };
        if !pending_left.is_empty() {
            return MessageResult::Retry;
        }
        let Ok(store) = self
            .ctx
            .repos
            .vector_stores
            .get(&conn, payload.tenant_id, payload.chat_id)
            .await
        else {
            return MessageResult::Retry;
        };
        let Some(store) = store else {
            return MessageResult::Ok; // nothing to delete
        };
        let Some(vs_id) = store.vector_store_id.clone() else {
            // Placeholder without a provider store: just remove the row.
            if let Err(err) = self
                .ctx
                .repos
                .vector_stores
                .delete_for_chat(&conn, payload.tenant_id, payload.chat_id)
                .await
            {
                tracing::error!("mini-chat: vector store row delete failed: {err}");
                return MessageResult::Retry;
            }
            return MessageResult::Ok;
        };
        let Some(storage) = self
            .registry
            .resolve_storage(&store.provider, &payload.tenant_id.to_string())
        else {
            return MessageResult::Reject(format!("unknown storage backend {}", store.provider));
        };
        match self
            .storage
            .delete_vector_store(&self.s2s, &storage, &vs_id)
            .await
        {
            Ok(()) => {
                if let Err(err) = self
                    .ctx
                    .repos
                    .vector_stores
                    .delete_for_chat(&conn, payload.tenant_id, payload.chat_id)
                    .await
                {
                    tracing::error!("mini-chat: vector store row delete failed: {err}");
                    return MessageResult::Retry;
                }
                self.ctx
                    .metrics
                    .cleanup_completed
                    .add(1, &[label("resource_type", "vector_store")]);
                MessageResult::Ok
            }
            Err(err) => {
                tracing::warn!("mini-chat: vector store delete failed: {err}");
                self.ctx.metrics.cleanup_retry.add(
                    1,
                    &[
                        label("resource_type", "vector_store"),
                        label("reason", "vector_store_delete_failed"),
                    ],
                );
                // All deliveries of the chat cleanup message count toward
                // the limit.
                if i32::from(msg.attempts) + 1 >= self.ctx.config.cleanup_worker.max_attempts as i32 {
                    self.ctx
                        .metrics
                        .cleanup_failed
                        .add(1, &[label("resource_type", "vector_store")]);
                    return MessageResult::Reject(format!(
                        "vector store delete: max attempts ({}) reached",
                        self.ctx.config.cleanup_worker.max_attempts
                    ));
                }
                MessageResult::Retry
            }
        }
    }
}

/// Thread summary handler (`mini-chat.thread_summary`).
pub struct ThreadSummaryHandler {
    ctx: Arc<ServiceContext>,
    adapter: Arc<ProviderAdapter>,
    registry: Arc<ProviderRegistry>,
    s2s: SecurityContext,
}

impl ThreadSummaryHandler {
    /// Create the handler.
    #[must_use]
    pub fn new(
        ctx: Arc<ServiceContext>,
        adapter: Arc<ProviderAdapter>,
        registry: Arc<ProviderRegistry>,
        s2s: SecurityContext,
    ) -> Self {
        Self {
            ctx,
            adapter,
            registry,
            s2s,
        }
    }

    fn metric(&self, result: &'static str) {
        self.ctx
            .metrics
            .thread_summary_execution
            .add(1, &[label("result", result)]);
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => return MessageResult::Reject(format!("corrupt summary payload: {err}")),
        };
        let result = self.run(&payload, msg.attempts).await;
        match result {
            Ok(()) => MessageResult::Ok,
            Err(SummaryFailure::Retry(reason)) => {
                // A handler attempt that would return Retry on its last
                // delivery returns Reject instead (dead letter).
                if i32::from(msg.attempts) + 1 >= self.ctx.config.thread_summary_worker.max_attempts as i32 {
                    self.metric("provider_error");
                    return MessageResult::Reject(format!("{reason}: max attempts reached"));
                }
                MessageResult::Retry
            }
            Err(SummaryFailure::Reject(reason)) => MessageResult::Reject(reason),
        }
    }
}

enum SummaryFailure {
    Retry(String),
    Reject(String),
}

impl ThreadSummaryHandler {
    async fn run(&self, payload: &ThreadSummaryPayload, _attempts: i16) -> Result<(), SummaryFailure> {
        let cfg = &self.ctx.config.thread_summary_worker;
        // Resolve the summary model (enabled filter).
        let user_id = toolkit_security::constants::DEFAULT_SUBJECT_ID;
        let version = self
            .ctx
            .policy
            .current_policy_version(user_id)
            .await
            .map_err(|e| SummaryFailure::Retry(format!("policy resolution failed: {e}")))?;
        let snapshot = self
            .ctx
            .policy
            .policy_snapshot(user_id, version)
            .await
            .map_err(|e| SummaryFailure::Retry(format!("policy resolution failed: {e}")))?;
        let model_id = if cfg.summary_model_id.is_empty() {
            crate::config::DEFAULT_SUMMARY_MODEL_ID
        } else {
            cfg.summary_model_id.as_str()
        };
        let Some(model) = snapshot.enabled_model(model_id) else {
            tracing::error!("mini-chat: summary model {model_id} missing or disabled");
            self.metric("model_unavailable");
            return Err(SummaryFailure::Reject(format!(
                "summary model {model_id} unavailable"
            )));
        };
        let model = model.clone();
        let provider = self
            .registry
            .resolve(&model.provider_id, &payload.tenant_id.to_string())
            .ok_or_else(|| SummaryFailure::Retry("provider resolution failed".to_owned()))?;

        let conn = self.ctx.db.conn().map_err(|_| SummaryFailure::Retry("db".to_owned()))?;
        let base = match (
            &payload.base_frontier_created_at,
            &payload.base_frontier_message_id,
        ) {
            (Some(ts), Some(id)) => {
                let ts = time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
                    .map_err(|e| SummaryFailure::Reject(format!("bad base frontier: {e}")))?;
                Some((ts, *id))
            }
            _ => None,
        };
        // If the base frontier is set, the stored summary must still exist.
        if base.is_some() {
            let summary = self
                .ctx
                .repos
                .summaries
                .get(&conn, payload.tenant_id, payload.chat_id)
                .await
                .map_err(|e| SummaryFailure::Retry(format!("{e}")))?;
            match summary {
                Some(row) => {
                    let stored = (row.summarized_up_to_created_at, row.summarized_up_to_message_id);
                    if Some(stored) != base {
                        // Frontier moved; another commit advanced it.
                        self.ctx.metrics.thread_summary_cas_conflicts.add(1, &[]);
                        return Ok(());
                    }
                }
                None => {
                    self.metric("base_missing");
                    return Ok(());
                }
            }
        }
        let target_ts = time::OffsetDateTime::parse(
            &payload.frozen_target_created_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|e| SummaryFailure::Reject(format!("bad target frontier: {e}")))?;
        let target = (target_ts, payload.frozen_target_message_id);
        let messages = self
            .ctx
            .repos
            .messages
            .summary_range(&conn, payload.tenant_id, payload.chat_id, base, target)
            .await
            .map_err(|e| SummaryFailure::Retry(format!("{e}")))?;
        if messages.is_empty() {
            return Ok(());
        }

        // Build the summary prompt.
        let existing_summary = self
            .ctx
            .repos
            .summaries
            .get(&conn, payload.tenant_id, payload.chat_id)
            .await
            .map_err(|e| SummaryFailure::Retry(format!("{e}")))?
            .map(|s| s.summary_text);
        let system_prompt = if !model.thread_summary_prompt.is_empty() {
            model.thread_summary_prompt.clone()
        } else {
            cfg.summary_system_prompt.clone()
        };
        let user_prompt = build_summary_prompt(
            existing_summary.as_deref(),
            &messages,
            cfg.message_content_limit,
        );

        let tenant_hex = payload.tenant_id.as_simple().to_string();
        let user_hex = user_id.as_simple().to_string();
        let request = LlmRequest {
            provider_model_id: model.provider_model_id.clone(),
            instructions: system_prompt,
            input: vec![InputMessage {
                role: "user".to_owned(),
                text: user_prompt,
                images: Vec::new(),
            }],
            trailing_items: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: model.max_output_tokens,
            max_tool_calls: model.max_tool_calls,
            user: format!("{tenant_hex}{user_hex}"),
            metadata: Some(serde_json::Map::from_iter([
                (
                    "tenant_id".to_owned(),
                    serde_json::Value::from(payload.tenant_id.to_string()),
                ),
                ("user_id".to_owned(), serde_json::Value::from(user_id.to_string())),
                (
                    "chat_id".to_owned(),
                    serde_json::Value::from(payload.chat_id.to_string()),
                ),
                ("request_type".to_owned(), serde_json::Value::from("summary")),
                ("feature".to_owned(), serde_json::Value::from("none")),
            ])),
            api_params: model.general_config.api_params.clone(),
            stream: false,
        };
        let result = self
            .adapter
            .complete(&self.s2s, &provider, &request)
            .await;
        let (text, usage, _response_id) = match result {
            Ok(ok) => ok,
            Err(err) => {
                self.metric("provider_error");
                self.ctx.metrics.summary_fallback.add(1, &[]);
                return Err(SummaryFailure::Retry(format!("summary call failed: {err}")));
            }
        };
        let Some(summary_text) = parse_summary_response(&text) else {
            self.metric("empty_summary");
            return Err(SummaryFailure::Retry("empty summary".to_owned()));
        };
        let token_estimate = summary_token_estimate(usage, &summary_text);

        // CAS commit.
        let commit = self
            .ctx
            .db
            .transaction(|tx| {
                let ctx = Arc::clone(&self.ctx);
                let summary_text = summary_text.clone();
                let payload_chat = payload.chat_id;
                let payload_tenant = payload.tenant_id;
                let system_request_id = payload.system_request_id;
                Box::pin(async move {
                    // The target frontier message must not be soft-deleted.
                    let target_alive = ctx
                        .repos
                        .messages
                        .get(
                            tx,
                            &toolkit_db::secure::AccessScope::allow_all(),
                            payload_tenant,
                            payload_chat,
                            target.1,
                        )
                        .await?
                        .is_some_and(|m| m.deleted_at.is_none());
                    if !target_alive {
                        return Ok(SummaryCommit::FrontierDeleted);
                    }
                    let won = ctx
                        .repos
                        .summaries
                        .commit_cas(tx, payload_tenant, payload_chat, base, &summary_text, target, token_estimate)
                        .await?;
                    if !won {
                        return Ok(SummaryCommit::CasLost);
                    }
                    ctx.repos
                        .messages
                        .mark_compressed(tx, payload_tenant, payload_chat, base, target)
                        .await?;
                    // System-task usage event.
                    let event = UsageEvent {
                        tenant_id: payload_tenant,
                        user_id: None,
                        chat_id: payload_chat,
                        turn_id: None,
                        request_id: system_request_id,
                        effective_model: model.id.clone(),
                        selected_model: model.id.clone(),
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(usage.unwrap_or_default()),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: crate::api::rest::dto::fmt_ts(&time::OffsetDateTime::now_utc()),
                        requester_type: "system".to_owned(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            payload_tenant.as_simple(),
                            system_request_id.as_simple()
                        ),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    let wake = ctx.outbox.enqueue_usage(tx, &event).await?;
                    Ok::<_, crate::error::DomainError>(SummaryCommit::Done(wake))
                })
            })
            .await;
        match commit {
            Ok(SummaryCommit::Done(wake)) => {
                wake.fire();
                self.metric("success");
                Ok(())
            }
            Ok(SummaryCommit::FrontierDeleted) => {
                self.metric("frontier_deleted");
                Ok(())
            }
            Ok(SummaryCommit::CasLost) => {
                self.ctx.metrics.thread_summary_cas_conflicts.add(1, &[]);
                Ok(())
            }
            Err(err) => {
                self.metric("retry");
                Err(SummaryFailure::Retry(format!("commit failed: {err}")))
            }
        }
    }
}

enum SummaryCommit {
    Done(toolkit_db::outbox::Wake),
    FrontierDeleted,
    CasLost,
}

/// Build the summary user prompt.
fn build_summary_prompt(
    existing: Option<&str>,
    messages: &[crate::infra::storage::entity::messages::Model],
    content_limit: usize,
) -> String {
    let mut out = String::new();
    match existing {
        None => out.push_str("Summarize the following conversation:\n\n"),
        Some(existing) => {
            out.push_str("The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.\n\n<existing_summary>\n");
            out.push_str(existing);
            out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
        }
    }
    for msg in messages {
        if msg.role == "system" {
            continue;
        }
        let role = if msg.role == "assistant" { "Assistant" } else { "User" };
        let content = if content_limit > 0 && msg.content.chars().count() > content_limit {
            let cut: String = msg.content.chars().take(content_limit).collect();
            format!("{cut}...")
        } else {
            msg.content.clone()
        };
        out.push_str(&format!("{role}: {content}\n\n"));
    }
    out.push_str("Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.\n");
    out
}

/// Parse the summary response: remove the `<analysis>` block, keep the
/// `<summary>` content (blank runs collapsed). Without a `<summary>` block,
/// the whole remaining text is stored unless it still contains markup.
#[must_use]
pub fn parse_summary_response(text: &str) -> Option<String> {
    // Remove the analysis block.
    let mut rest = text.to_owned();
    if let (Some(start), Some(end)) = (rest.find("<analysis>"), rest.find("</analysis>")) {
        rest = format!("{}{}", &rest[..start], &rest[end + "</analysis>".len()..]);
    }
    let summary = if let (Some(start), Some(end)) = (rest.find("<summary>"), rest.find("</summary>")) {
        rest[start + "<summary>".len()..end].to_owned()
    } else {
        if rest.contains("<analysis") || rest.contains("<summary") {
            return None;
        }
        rest
    };
    // Collapse runs of blank lines.
    let mut collapsed = String::with_capacity(summary.len());
    let mut blank = false;
    for line in summary.trim().lines() {
        if line.trim().is_empty() {
            if !blank {
                collapsed.push('\n');
            }
            blank = true;
        } else {
            if !collapsed.is_empty() {
                collapsed.push('\n');
            }
            collapsed.push_str(line);
            blank = false;
        }
    }
    let collapsed = collapsed.trim().to_owned();
    if collapsed.is_empty() { None } else { Some(collapsed) }
}

/// The stored token estimate: provider `output_tokens - reasoning_tokens`,
/// or `ceil(bytes / 4)` when not positive.
#[must_use]
pub fn summary_token_estimate(usage: Option<mini_chat_sdk::UsageTokens>, summary: &str) -> i32 {
    let from_usage = usage.map_or(0, |u| u.output_tokens - u.reasoning_tokens);
    if from_usage > 0 {
        return i32::try_from(from_usage).unwrap_or(i32::MAX);
    }
    i32::try_from(summary.len().div_ceil(4)).unwrap_or(i32::MAX)
}
