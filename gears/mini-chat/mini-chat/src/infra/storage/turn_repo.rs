//! Turn and attachment reads of the send pipeline and the turn status API over the Secure ORM
//! (slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`). Every function takes
//! the caller's `runner` and reaches its rows only through a [`ChatRecord`] and its tenant.

use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, Set};
use time::{OffsetDateTime, UtcOffset};
use toolkit_db::secure::{DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quota::{PreflightDecision, ToolCallCounts};
use crate::domain::streaming::AttachmentFact;
use crate::domain::streaming::audit::OrphanTurnFacts;
use crate::domain::turns::{TurnRow, TurnState};
use crate::infra::storage::entity::{attachment, chat_turn};
use crate::infra::storage::keyset_page::{TimeStorage, time_bound, time_key};
use crate::infra::storage::repo_impl::{ChatRecord, child_scope};

pub(crate) fn turn_row(model: chat_turn::Model) -> Result<TurnRow, DomainError> {
    let state = TurnState::parse(&model.state).ok_or_else(|| {
        DomainError::internal(format!(
            "turn {} has the unknown state `{}`",
            model.id, model.state
        ))
    })?;
    Ok(TurnRow {
        id: model.id,
        chat_id: model.chat_id,
        request_id: model.request_id,
        requester_user_id: model.requester_user_id,
        state,
        assistant_message_id: model.assistant_message_id,
        error_code: model.error_code,
        effective_model: model.effective_model,
        deleted_at: model.deleted_at,
        started_at: model.started_at,
        updated_at: model.updated_at,
    })
}

/// The turn `(chat_id, request_id)` of the chat, with or without the soft-deleted row.
async fn find_turn(
    runner: &impl DBRunner,
    chat: &ChatRecord,
    request_id: Uuid,
    include_deleted: bool,
) -> Result<Option<TurnRow>, DomainError> {
    let mut filter = Condition::all()
        .add(chat_turn::Column::ChatId.eq(chat.id))
        .add(chat_turn::Column::RequestId.eq(request_id));
    if !include_deleted {
        filter = filter.add(chat_turn::Column::DeletedAt.is_null());
    }
    chat_turn::Entity::find()
        .secure()
        .scope_with(&child_scope(chat))
        .filter(filter)
        .one(runner)
        .await?
        .map(turn_row)
        .transpose()
}

/// Turn reads.
pub struct TurnRepo;

impl TurnRepo {
    /// The `(chat_id, request_id)` turn, soft-deleted rows included (the idempotency lookup).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn find_by_key(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        find_turn(runner, chat, request_id, true).await
    }

    /// The non-deleted `(chat_id, request_id)` turn (the status API).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn find_active(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        find_turn(runner, chat, request_id, false).await
    }

    /// Whether the chat has any `running` turn with `deleted_at IS NULL`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn has_running(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<bool, DomainError> {
        // `chat_turns` is the only source of running state, whatever the `request_id`
        let running = chat_turn::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat.id))
                    .add(chat_turn::Column::State.eq(TurnState::Running.as_str()))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .count(runner)
            .await?;
        Ok(running > 0)
    }
}

/// The replacement turn of a retry or an edit, as the mutation transaction inserts it.
#[derive(Debug, Clone, Copy)]
pub struct UnreservedTurn {
    pub id: Uuid,
    pub request_id: Uuid,
    /// The requester: the caller of the mutation.
    pub user_id: Uuid,
    pub web_search_enabled: bool,
    /// The preflight instant of the mutation preflight decision.
    pub started_at: OffsetDateTime,
    /// The insert instant (`last_progress_at` and `updated_at`).
    pub now: OffsetDateTime,
}

impl TurnRepo {
    /// The latest turn of the chat: `deleted_at IS NULL`, the greatest `(started_at, id)`, any
    /// state; `started_at` goes through the fixed-width key on `SQLite`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn latest(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        storage: TimeStorage,
    ) -> Result<Option<TurnRow>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-select
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-sort-key
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-every-state
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-skip-deleted
        let rows = chat_turn::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat.id))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .order_by(
                time_key(storage, Expr::col(chat_turn::Column::StartedAt)),
                Order::Desc,
            )
            .order_by(chat_turn::Column::Id, Order::Desc)
            .limit(1)
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-skip-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-every-state
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-sort-key
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-select
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-return
        rows.into_iter().next().map(turn_row).transpose()
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn:p1:inst-lt-return
    }

    /// The raw row of the turn `(chat_id, request_id)`, soft-deleted rows included.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn find_model(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
    ) -> Result<Option<chat_turn::Model>, DomainError> {
        Ok(chat_turn::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat.id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .one(runner)
            .await?)
    }

    /// The guarded soft-delete of a mutation: `deleted_at` and `updated_at` set to `now` and, for
    /// a retry or an edit, `replaced_by_request_id`; `true` when the guard `deleted_at IS NULL`
    /// matched the row.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn soft_delete(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
        replaced_by: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let affected = chat_turn::Entity::update_many()
            .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .col_expr(
                chat_turn::Column::ReplacedByRequestId,
                Expr::value(replaced_by),
            )
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat.id))
                    .add(chat_turn::Column::RequestId.eq(request_id))
                    .add(chat_turn::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?
            .rows_affected;
        Ok(affected > 0)
    }

    /// Take the mutation lock on the turn `(chat_id, request_id)` with a write that changes
    /// nothing: on `SQLite` any write statement takes the database write lock before the
    /// transaction's first read, on `PostgreSQL` it holds the row lock of the target until the
    /// transaction ends (the secure query API has no `FOR UPDATE`). Returns the rows matched.
    ///
    /// # Errors
    ///
    /// A [`ScopeError`] when the database fails; the driver error is kept, so the transaction
    /// retry helper can recognize lock contention.
    pub async fn lock_for_mutation(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        request_id: Uuid,
    ) -> Result<u64, ScopeError> {
        Ok(chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::UpdatedAt,
                Expr::col(chat_turn::Column::UpdatedAt),
            )
            .filter(
                Condition::all()
                    .add(chat_turn::Column::ChatId.eq(chat.id))
                    .add(chat_turn::Column::RequestId.eq(request_id)),
            )
            .secure()
            .scope_with(&child_scope(chat))
            .exec(runner)
            .await?
            .rows_affected)
    }

    /// Insert the replacement turn of a retry or an edit: `running`, the requester, `started_at`
    /// the preflight instant, `last_progress_at` and `updated_at` the insert instant, the carried
    /// web search flag, the six preflight columns NULL, no provider name and zero counters.
    ///
    /// # Errors
    ///
    /// [`DomainError::UniqueViolation`] when `UNIQUE(chat_id, request_id)` or the running-turn
    /// index is violated; another [`DomainError`] when the database fails.
    pub async fn insert_unreserved(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        turn: UnreservedTurn,
    ) -> Result<(), DomainError> {
        let row = chat_turn::ActiveModel {
            id: Set(turn.id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(turn.request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(turn.user_id)),
            state: Set(TurnState::Running.as_str().to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(None),
            max_output_tokens_applied: Set(None),
            reserved_credits_micro: Set(None),
            policy_version_applied: Set(None),
            effective_model: Set(None),
            minimal_generation_floor_applied: Set(None),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(turn.started_at),
            last_progress_at: Set(Some(turn.now)),
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-counters
            web_search_enabled: Set(turn.web_search_enabled),
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-tool-usage-carry-forward:p1:inst-tcf-counters
            completed_at: Set(None),
            updated_at: Set(turn.now),
        };
        secure_insert::<chat_turn::Entity>(row, &child_scope(chat), runner).await?;
        Ok(())
    }

    /// The unstarted-turn failure: a plain CAS `running` to `failed` with the code, the detail and
    /// `completed_at` and `updated_at` set to `now`; `true` when this call won the CAS.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn fail_running(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        turn_id: Uuid,
        code: &str,
        detail: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-cas
        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-immutable
        let affected = chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::State,
                Expr::value(TurnState::Failed.as_str()),
            )
            .col_expr(
                chat_turn::Column::ErrorCode,
                Expr::value(Some(code.to_owned())),
            )
            .col_expr(
                chat_turn::Column::ErrorDetail,
                Expr::value(Some(detail.to_owned())),
            )
            .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
            .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all()
                    .add(chat_turn::Column::Id.eq(turn_id))
                    .add(chat_turn::Column::State.eq(TurnState::Running.as_str())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?
            .rows_affected;
        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-immutable
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure:p1:inst-uf-cas
        Ok(affected > 0)
    }
}

impl TurnRepo {
    /// Set `provider_response_id` of a turn of the tenant (the provider response id is kept
    /// internally, on the turn and on the assistant message).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn set_provider_response_id(
        runner: &impl DBRunner,
        tenant_id: Uuid,
        turn_id: Uuid,
        response_id: &str,
    ) -> Result<(), DomainError> {
        chat_turn::Entity::update_many()
            .col_expr(
                chat_turn::Column::ProviderResponseId,
                Expr::value(Some(response_id.to_owned())),
            )
            .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(runner)
            .await?;
        Ok(())
    }

    /// The stale candidates of the orphan watchdog across every tenant: `running`, not
    /// soft-deleted, `COALESCE(last_progress_at, started_at)` at most `cutoff`, oldest progress
    /// first, at most `limit`; on `SQLite` the comparison goes through the fixed-width key.
    ///
    /// The scan is the runtime-owned privileged access of the leader-only watchdog (no user and
    /// no tenant exists): every finalization that follows runs inside the tenant of its row.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn stale_running(
        runner: &impl DBRunner,
        cutoff: OffsetDateTime,
        limit: u64,
        storage: TimeStorage,
    ) -> Result<Vec<OrphanTurnFacts>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-scan
        let alive_since = || {
            time_key(
                storage,
                Expr::expr(Func::coalesce([
                    Expr::col((chat_turn::Entity, chat_turn::Column::LastProgressAt)),
                    Expr::col((chat_turn::Entity, chat_turn::Column::StartedAt)),
                ])),
            )
        };
        let bound = time_bound(storage, cutoff)
            .map_err(|e| DomainError::internal(format!("cannot bind the orphan cutoff: {e}")))?;
        let rows = chat_turn::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::State.eq(TurnState::Running.as_str()))
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(alive_since().lte(bound)),
            )
            .order_by(alive_since(), Order::Asc)
            .order_by(chat_turn::Column::Id, Order::Asc)
            .limit(limit)
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-scan
        Ok(rows
            .into_iter()
            .map(|row| OrphanTurnFacts {
                tenant_id: row.tenant_id,
                // a turn without a requester has no user to name; the audit event carries nil
                user_id: row.requester_user_id.unwrap_or_else(Uuid::nil),
                chat_id: row.chat_id,
                turn_id: row.id,
                request_id: row.request_id,
                effective_model: row.effective_model.unwrap_or_default(),
                started_at: row.started_at.to_offset(UtcOffset::UTC),
                tool_counts: ToolCallCounts {
                    web_search: u32::try_from(row.web_search_completed_count).unwrap_or(0),
                    code_interpreter: u32::try_from(row.code_interpreter_completed_count)
                        .unwrap_or(0),
                    file_search: u32::try_from(row.file_search_completed_count).unwrap_or(0),
                },
            })
            .collect())
    }
}

/// The input of the insert of a new `running` turn.
#[derive(Debug, Clone)]
pub struct NewTurn<'a> {
    pub id: Uuid,
    pub request_id: Uuid,
    /// The requester (`requester_type` is always `user`).
    pub user_id: Uuid,
    pub web_search_enabled: bool,
    /// The preflight columns and `started_at` (the preflight instant).
    pub decision: &'a PreflightDecision,
    /// The insert instant: `last_progress_at` and `updated_at`.
    pub now: OffsetDateTime,
}

/// The `i32` of a preflight column.
fn column_i32(value: i64, column: &str) -> Result<i32, DomainError> {
    i32::try_from(value)
        .map_err(|_| DomainError::internal(format!("`{column}` does not fit in an i32")))
}

impl TurnRepo {
    /// Insert the `running` turn of a new send (`tenant_id` and `chat_id` from the resolved
    /// chat).
    ///
    /// # Errors
    ///
    /// [`DomainError::UniqueViolation`] when `UNIQUE(chat_id, request_id)` or the running-turn
    /// index is violated; another [`DomainError`] when the database fails.
    pub async fn insert_running(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        new: &NewTurn<'_>,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-turn
        // `running`, the requester, `started_at` equal to the preflight instant of the decision,
        // `last_progress_at` equal to the insert instant, the requested web search flag, the
        // preflight columns, no provider name and zero counters
        let decision = new.decision;
        let row = chat_turn::ActiveModel {
            id: Set(new.id),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(new.request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(new.user_id)),
            state: Set(TurnState::Running.as_str().to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(Some(decision.reserve_tokens)),
            max_output_tokens_applied: Set(Some(column_i32(
                decision.max_output_tokens_applied,
                "max_output_tokens_applied",
            )?)),
            reserved_credits_micro: Set(Some(decision.reserved_credits_micro)),
            policy_version_applied: Set(Some(
                i64::try_from(decision.policy_version_applied).map_err(|_| {
                    DomainError::internal("`policy_version_applied` does not fit in an i64")
                })?,
            )),
            effective_model: Set(Some(decision.effective_model.clone())),
            minimal_generation_floor_applied: Set(Some(column_i32(
                decision.minimal_generation_floor_applied,
                "minimal_generation_floor_applied",
            )?)),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(decision.started_at),
            last_progress_at: Set(Some(new.now)),
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-store-flag
            web_search_enabled: Set(new.web_search_enabled),
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-store-flag
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            completed_at: Set(None),
            updated_at: Set(new.now),
        };
        secure_insert::<chat_turn::Entity>(row, &child_scope(chat), runner).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-reserve-transaction:p1:inst-rtx-turn
        Ok(())
    }
}

/// Attachment reads of the preflight.
pub struct AttachmentRepo;

impl AttachmentRepo {
    /// The attachments referenced by `attachment_ids` that exist in the chat and are not
    /// soft-deleted, in the order of `attachment_ids` (any status); an unknown id is absent.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn referenced(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        attachment_ids: &[Uuid],
    ) -> Result<Vec<AttachmentFact>, DomainError> {
        if attachment_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(chat.id))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::Id.is_in(attachment_ids.to_vec())),
            )
            .all(runner)
            .await?;
        Ok(attachment_ids
            .iter()
            .filter_map(|id| rows.iter().find(|row| row.id == *id))
            .map(|row| AttachmentFact {
                id: row.id,
                kind: row.attachment_kind.clone(),
                provider_file_id: row.provider_file_id.clone(),
                secondary_file_id: row.secondary_file_id.clone(),
                secondary_status: row.secondary_status.clone(),
            })
            .collect())
    }

    /// The attachment validation of the reserve transaction, inside its transaction: every
    /// referenced id must exist, belong to the tenant of the chat, be uploaded by `user_id`,
    /// belong to the chat, not be soft-deleted and have status `ready`.
    ///
    /// # Errors
    ///
    /// [`DomainError::InvalidAttachment`] when any id is rejected; another [`DomainError`] when
    /// the database fails.
    pub async fn validate_for_send(
        runner: &impl DBRunner,
        chat: &ChatRecord,
        user_id: Uuid,
        attachment_ids: &[Uuid],
    ) -> Result<(), DomainError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-validation
        // `attachment_ids` never scopes or filters retrieval: they are only validated here
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-select
        // the tenant predicate of the scope hides the attachments of another tenant, which
        // are therefore indistinguishable from an unknown id
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(&child_scope(chat))
            .filter(Condition::all().add(attachment::Column::Id.is_in(attachment_ids.to_vec())))
            .all(runner)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-select
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-each
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-check
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-rejected
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-return-rejected
        // no row, another uploader, another chat, a soft-deleted row or a status other than
        // `ready` (an `uploaded` document still indexing is not ready) rejects the request; the
        // caller rolls the reserve and the user message back and maps it to 400
        // `invalid_attachment`
        let accepted = attachment_ids.iter().all(|id| {
            rows.iter().find(|row| row.id == *id).is_some_and(|row| {
                row.tenant_id == chat.tenant_id
                    && row.uploaded_by_user_id == user_id
                    && row.chat_id == chat.id
                    && row.deleted_at.is_none()
                    && row.status == "ready"
            })
        });
        if !accepted {
            return Err(DomainError::InvalidAttachment);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-return-rejected
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-rejected
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-check
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-attachment-validation:p1:inst-att-each
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-validation
        Ok(())
    }

    /// The facts of the quota estimate and the send preflight, with the predicates of the ready
    /// document fact (ready, non-deleted attachments with `for_file_search`) and the ready code
    /// interpreter fact (ready, non-deleted attachments with `for_code_interpreter` and a
    /// provider file id) of FEATURE 0005, so the surcharge and the tool appear together.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    pub async fn chat_facts(
        runner: &impl DBRunner,
        chat: &ChatRecord,
    ) -> Result<(u32, u32), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-facts
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-consumers
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-consumers
        let ready = Condition::all()
            .add(attachment::Column::ChatId.eq(chat.id))
            .add(attachment::Column::DeletedAt.is_null())
            .add(attachment::Column::Status.eq("ready"));
        let count = |filter: Condition| async move {
            let count = attachment::Entity::find()
                .secure()
                .scope_with(&child_scope(chat))
                .filter(filter)
                .count(runner)
                .await?;
            u32::try_from(count)
                .map_err(|_| DomainError::internal("the attachment count does not fit in a u32"))
        };
        // the two predicates of FEATURE 0005: never `attachment_kind`
        let documents = count(
            ready
                .clone()
                .add(attachment::Column::ForFileSearch.eq(true)),
        )
        .await?;
        let code_interpreter = count(
            ready
                .add(attachment::Column::ForCodeInterpreter.eq(true))
                .add(attachment::Column::ProviderFileId.is_not_null()),
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-code-interpreter-fact:p1:inst-rci-consumers
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-ready-document-fact:p1:inst-rdf-consumers
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-facts
        Ok((documents, code_interpreter))
    }
}

#[cfg(test)]
#[path = "turn_repo_tests.rs"]
mod turn_repo_tests;
