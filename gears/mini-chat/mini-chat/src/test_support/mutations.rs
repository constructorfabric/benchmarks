//! Test doubles and harness helpers of the turn mutations and reactions feature
//! (`cpt-cf-mini-chat-feature-turn-mutations-and-reactions`): seeding of terminal turns with
//! their messages, links and counters that does not go through the code under test, a snapshot
//! of everything a mutation writes (to prove a rejection leaves nothing behind), an audit outbox
//! double that fails the mutation enqueue, REST helpers of the five endpoints on the
//! [`SendRig`] and the Problem assertions of the feature.
//!
//! If the author stage changes the constructors of `TurnMutationService` or `ReactionService`,
//! `send_rig_with` in `send.rs` is the only place to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use toolkit_db::DBProvider;
use toolkit_odata::{ODataQuery, Page};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use mini_chat_sdk::AuditEvent;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Set};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DbTx, SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::chats::{
    Reply, a1, assert_problem, json_request, outbox_body_count, request, request_with_body,
};
use super::quota::{QuotaRowView, all_quota_rows};
use super::send::ReserveGate;
use super::send::{
    MessageSeed, SendRig, SseFrame, TurnSeedFull, chat_model, link_rows, parse_sse, seed_msg,
    seed_turn_full, t, turn_rows,
};
use super::{TestDb, USER_A1};
use crate::domain::chats::{ChatCleanupQueue, ChatDetail, ChatDraft, ChatStore};
use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::mutations::{
    CommitCommand, CommittedMutation, ExistingReserve, MutationReads, MutationStore, MutationTurn,
    UnstartedFailure,
};
use crate::domain::quota::{QuotaService, ReserveReceipt};
use crate::domain::streaming::summary::ThreadSummaryWork;
use crate::domain::streaming::{
    ContextReads, PreflightReads, ReserveCommand, ReservedTurn, SnapshotBoundary, StreamStore,
    TurnOutbox,
};
use crate::domain::turns::TurnRow;
use crate::infra::storage::entity::{
    chat_turn, message, message_attachment, message_reaction, thread_summary,
};
use crate::infra::storage::repo_impl::ChatRecord;
use toolkit_security::SecurityContext;

// ── URIs ─────────────────────────────────────────────────────────────────────

/// `/mini-chat/v1/chats/{id}/turns/{request_id}/retry`.
pub fn retry_uri(chat_id: Uuid, request_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/turns/{request_id}/retry")
}

/// `/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction`.
pub fn reaction_uri(chat_id: Uuid, message_id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat_id}/messages/{message_id}/reaction")
}

// ── seeding of terminal turns ────────────────────────────────────────────────

/// What a seeded turn looks like. Offsets are microseconds after `t(0)`.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    state: &'static str,
    user_text: String,
    answer: Option<String>,
    user_at: i64,
    started_at: Option<i64>,
    requester: Option<Uuid>,
    web_search: bool,
    counts: (i32, i32, i32),
    attachments: Vec<(Uuid, i64)>,
    request_id: Uuid,
    error_code: Option<String>,
}

/// A `completed` turn of user 1: a user message at `t(10)` with `user_text`, an assistant message
/// one microsecond later and the turn started at the user message instant.
pub fn turn(user_text: &str) -> TurnPlan {
    TurnPlan {
        state: "completed",
        user_text: user_text.to_owned(),
        answer: Some("the answer".to_owned()),
        user_at: 10,
        started_at: None,
        requester: Some(USER_A1),
        web_search: false,
        counts: (0, 0, 0),
        attachments: Vec::new(),
        request_id: Uuid::new_v4(),
        error_code: None,
    }
}

impl TurnPlan {
    /// The user message instant and, unless [`started`](Self::started) is set, the turn start.
    pub fn at(mut self, micros: i64) -> Self {
        self.user_at = micros;
        self
    }

    /// `chat_turns.started_at` (the latest-turn order key).
    pub fn started(mut self, micros: i64) -> Self {
        self.started_at = Some(micros);
        self
    }

    /// A `failed` turn: no assistant message, `error_code` `provider_error`.
    pub fn failed(mut self) -> Self {
        self.state = "failed";
        self.answer = None;
        self.error_code = Some("provider_error".to_owned());
        self
    }

    /// A `cancelled` turn with a partial assistant message (`Some`) or without text (`None`).
    pub fn cancelled(mut self, partial: Option<&str>) -> Self {
        self.state = "cancelled";
        self.answer = partial.map(ToOwned::to_owned);
        self
    }

    /// A `running` turn: no assistant message yet.
    pub fn running(mut self) -> Self {
        self.state = "running";
        self.answer = None;
        self
    }

    /// The assistant text of the turn.
    pub fn answer(mut self, text: &str) -> Self {
        self.answer = Some(text.to_owned());
        self
    }

    /// The requester of the turn (`None`: a NULL requester).
    pub fn requester(mut self, user_id: Option<Uuid>) -> Self {
        self.requester = user_id;
        self
    }

    /// `chat_turns.web_search_enabled`.
    pub fn web_search(mut self) -> Self {
        self.web_search = true;
        self
    }

    /// The completed tool counts `(web_search, code_interpreter, file_search)`.
    pub fn counts(mut self, web: i32, code: i32, file: i32) -> Self {
        self.counts = (web, code, file);
        self
    }

    /// Link `attachment_id` to the user message; the link `created_at` is `t(micros)`.
    pub fn attach(mut self, attachment_id: Uuid, link_micros: i64) -> Self {
        self.attachments.push((attachment_id, link_micros));
        self
    }
}

/// The ids of a seeded turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the four identifiers are the identity of the seeded turn"
)]
pub struct SeededTurn {
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    pub assistant_message_id: Option<Uuid>,
}

/// Insert the turn, its messages and its links through the Secure ORM.
pub async fn seed(db: &TestDb, chat: &ChatRecord, plan: TurnPlan) -> SeededTurn {
    let user_message_id = Uuid::new_v4();
    seed_msg(
        db,
        chat,
        MessageSeed::new("user", &plan.user_text, t(plan.user_at))
            .id(user_message_id)
            .request(Some(plan.request_id)),
    )
    .await;
    let assistant_message_id = match &plan.answer {
        Some(text) => {
            let id = Uuid::new_v4();
            seed_msg(
                db,
                chat,
                MessageSeed::new("assistant", text, t(plan.user_at + 1))
                    .id(id)
                    .request(Some(plan.request_id))
                    .model(&chat.model)
                    .tokens(120, 30),
            )
            .await;
            Some(id)
        }
        None => None,
    };
    let turn_id = Uuid::new_v4();
    let mut seed_row = TurnSeedFull::new(plan.state)
        .id(turn_id)
        .request(plan.request_id)
        .requester(plan.requester)
        .started(t(plan.started_at.unwrap_or(plan.user_at)))
        .updated(t(plan.user_at + 2))
        .counts(plan.counts.0, plan.counts.1, plan.counts.2)
        .model(&chat.model);
    if plan.web_search {
        seed_row = seed_row.web_search(true);
    }
    if let Some(id) = assistant_message_id {
        seed_row = seed_row.assistant(id);
    }
    if let Some(code) = &plan.error_code {
        seed_row = seed_row.error(code);
    }
    seed_turn_full(db, chat, seed_row).await;
    for (attachment_id, micros) in &plan.attachments {
        link_at(db, chat, user_message_id, *attachment_id, t(*micros)).await;
    }
    SeededTurn {
        request_id: plan.request_id,
        turn_id,
        user_message_id,
        assistant_message_id,
    }
}

/// Link an attachment to a message with the given link `created_at`.
pub async fn link_at(
    db: &TestDb,
    chat: &ChatRecord,
    message_id: Uuid,
    attachment_id: Uuid,
    created_at: OffsetDateTime,
) {
    let conn = db.provider.conn().unwrap();
    let row = message_attachment::ActiveModel {
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(created_at),
    };
    secure_insert::<message_attachment::Entity>(
        row,
        &AccessScope::for_tenant(chat.tenant_id),
        &conn,
    )
    .await
    .expect("link attachment");
}

/// Soft-delete an attachment row directly.
pub async fn soft_delete_attachment(db: &TestDb, attachment_id: Uuid) {
    super::raw_exec(
        &db.raw,
        &format!(
            "UPDATE attachments SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            super::chats::blob(attachment_id)
        ),
    )
    .await
    .expect("soft-delete attachment");
}

/// Soft-delete a chat row directly (the chat delete endpoint needs the cleanup queue, which the
/// rig leaves unbound).
pub async fn soft_delete_chat(db: &TestDb, chat_id: Uuid) {
    super::raw_exec(
        &db.raw,
        &format!(
            "UPDATE chats SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = {}",
            super::chats::blob(chat_id)
        ),
    )
    .await
    .expect("soft-delete chat");
}

/// Set `is_compressed` on every message of the chat (a summary covered them).
pub async fn mark_compressed(db: &TestDb, chat_id: Uuid) {
    super::raw_exec(
        &db.raw,
        &format!(
            "UPDATE messages SET is_compressed = 1 WHERE chat_id = {}",
            super::chats::blob(chat_id)
        ),
    )
    .await
    .expect("mark compressed");
}

// ── inspection ───────────────────────────────────────────────────────────────

/// Everything a mutation writes: two snapshots are equal when nothing was written in between.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub turns: Vec<chat_turn::Model>,
    pub messages: Vec<message::Model>,
    pub links: Vec<message_attachment::Model>,
    pub summary: Option<thread_summary::Model>,
    pub chat_updated_at: OffsetDateTime,
    pub outbox: i64,
    pub quota: Vec<QuotaRowView>,
}

/// The [`Snapshot`] of a chat.
pub async fn snapshot(db: &TestDb, chat: &ChatRecord) -> Snapshot {
    Snapshot {
        turns: turn_rows(db, chat.id).await,
        messages: super::chats::message_rows(db, chat.id).await,
        links: link_rows(db, chat.id).await,
        summary: summary_row(db, chat.id).await,
        chat_updated_at: chat_model(db, chat.id).await.updated_at,
        outbox: outbox_body_count(db).await,
        quota: all_quota_rows(db).await,
    }
}

/// The thread summary row of the chat, `None` without one.
pub async fn summary_row(db: &TestDb, chat_id: Uuid) -> Option<thread_summary::Model> {
    let conn = db.provider.conn().unwrap();
    thread_summary::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .one(&conn)
        .await
        .unwrap()
}

/// The reaction rows of a message, every user.
pub async fn reaction_rows(db: &TestDb, message_id: Uuid) -> Vec<message_reaction::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = message_reaction::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(message_reaction::Column::MessageId.eq(message_id)))
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| row.user_id);
    rows
}

/// The non-deleted messages of the chat in chronological order `(created_at, id)`.
pub async fn live_messages(db: &TestDb, chat_id: Uuid) -> Vec<message::Model> {
    let mut rows: Vec<message::Model> = super::chats::message_rows(db, chat_id)
        .await
        .into_iter()
        .filter(|row| row.deleted_at.is_none())
        .collect();
    rows.sort_by_key(|row| (row.created_at, row.id));
    rows
}

/// The turn row of `request_id`.
pub async fn turn_by_request(db: &TestDb, chat_id: Uuid, request_id: Uuid) -> chat_turn::Model {
    turn_rows(db, chat_id)
        .await
        .into_iter()
        .find(|row| row.request_id == request_id)
        .unwrap_or_else(|| panic!("no turn with request_id {request_id}"))
}

/// The turn that replaced `request_id`, found through `replaced_by_request_id`.
pub async fn replacement_of(db: &TestDb, chat_id: Uuid, request_id: Uuid) -> chat_turn::Model {
    let old = turn_by_request(db, chat_id, request_id).await;
    let new_id = old
        .replaced_by_request_id
        .unwrap_or_else(|| panic!("turn {request_id} was not replaced"));
    turn_by_request(db, chat_id, new_id).await
}

/// The audit events of the audit queue that are mutation events (`turn_retry`, `turn_edit`,
/// `turn_delete`), typed.
pub async fn mutation_events(rig: &SendRig) -> Vec<AuditEvent> {
    rig.audit_events()
        .await
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                AuditEvent::TurnRetry(_) | AuditEvent::TurnEdit(_) | AuditEvent::TurnDelete(_)
            )
        })
        .collect()
}

/// The audit events of the audit queue that are turn finalization events.
pub async fn turn_events(rig: &SendRig) -> Vec<AuditEvent> {
    rig.audit_events()
        .await
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                AuditEvent::TurnCompleted(_) | AuditEvent::TurnFailed(_)
            )
        })
        .collect()
}

// ── an outbox that fails the mutation audit enqueue ──────────────────────────

/// A turn outbox that delegates everything to the real enqueuer except the mutation audit
/// enqueue, which fails with the error built by `make` (an oversized payload, a database
/// failure). Bind the real enqueuer once the rig exists.
pub struct FailingMutationOutbox {
    inner: OnceLock<Arc<dyn TurnOutbox>>,
    make: fn() -> DomainError,
    calls: AtomicUsize,
}

impl FailingMutationOutbox {
    fn with(make: fn() -> DomainError) -> Arc<Self> {
        Arc::new(Self {
            inner: OnceLock::new(),
            make,
            calls: AtomicUsize::new(0),
        })
    }

    /// The enqueue fails as an oversized payload of the retry site.
    pub fn payload_too_large() -> Arc<Self> {
        Self::with(|| DomainError::OutboxPayloadTooLarge {
            site: crate::domain::error::EnqueueSite::TurnRetry,
        })
    }

    /// The enqueue fails with an internal error (a database failure).
    pub fn internal() -> Arc<Self> {
        Self::with(|| DomainError::internal("forced mutation audit enqueue failure"))
    }

    /// Bind the real enqueuer that every other call is delegated to.
    pub fn bind(&self, inner: Arc<dyn TurnOutbox>) {
        drop(self.inner.set(inner));
    }

    /// How often the mutation audit enqueue was attempted.
    pub fn attempts(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn inner(&self) -> &Arc<dyn TurnOutbox> {
        self.inner.get().expect("the real enqueuer is bound")
    }
}

#[async_trait]
impl TurnOutbox for FailingMutationOutbox {
    async fn enqueue_turn_audit(
        &self,
        tx: &DbTx<'_>,
        event: &AuditEvent,
    ) -> Result<Wake, DomainError> {
        self.inner().enqueue_turn_audit(tx, event).await
    }

    async fn enqueue_mutation_audit(
        &self,
        _tx: &DbTx<'_>,
        _tenant_id: Uuid,
        _event: &AuditEvent,
    ) -> Result<Wake, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err((self.make)())
    }

    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        work: &ThreadSummaryWork,
    ) -> Result<Wake, DomainError> {
        self.inner().enqueue_thread_summary(tx, work).await
    }
}

// ── a mutation store with hooks ──────────────────────────────────────────────

/// Runs right before the mutation transaction (a concurrent send commits a turn) or right after
/// it committed and before the post-commit setup starts (a concurrent reserve fills the quota, a
/// database failure is armed).
#[async_trait]
pub trait CommitHook: Send + Sync {
    async fn before_commit(&self, _raw: &sea_orm::DatabaseConnection) {}

    async fn after_commit(&self, _raw: &sea_orm::DatabaseConnection) {}
}

/// A [`CommitHook`] that executes raw SQL statements after the mutation commit.
pub struct ExecSql(pub Vec<String>);

#[async_trait]
impl CommitHook for ExecSql {
    async fn after_commit(&self, raw: &sea_orm::DatabaseConnection) {
        for statement in &self.0 {
            super::raw_exec(raw, statement)
                .await
                .expect("hook statement");
        }
    }
}

/// A [`CommitHook`] armed after the rig exists (the statements often name the chat of the test):
/// its statements run right before and right after the next mutation commits.
#[derive(Default)]
pub struct ArmedHook {
    before: std::sync::Mutex<Vec<String>>,
    after: std::sync::Mutex<Vec<String>>,
}

impl ArmedHook {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The statements to run right before the mutation transaction.
    pub fn arm_before(&self, statements: Vec<String>) {
        *self.before.lock().unwrap() = statements;
    }

    /// The statements to run right after the mutation transaction committed.
    pub fn arm_after(&self, statements: Vec<String>) {
        *self.after.lock().unwrap() = statements;
    }
}

#[async_trait]
impl CommitHook for ArmedHook {
    async fn before_commit(&self, raw: &sea_orm::DatabaseConnection) {
        let statements = std::mem::take(&mut *self.before.lock().unwrap());
        for statement in statements {
            super::raw_exec(raw, &statement)
                .await
                .expect("hook statement");
        }
    }

    async fn after_commit(&self, raw: &sea_orm::DatabaseConnection) {
        let statements = std::mem::take(&mut *self.after.lock().unwrap());
        for statement in statements {
            super::raw_exec(raw, &statement)
                .await
                .expect("hook statement");
        }
    }
}

/// The statement that inserts a `running` turn of the owner in `chat` started at `started_at`
/// (the stored text form `YYYY-MM-DDTHH:MM:SSZ`): a send that committed a turn concurrently.
pub fn running_turn_sql(chat: &ChatRecord, started_at: &str) -> String {
    format!(
        "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
         requester_user_id, state, started_at, updated_at, last_progress_at) \
         VALUES ({}, {}, {}, {}, 'user', {}, 'running', '{started_at}', '{started_at}', \
         '{started_at}')",
        super::chats::blob(Uuid::new_v4()),
        super::chats::blob(chat.tenant_id),
        super::chats::blob(chat.id),
        super::chats::blob(Uuid::new_v4()),
        super::chats::blob(chat.user_id),
    )
}

/// The statement that books `reserved` credits in the daily `total` bucket of user 1 (a concurrent
/// turn that filled the quota); the day is the default instant of the rigs.
pub fn fill_daily_reserved_sql(reserved: i64) -> String {
    format!(
        "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
         spent_credits_micro, reserved_credits_micro, web_search_calls, code_interpreter_calls, \
         updated_at) VALUES ({}, {}, {}, 'daily', '2026-02-28', 'total', 0, {reserved}, 0, 0, \
         '2026-01-01 00:00:00+00:00')",
        super::chats::blob(Uuid::new_v4()),
        super::chats::blob(super::TENANT_A),
        super::chats::blob(USER_A1),
    )
}

/// A [`MutationStore`] over the database provider that runs a [`CommitHook`] after the mutation
/// commit and parks the setup on a [`ReserveGate`] after the `Existing` reserve commit.
pub struct HookedMutationStore {
    pub inner: Arc<DBProvider<DomainError>>,
    pub raw: sea_orm::DatabaseConnection,
    pub hook: Option<Arc<dyn CommitHook>>,
    pub gate: Option<Arc<ReserveGate>>,
}

#[async_trait]
impl ChatStore for HookedMutationStore {
    async fn insert_chat(
        &self,
        scope: &AccessScope,
        ctx: &SecurityContext,
        draft: ChatDraft,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.insert_chat(scope, ctx, draft).await
    }

    async fn load_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.load_chat(scope, chat_id).await
    }

    async fn list_chats(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<ChatDetail>, DomainError> {
        self.inner.list_chats(scope, query).await
    }

    async fn rename_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        title: &str,
    ) -> Result<ChatDetail, DomainError> {
        self.inner.rename_chat(scope, chat_id, title).await
    }

    async fn delete_chat(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        cleanup: Arc<dyn ChatCleanupQueue>,
    ) -> Result<(), DomainError> {
        self.inner.delete_chat(scope, chat_id, cleanup).await
    }
}

#[async_trait]
impl StreamStore for HookedMutationStore {
    async fn find_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        self.inner.find_turn(tenant_id, chat_id, request_id).await
    }

    async fn has_running_turn(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        self.inner.has_running_turn(tenant_id, chat_id).await
    }

    async fn load_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        self.inner
            .load_message(tenant_id, chat_id, message_id)
            .await
    }

    async fn read_preflight(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<PreflightReads, DomainError> {
        self.inner
            .read_preflight(tenant_id, chat_id, attachment_ids)
            .await
    }

    async fn read_context(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        boundary: Option<&SnapshotBoundary>,
        limit: u32,
    ) -> Result<ContextReads, DomainError> {
        self.inner
            .read_context(tenant_id, chat_id, boundary, limit)
            .await
    }

    async fn reserve(
        &self,
        quota: &QuotaService,
        command: &ReserveCommand,
    ) -> Result<ReservedTurn, DomainError> {
        self.inner.reserve(quota, command).await
    }
}

#[async_trait]
impl MutationStore for HookedMutationStore {
    async fn find_mutation_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MutationTurn>, DomainError> {
        self.inner
            .find_mutation_turn(tenant_id, chat_id, request_id)
            .await
    }

    async fn latest_turn(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        self.inner.latest_turn(tenant_id, chat_id).await
    }

    async fn target_user_message(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<MessageRow>, DomainError> {
        self.inner
            .target_user_message(tenant_id, chat_id, request_id)
            .await
    }

    async fn read_mutation(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        target_request_id: Uuid,
    ) -> Result<MutationReads, DomainError> {
        self.inner
            .read_mutation(tenant_id, chat_id, target_request_id)
            .await
    }

    async fn commit_mutation(
        &self,
        outbox: Arc<dyn TurnOutbox>,
        command: &CommitCommand,
    ) -> Result<CommittedMutation, DomainError> {
        if let Some(hook) = &self.hook {
            hook.before_commit(&self.raw).await;
        }
        let committed = self.inner.commit_mutation(outbox, command).await?;
        if let Some(hook) = &self.hook {
            hook.after_commit(&self.raw).await;
        }
        Ok(committed)
    }

    async fn reserve_existing(
        &self,
        quota: &QuotaService,
        command: &ExistingReserve,
    ) -> Result<ReserveReceipt, DomainError> {
        let receipt = self.inner.reserve_existing(quota, command).await?;
        if let Some(gate) = &self.gate {
            gate.committed.notify_one();
            gate.release.notified().await;
        }
        Ok(receipt)
    }

    async fn fail_unstarted(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        code: UnstartedFailure,
    ) -> Result<bool, DomainError> {
        self.inner.fail_unstarted(tenant_id, turn_id, code).await
    }
}

// ── REST helpers of the five endpoints ───────────────────────────────────────

impl SendRig {
    /// `POST .../turns/{request_id}/retry` as `who`, no body.
    pub async fn retry_as(
        &self,
        who: &toolkit_security::SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Reply {
        self.send(request("POST", &retry_uri(chat_id, request_id), who))
            .await
    }

    /// `POST .../turns/{request_id}/retry` as user 1 of tenant A.
    pub async fn retry(&self, chat_id: Uuid, request_id: Uuid) -> Reply {
        self.retry_as(&a1(), chat_id, request_id).await
    }

    /// The retry request as user 1 of tenant A.
    #[allow(
        clippy::unused_self,
        reason = "a method of the rig for symmetry with the helpers"
    )]
    pub fn retry_request(&self, chat_id: Uuid, request_id: Uuid) -> Request<Body> {
        request("POST", &retry_uri(chat_id, request_id), &a1())
    }

    /// `PATCH .../turns/{request_id}` with a JSON body as `who`.
    pub async fn edit_as(
        &self,
        who: &toolkit_security::SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        body: &Value,
    ) -> Reply {
        self.send(json_request(
            "PATCH",
            &super::send::turn_uri(chat_id, request_id),
            who,
            &body.to_string(),
        ))
        .await
    }

    /// `PATCH .../turns/{request_id}` with a JSON body as user 1 of tenant A.
    pub async fn edit(&self, chat_id: Uuid, request_id: Uuid, body: &Value) -> Reply {
        self.edit_as(&a1(), chat_id, request_id, body).await
    }

    /// `PATCH .../turns/{request_id}` with a raw body and content type (`None`: no content type).
    pub async fn edit_raw(
        &self,
        chat_id: Uuid,
        request_id: Uuid,
        content_type: Option<&str>,
        body: &str,
    ) -> Reply {
        self.send(request_with_body(
            "PATCH",
            &super::send::turn_uri(chat_id, request_id),
            &a1(),
            content_type,
            body,
        ))
        .await
    }

    /// `DELETE .../turns/{request_id}` as `who`.
    pub async fn delete_turn_as(
        &self,
        who: &toolkit_security::SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Reply {
        self.send(request(
            "DELETE",
            &super::send::turn_uri(chat_id, request_id),
            who,
        ))
        .await
    }

    /// `DELETE .../turns/{request_id}` as user 1 of tenant A.
    pub async fn delete_turn(&self, chat_id: Uuid, request_id: Uuid) -> Reply {
        self.delete_turn_as(&a1(), chat_id, request_id).await
    }

    /// `PUT .../messages/{msg_id}/reaction` with a JSON body as `who`.
    pub async fn put_reaction_as(
        &self,
        who: &toolkit_security::SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
        body: &Value,
    ) -> Reply {
        self.send(json_request(
            "PUT",
            &reaction_uri(chat_id, message_id),
            who,
            &body.to_string(),
        ))
        .await
    }

    /// `PUT {"reaction": value}` as user 1 of tenant A.
    pub async fn put_reaction(&self, chat_id: Uuid, message_id: Uuid, value: &str) -> Reply {
        self.put_reaction_as(
            &a1(),
            chat_id,
            message_id,
            &serde_json::json!({ "reaction": value }),
        )
        .await
    }

    /// `PUT` with a raw body and content type (`None`: no content type).
    pub async fn put_reaction_raw(
        &self,
        chat_id: Uuid,
        message_id: Uuid,
        content_type: Option<&str>,
        body: &str,
    ) -> Reply {
        self.send(request_with_body(
            "PUT",
            &reaction_uri(chat_id, message_id),
            &a1(),
            content_type,
            body,
        ))
        .await
    }

    /// `DELETE .../messages/{msg_id}/reaction` as `who`.
    pub async fn delete_reaction_as(
        &self,
        who: &toolkit_security::SecurityContext,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Reply {
        self.send(request("DELETE", &reaction_uri(chat_id, message_id), who))
            .await
    }

    /// `DELETE .../messages/{msg_id}/reaction` as user 1 of tenant A.
    pub async fn delete_reaction(&self, chat_id: Uuid, message_id: Uuid) -> Reply {
        self.delete_reaction_as(&a1(), chat_id, message_id).await
    }

    /// The items of the messages list (at most 100) as user 1 of tenant A.
    pub async fn list_messages(&self, chat_id: Uuid) -> Vec<Value> {
        let uri = format!("{}?limit=100", super::send::messages_uri(chat_id));
        let reply = self.get(&uri, &a1()).await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        reply.json()["items"].as_array().unwrap().clone()
    }

    /// The `message_count` of the chat as `GET /v1/chats/{id}` reports it.
    pub async fn message_count(&self, chat_id: Uuid) -> i64 {
        let reply = self.get(&super::chats::chat_uri(chat_id), &a1()).await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        reply.json()["message_count"].as_i64().unwrap()
    }

    /// Wait (5 s at most) until the turn with `request_id` left `running`; returns its row.
    pub async fn wait_turn_terminal(&self, chat_id: Uuid, request_id: Uuid) -> chat_turn::Model {
        for _ in 0..500 {
            let row = turn_by_request(&self.quota.db, chat_id, request_id).await;
            if row.state != "running" {
                return row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the turn {request_id} is still running");
    }
}

/// A mock OAGW that answers every request with the streamed text `Hel` `lo` and usage 120/30.
pub fn happy_upstream() -> Arc<super::provider::MockProvider> {
    super::provider::MockProvider::answering(super::provider::Reply::Sse(
        super::provider::happy_steps(&["Hel", "lo"], (120, 30)),
    ))
}

/// A real-provider-task rig over [`happy_upstream`] with a live standard-model chat of user 1.
pub async fn streaming_chat() -> (SendRig, ChatRecord, Arc<super::provider::MockProvider>) {
    let upstream = happy_upstream();
    let rig = super::send::stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    (rig, chat, upstream)
}

/// The tool of `kind` in a recorded provider request body.
pub fn tool_of<'a>(body: &'a Value, kind: &str) -> Option<&'a Value> {
    body["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["type"] == kind))
}

/// The history items `(role, text)` of a recorded Responses request: every `input` item but the
/// last one (the current user message).
pub fn history_of(body: &Value) -> Vec<(String, String)> {
    let input = body["input"].as_array().expect("an input array");
    input[..input.len() - 1]
        .iter()
        .map(|item| {
            (
                item["role"].as_str().unwrap().to_owned(),
                item["content"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The current user message of a recorded Responses request: the last `input` item.
pub fn current_of(body: &Value) -> &Value {
    body["input"]
        .as_array()
        .and_then(|input| input.last())
        .expect("an input item")
}

/// A rejected request is a JSON Problem (never a stream) and left every row, the quota and the
/// provider untouched.
pub async fn assert_untouched(
    rig: &SendRig,
    chat: &ChatRecord,
    before: &Snapshot,
    upstream: &super::provider::MockProvider,
    reply: &Reply,
) {
    assert!(
        reply.content_type.starts_with("application/problem+json"),
        "no SSE stream may open: {} {}",
        reply.content_type,
        reply.text
    );
    assert_eq!(
        &snapshot(&rig.quota.db, chat).await,
        before,
        "rows were left behind"
    );
    assert_eq!(upstream.call_count(), 0, "a provider request was proxied");
    assert_eq!(rig.provider.call_count(), 0);
}

/// The `request_id` and `message_id` of the `stream_started` frame of a retry or edit stream.
pub fn started_ids(frames: &[SseFrame]) -> (Uuid, Uuid) {
    let started = frames
        .iter()
        .find(|frame| frame.event == "stream_started")
        .unwrap_or_else(|| panic!("no stream_started in {frames:?}"));
    (
        Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap(),
        Uuid::parse_str(started.data["message_id"].as_str().unwrap()).unwrap(),
    )
}

/// The frames of a finished SSE reply, asserting HTTP 200 and `text/event-stream`.
pub fn sse_of(reply: &Reply) -> Vec<SseFrame> {
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("text/event-stream"),
        "{} {}",
        reply.content_type,
        reply.text
    );
    parse_sse(&reply.text)
}

// ── Problem assertions ───────────────────────────────────────────────────────

/// A 409 `aborted` Problem with the given `context.reason`; returns the body.
pub fn assert_conflict(reply: &Reply, reason: &str) -> Value {
    let body = assert_problem(reply, 409, "aborted");
    assert_eq!(body["context"]["reason"], reason, "{body}");
    body
}

/// A 400 `failed_precondition` Problem with the violation `{subject, type: STATE}`.
pub fn assert_state_violation(reply: &Reply, subject: &str) -> Value {
    let body = assert_problem(reply, 400, "failed_precondition");
    let violations = body["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{body}");
    assert_eq!(violations[0]["subject"], subject, "{body}");
    assert_eq!(violations[0]["type"], "STATE", "{body}");
    body
}

/// A 404 `not_found` Problem of the given `context.resource_type`; returns the body.
pub fn assert_not_found_of(reply: &Reply, resource_type: &str) -> Value {
    let body = assert_problem(reply, 404, "not_found");
    assert_eq!(body["context"]["resource_type"], resource_type, "{body}");
    body
}

/// A 403 `permission_denied` Problem with the reason `AUTHZ_DENIED`.
pub fn assert_authz_denied(reply: &Reply) -> Value {
    let body = assert_problem(reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED", "{body}");
    body
}

/// A 500 `internal` Problem with a generic detail (no cause text).
pub fn assert_internal(reply: &Reply) -> Value {
    let body = assert_problem(reply, 500, "internal");
    let detail = body["detail"].as_str().unwrap_or_default().to_lowercase();
    for leak in ["sqlite", "constraint", "forced", "outbox"] {
        assert!(!detail.contains(leak), "{leak} leaked: {body}");
    }
    body
}

/// `context.resource_type` of a message not-found.
pub const MESSAGE_RESOURCE: &str = "gts.cf.core.mini_chat.message.v1~";
