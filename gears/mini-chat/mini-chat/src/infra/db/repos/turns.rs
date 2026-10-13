//! Turn persistence: the Secure ORM implementation of the
//! [`TurnRepository`] port of the stream pipeline
//! (`cpt-cf-mini-chat-feature-streaming-turns`).
//!
//! The reserve transaction is opened through
//! [`SeaQuotaRepository::reserve_with`] and the finalization transaction
//! through [`SeaQuotaRepository::finalize_turn_with`]; this module only adds
//! the statements that run in their closures. Child rows carry no owner
//! column: their queries use the tenant part of the scope plus the id of the
//! chat the owner-scoped load already authorized.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::quota::SeaQuotaRepository;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::reserve::ReservedColumns;
use crate::domain::service::stream::ports::{
    AssistantMessageWrite, AttachmentInfo, AttachmentKind, CreateTurnInput, FinalizeError,
    FinalizeWrites, HistoryMessage, HistoryRole, MessageBoundary, ProgressUpdate, RecentQuery,
    StoredAssistantMessage, ThreadSummaryInfo, TurnRepository,
};
use crate::domain::service::stream::types::{TurnRecord, TurnState};
use crate::domain::service::workers::summary_trigger::TriggerResult;
use crate::infra::db::entities::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, thread_summary,
};
use crate::infra::db::mapping::scope_error;
use crate::infra::db::repos::summaries::evaluate_summary_trigger;
use crate::infra::outbox::audit::enqueue_audit_event;
use crate::infra::outbox::queues::OutboxEnqueuer;

/// Secure ORM turn store.
pub struct SeaTurnRepository {
    db: Arc<DBProvider<DomainError>>,
    quota: Arc<SeaQuotaRepository>,
    outbox: Arc<OutboxEnqueuer>,
}

impl SeaTurnRepository {
    /// Creates the store over the database provider, the quota store (whose
    /// transactions it extends) and the outbox enqueuer (the audit queue).
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        quota: Arc<SeaQuotaRepository>,
        outbox: Arc<OutboxEnqueuer>,
    ) -> Self {
        Self { db, quota, outbox }
    }
}

fn turn_record(row: chat_turn::Model) -> Option<TurnRecord> {
    Some(TurnRecord {
        id: row.id,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        request_id: row.request_id,
        requester_user_id: row.requester_user_id,
        state: TurnState::parse(&row.state)?,
        assistant_message_id: row.assistant_message_id,
        error_code: row.error_code,
        effective_model: row.effective_model,
        started_at: row.started_at,
        updated_at: row.updated_at,
        deleted: row.deleted_at.is_some(),
    })
}

fn attachment_info(row: attachment::Model) -> AttachmentInfo {
    AttachmentInfo {
        id: row.id,
        kind: if row.attachment_kind == "image" {
            AttachmentKind::Image
        } else {
            AttachmentKind::Document
        },
        status: row.status,
        filename: row.filename,
        provider_file_id: row.provider_file_id,
        secondary_file_id: row.secondary_file_id,
        for_file_search: row.for_file_search,
        for_code_interpreter: row.for_code_interpreter,
        created_at: row.created_at,
    }
}

/// `(created_at, id)` strictly after `at`.
fn after(at: &MessageBoundary) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.gt(at.created_at))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(at.created_at))
                .add(message::Column::Id.gt(at.id)),
        )
}

/// `(created_at, id)` at most `at`.
fn at_most(at: &MessageBoundary) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.lt(at.created_at))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(at.created_at))
                .add(message::Column::Id.lte(at.id)),
        )
}

#[async_trait]
impl TurnRepository for SeaTurnRepository {
    async fn find_by_request_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-select-key
        let conn = self.db.conn()?;
        let row = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        Ok(row.and_then(turn_record))
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-select-key
    }

    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-select-running
        let row = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat_id))
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency:p1:inst-select-running
        Ok(row.and_then(turn_record))
    }

    async fn load_assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-load-assistant
        let row = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::Id.eq(message_id))
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-replay-completed:p1:inst-load-assistant
        Ok(row.map(|row| StoredAssistantMessage {
            id: row.id,
            content: row.content,
            input_tokens: u64::try_from(row.input_tokens).unwrap_or(0),
            output_tokens: u64::try_from(row.output_tokens).unwrap_or(0),
        }))
    }

    async fn request_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-request-attachments
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.is_in(ids.iter().copied()))
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-request-attachments
        Ok(rows.into_iter().map(attachment_info).collect())
    }

    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-ready-attachments
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat_id))
                    .add(attachment::Column::Status.eq("ready"))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(attachment::Column::CreatedAt, Order::Asc)
            .order_by(attachment::Column::Id, Order::Asc)
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-ready-attachments
        Ok(rows.into_iter().map(attachment_info).collect())
    }

    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-vector-store
        let row = chat_vector_store::Entity::find()
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-vector-store
        Ok(row.and_then(|row| row.vector_store_id))
    }

    async fn snapshot_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-snapshot-boundary
        let row = message::Entity::find()
            .filter(
                Condition::all()
                    .add(message::Column::ChatId.eq(chat_id))
                    .add(message::Column::RequestId.is_not_null())
                    .add(message::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(1)
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-snapshot-boundary
        Ok(row.map(|row| MessageBoundary {
            created_at: row.created_at,
            id: row.id,
        }))
    }

    async fn thread_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryInfo>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-summary
        let row = thread_summary::Entity::find()
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
            .secure()
            .scope_with(&scope.tenant_only())
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-summary
        Ok(row.map(|row| ThreadSummaryInfo {
            summary_text: row.summary_text,
            frontier: MessageBoundary {
                created_at: row.summarized_up_to_created_at,
                id: row.summarized_up_to_message_id,
            },
            token_estimate: row.token_estimate,
        }))
    }

    async fn recent_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<HistoryMessage>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-recent
        let mut filter = Condition::all()
            .add(message::Column::ChatId.eq(chat_id))
            .add(message::Column::RequestId.is_not_null())
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(message::Column::Role.is_in(["user", "assistant"]))
            .add(at_most(&query.boundary));
        if let Some(frontier) = &query.frontier {
            filter = filter.add(after(frontier));
        }
        let rows = message::Entity::find()
            .filter(filter)
            .secure()
            .scope_with(&scope.tenant_only())
            .order_by(message::Column::CreatedAt, Order::Desc)
            .order_by(message::Column::Id, Order::Desc)
            .limit(u64::from(query.limit))
            .all(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs:p1:inst-select-recent
        Ok(rows
            .into_iter()
            .rev()
            .map(|row| HistoryMessage {
                id: row.id,
                role: if row.role == "assistant" {
                    HistoryRole::Assistant
                } else {
                    HistoryRole::User
                },
                content: row.content,
                created_at: row.created_at,
            })
            .collect())
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-turn-creation:p1
    async fn create_turn(
        &self,
        input: &CreateTurnInput<'_>,
    ) -> Result<ReservedColumns, DomainError> {
        let plan = Arc::new(CreatePlan::from_input(input));
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-reserve
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-commit-reserve
        self.quota
            .reserve_with(&input.reserve, move |tx, columns| {
                let plan = Arc::clone(&plan);
                Box::pin(async move { plan.write(tx, columns).await })
            })
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-commit-reserve
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-reserve
    }

    async fn finalize(
        &self,
        request: &FinalizeRequest,
        writes: &FinalizeWrites,
    ) -> Result<FinalizeResult, FinalizeError> {
        let message_failed = Arc::new(AtomicBool::new(false));
        let trigger: Arc<Mutex<Option<TriggerResult>>> = Arc::new(Mutex::new(None));
        let wakes: Arc<Mutex<Vec<Wake>>> = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(writes.clone());
        let outbox = Arc::clone(&self.outbox);
        let result = {
            let (message_failed, wakes) = (Arc::clone(&message_failed), Arc::clone(&wakes));
            let trigger = Arc::clone(&trigger);
            self.quota
                .finalize_turn_with(request, move |tx, _won| {
                    let (writes, outbox) = (Arc::clone(&writes), Arc::clone(&outbox));
                    let (message_failed, wakes) = (Arc::clone(&message_failed), Arc::clone(&wakes));
                    let trigger = Arc::clone(&trigger);
                    Box::pin(async move {
                        message_failed.store(false, Ordering::SeqCst);
                        if let Ok(mut held) = trigger.lock() {
                            *held = None;
                        }
                        if let Some(message) = &writes.assistant_message {
                            insert_assistant_message(tx, message)
                                .await
                                .inspect_err(|_| {
                                    message_failed.store(true, Ordering::SeqCst);
                                })?;
                        }
                        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-enqueue-audit
                        let wake = enqueue_audit_event(tx, &outbox, &writes.audit).await?;
                        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-enqueue-audit
                        if let Ok(mut held) = wakes.lock() {
                            held.push(wake);
                        }
                        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-trigger-on-finalize
                        if let Some(summary) = &writes.summary {
                            let (result, wake) =
                                evaluate_summary_trigger(tx, &outbox, summary).await?;
                            if let Ok(mut held) = trigger.lock() {
                                *held = Some(result);
                            }
                            if let Some(wake) = wake
                                && let Ok(mut held) = wakes.lock()
                            {
                                held.push(wake);
                            }
                        }
                        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-trigger-on-finalize
                        Ok(())
                    })
                })
                .await
        };
        match result {
            Ok(mut result) => {
                if let Ok(mut held) = wakes.lock() {
                    held.drain(..).for_each(Wake::fire);
                }
                if let FinalizeResult::Won(won) = &mut result {
                    won.summary_trigger = trigger.lock().ok().and_then(|held| *held);
                }
                Ok(result)
            }
            Err(error) if message_failed.load(Ordering::SeqCst) => {
                Err(FinalizeError::MessageInsert(error))
            }
            Err(error) => Err(FinalizeError::Other(error)),
        }
    }

    async fn turn_state(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnState>, DomainError> {
        let conn = self.db.conn()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-check-turn-running
        let row = chat_turn::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::ChatId.eq(chat_id)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .one(&conn)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-run-provider-task:p1:inst-check-turn-running
        Ok(row.and_then(|row| TurnState::parse(&row.state)))
    }

    async fn record_progress(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
        update: &ProgressUpdate,
    ) -> Result<(), DomainError> {
        use chat_turn::Column as C;
        let conn = self.db.conn()?;
        let mut statement = chat_turn::Entity::update_many();
        if let Some(at) = update.at {
            statement = statement.col_expr(C::LastProgressAt, Expr::value(at));
        }
        for (column, count) in [
            (C::WebSearchCompletedCount, update.web_search_done),
            (
                C::CodeInterpreterCompletedCount,
                update.code_interpreter_done,
            ),
            (C::FileSearchCompletedCount, update.file_search_done),
        ] {
            if let Some(count) = count {
                statement = statement.col_expr(
                    column,
                    Expr::value(i32::try_from(count).unwrap_or(i32::MAX)),
                );
            }
        }
        statement
            .filter(
                Condition::all()
                    .add(C::Id.eq(turn_id))
                    .add(C::ChatId.eq(chat_id))
                    .add(C::State.eq("running")),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await
            .map(|_| ())
            .map_err(|error| scope_error(&error))
    }
}

/// What the reserve transaction writes besides the quota rows, owned so that
/// every attempt of the transaction starts from the same values.
struct CreatePlan {
    tenant_id: Uuid,
    user_id: Uuid,
    chat_id: Uuid,
    request_id: Uuid,
    turn_id: Uuid,
    user_message_id: Uuid,
    content: String,
    attachment_ids: Vec<Uuid>,
    web_search_enabled: bool,
    now: OffsetDateTime,
}

impl CreatePlan {
    fn from_input(input: &CreateTurnInput<'_>) -> Self {
        Self {
            tenant_id: input.reserve.tenant_id,
            user_id: input.reserve.user_id,
            chat_id: input.chat_id,
            request_id: input.request_id,
            turn_id: input.turn_id,
            user_message_id: input.user_message_id,
            content: input.content.to_owned(),
            attachment_ids: input.attachment_ids.to_vec(),
            web_search_enabled: input.web_search_enabled,
            now: input.reserve.now,
        }
    }

    /// The statements of `cpt-cf-mini-chat-algo-streaming-turns-create-turn`
    /// after the quota statements, in the same transaction.
    async fn write(&self, tx: &DbTx<'_>, columns: &ReservedColumns) -> Result<(), DomainError> {
        let tenant = AccessScope::for_tenant(self.tenant_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-user-message
        let user_message = message::ActiveModel {
            id: Set(self.user_message_id),
            tenant_id: Set(self.tenant_id),
            chat_id: Set(self.chat_id),
            request_id: Set(Some(self.request_id)),
            role: Set("user".to_owned()),
            content: Set(self.content.clone()),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(None),
            request_kind: Set("chat".to_owned()),
            features_used: Set(serde_json::json!([])),
            input_tokens: Set(0),
            output_tokens: Set(0),
            cache_read_input_tokens: Set(0),
            cache_write_input_tokens: Set(0),
            reasoning_tokens: Set(0),
            model: Set(None),
            is_compressed: Set(false),
            created_at: Set(self.now),
            deleted_at: Set(None),
        };
        insert::<message::Entity>(tx, &tenant, user_message).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-user-message
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-touch-chat
        chat::Entity::update_many()
            .col_expr(chat::Column::UpdatedAt, Expr::value(self.now))
            .filter(Condition::all().add(chat::Column::Id.eq(self.chat_id)))
            .secure()
            .scope_with(&AccessScope::for_tenant(self.tenant_id).ensure_owner(self.user_id))
            .exec(tx)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-touch-chat
        if !self.attachment_ids.is_empty() {
            self.check_and_link_attachments(tx, &tenant).await?;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-turn
        let turn = chat_turn::ActiveModel {
            id: Set(self.turn_id),
            tenant_id: Set(self.tenant_id),
            chat_id: Set(self.chat_id),
            request_id: Set(self.request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(self.user_id)),
            state: Set("running".to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(Some(columns.reserve_tokens)),
            max_output_tokens_applied: Set(Some(
                i32::try_from(columns.max_output_tokens_applied).map_err(|_| {
                    DomainError::Internal("max_output_tokens_applied out of range".to_owned())
                })?,
            )),
            reserved_credits_micro: Set(Some(columns.reserved_credits_micro)),
            policy_version_applied: Set(Some(
                i64::try_from(columns.policy_version_applied)
                    .map_err(|_| DomainError::Internal("policy_version out of range".to_owned()))?,
            )),
            effective_model: Set(Some(columns.effective_model.clone())),
            minimal_generation_floor_applied: Set(Some(
                i32::try_from(columns.minimal_generation_floor_applied)
                    .map_err(|_| DomainError::Internal("floor out of range".to_owned()))?,
            )),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(self.now),
            last_progress_at: Set(Some(self.now)),
            web_search_enabled: Set(self.web_search_enabled),
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            completed_at: Set(None),
            updated_at: Set(self.now),
        };
        insert::<chat_turn::Entity>(tx, &tenant, turn).await
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-turn
    }

    /// The in-transaction attachment check and the `message_attachments` rows.
    async fn check_and_link_attachments(
        &self,
        tx: &DbTx<'_>,
        tenant: &AccessScope,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-select-attachments-tx
        let rows = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::Id.is_in(self.attachment_ids.iter().copied()))
                    .add(attachment::Column::ChatId.eq(self.chat_id))
                    .add(attachment::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(tenant)
            .all(tx)
            .await
            .map_err(|error| scope_error(&error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-select-attachments-tx
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-attachment-invalid
        let valid = self.attachment_ids.iter().all(|id| {
            rows.iter().any(|row| {
                row.id == *id && row.uploaded_by_user_id == self.user_id && row.status == "ready"
            })
        });
        if !valid {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-attachment-invalid
            return Err(DomainError::InvalidAttachmentIds);
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-return-attachment-invalid
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-if-attachment-invalid
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-links
        for id in &self.attachment_ids {
            let link = message_attachment::ActiveModel {
                tenant_id: Set(self.tenant_id),
                chat_id: Set(self.chat_id),
                message_id: Set(self.user_message_id),
                attachment_id: Set(*id),
                created_at: Set(self.now),
            };
            insert::<message_attachment::Entity>(tx, tenant, link).await?;
        }
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-create-turn:p1:inst-insert-links
    }
}

/// One Secure ORM INSERT; a unique violation is the typed
/// [`DomainError::UniqueViolation`] (see `scope_error`).
async fn insert<E>(
    tx: &DbTx<'_>,
    scope: &AccessScope,
    model: E::ActiveModel,
) -> Result<(), DomainError>
where
    E: sea_orm::EntityTrait + toolkit_db::secure::ScopableEntity,
    E::ActiveModel: Clone + Send + sea_orm::ActiveModelTrait<Entity = E>,
{
    E::insert(model.clone())
        .secure()
        .scope_with_model(scope, &model)
        .map_err(|error| match error {
            ScopeError::Denied(_) | ScopeError::TenantNotInScope { .. } => DomainError::AuthzDenied,
            other => scope_error(&other),
        })?
        .exec(tx)
        .await
        .map(|_| ())
        .map_err(|error| scope_error(&error))
}

async fn insert_assistant_message(
    tx: &DbTx<'_>,
    write: &AssistantMessageWrite,
) -> Result<(), DomainError> {
    let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    let usage = write.usage.unwrap_or(mini_chat_sdk::UsageTokens {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    });
    let row = message::ActiveModel {
        id: Set(write.id),
        tenant_id: Set(write.tenant_id),
        chat_id: Set(write.chat_id),
        request_id: Set(Some(write.request_id)),
        role: Set("assistant".to_owned()),
        content: Set(write.content.clone()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(write.provider_response_id.clone()),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(count(usage.input_tokens)),
        output_tokens: Set(count(usage.output_tokens)),
        cache_read_input_tokens: Set(count(usage.cache_read_input_tokens)),
        cache_write_input_tokens: Set(count(usage.cache_write_input_tokens)),
        reasoning_tokens: Set(count(usage.reasoning_tokens)),
        model: Set(Some(write.model.clone())),
        is_compressed: Set(false),
        created_at: Set(write.created_at),
        deleted_at: Set(None),
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-insert-assistant
    insert::<message::Entity>(tx, &AccessScope::for_tenant(write.tenant_id), row).await
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-finalize-stream:p1:inst-insert-assistant
}

#[cfg(test)]
#[path = "turns_tests.rs"]
mod tests;
