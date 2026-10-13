//! The reserve transaction (DESIGN 3.6 "Single reserve transaction", 5.7 "User
//! message persistence invariant"): the user message, its attachment links,
//! the `chats.updated_at` bump and the `running` turn commit together, before
//! any provider call, or not at all.
//!
//! [`reserve_turn_tx`] also books the quota reserve (DESIGN 5.4.3): the reserve
//! columns of [`NewTurn`], the `quota_usage` increments (atomic upserts, so
//! they never raise a unique violation) and the re-check of the limits after
//! the increments (DESIGN 5.4.2 "TOCTOU"). A request that fails the re-check
//! rolls back with 429 and leaves no turn, message or reserve behind.

use std::sync::Arc;

use mini_chat_sdk::{ModelTier, UserLimits};
use time::OffsetDateTime;
use tokio::time::Instant;
use toolkit_db::DbTx;
use toolkit_db::secure::TxConfig;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::preflight::{FileTarget, TurnPlan};
use super::run::TurnRun;
use super::{SendMessageCommand, StreamService, TurnEvents, TurnScopes};
use crate::domain::chats::{ChatRow, chat_not_found};
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::messages::{MessageRole, NewMessage};
use crate::domain::ports::{
    AttachmentRepository, ChatRepository, MessageRepository, NewMessageAttachment, QuotaOwner,
    QuotaRepository, TurnLifecycleRepository, TurnRepository,
};
use crate::domain::provider::TokenBreakdown;
use crate::domain::quota::{
    Bucket, PeriodStarts, QuotaUsage, TurnReserve, minimal_generation_floor_applied, tier_available,
};
use crate::domain::settlement::TurnLedger;
use crate::domain::turns::{NewTurn, TurnPreflightColumns, TurnRow};

/// A turn that was committed and is ready to stream.
pub(super) struct AdmittedTurn {
    pub(super) plan: TurnPlan,
    pub(super) chat: ChatRow,
    pub(super) request_id: Uuid,
    pub(super) turn_id: Uuid,
    pub(super) assistant_message_id: Uuid,
    pub(super) user_created_at: time::OffsetDateTime,
}

/// Result of the reserve step.
pub(super) enum Reserved {
    /// The turn was committed and is ready to stream.
    Admitted(Box<AdmittedTurn>),
    /// A concurrent request with the same `request_id` won the insert race;
    /// the caller serves it like any other existing key (replay when
    /// completed, `request_id_conflict` otherwise).
    Existing(Box<TurnRow>),
}

/// Why a send cannot start a new turn although the keys looked free.
pub(super) enum Contention {
    /// A turn with the request's `(chat_id, request_id)` key exists (in any
    /// state): idempotency wins over the parallel-turn guard.
    SameKey(TurnRow),
    /// Only another request's turn runs in the chat: `turn_already_running`.
    Parallel,
}

/// The insert lost a unique-index race (DESIGN 3.3 "Idempotency": the stream
/// service re-reads the key). The key is read first: a turn with this
/// `request_id` -> same-key, otherwise the loss was on the one-running-turn
/// index. The running turn is deliberately not re-read: its winner may have
/// finalized meanwhile, and the answer is `turn_already_running` anyway.
pub(super) async fn resolve_lost_insert<R: TurnRepository>(
    conn: &R,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Contention, DomainError> {
    Ok(match conn.find_turn(scope, chat_id, request_id).await? {
        Some(turn) => Contention::SameKey(turn),
        None => Contention::Parallel,
    })
}

/// The parallel-turn guard: `None` when no turn runs in the chat. When one
/// does, it is `SameKey` if it carries this `request_id` (a concurrent send
/// of the same key committed between the key lookup and this read), else the
/// key is re-read once before answering `Parallel`.
pub(super) async fn running_guard<R: TurnRepository>(
    conn: &R,
    scope: &AccessScope,
    chat_id: Uuid,
    request_id: Uuid,
) -> Result<Option<Contention>, DomainError> {
    let Some(running) = conn.find_running_turn(scope, chat_id).await? else {
        return Ok(None);
    };
    if running.request_id == request_id {
        return Ok(Some(Contention::SameKey(running)));
    }
    resolve_lost_insert(conn, scope, chat_id, request_id)
        .await
        .map(Some)
}

/// The quota part of the reserve transaction.
#[derive(Clone)]
struct QuotaReserve {
    scope: AccessScope,
    owner: QuotaOwner,
    tier: ModelTier,
    starts: PeriodStarts,
    credits_micro: i64,
    limits: UserLimits,
}

/// Everything the reserve transaction writes (cloned per attempt).
#[derive(Clone)]
struct ReserveInput {
    owner_scope: AccessScope,
    tenant_scope: AccessScope,
    chat_id: Uuid,
    user_id: Uuid,
    turn: NewTurn,
    quota: QuotaReserve,
    message: NewMessage,
    attachment_ids: Vec<Uuid>,
    links: Vec<NewMessageAttachment>,
}

impl StreamService {
    pub(super) async fn reserve(
        &self,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        request_id: Uuid,
        command: &SendMessageCommand,
        plan: TurnPlan,
    ) -> Result<Reserved, DomainError> {
        let (owner_scope, tenant_scope) = (&scopes.owner, &scopes.tenant);
        let now = now_utc();
        let turn_id = Uuid::new_v4();
        let user_message_id = Uuid::new_v4();
        let reserve = plan.reserve;
        let input = ReserveInput {
            owner_scope: owner_scope.clone(),
            tenant_scope: tenant_scope.clone(),
            chat_id: chat.id,
            user_id: ctx.subject_id(),
            turn: NewTurn {
                id: turn_id,
                tenant_id: chat.tenant_id,
                chat_id: chat.id,
                request_id,
                requester_user_id: ctx.subject_id(),
                effective_model: plan.entry.id.clone(),
                policy_version_applied: plan.policy_version,
                reserve_tokens: reserve.reserve_tokens,
                reserved_credits_micro: reserve.reserved_credits_micro,
                max_output_tokens_applied: reserve.max_output_tokens_applied,
                minimal_generation_floor_applied: minimal_generation_floor_applied(
                    self.config.estimation_budgets.minimal_generation_floor,
                    reserve.max_output_tokens_applied,
                ),
                web_search_enabled: plan.web_search_requested,
                now,
            },
            quota: QuotaReserve {
                scope: scopes.quota.clone(),
                owner: QuotaOwner {
                    tenant_id: chat.tenant_id,
                    user_id: ctx.subject_id(),
                },
                tier: plan.tier,
                starts: plan.starts,
                credits_micro: reserve.reserved_credits_micro,
                limits: plan.limits.clone(),
            },
            message: NewMessage {
                id: user_message_id,
                tenant_id: chat.tenant_id,
                chat_id: chat.id,
                request_id,
                role: MessageRole::User,
                content: command.content.clone(),
                model: None,
                provider_response_id: None,
                input_tokens: 0,
                output_tokens: 0,
                breakdown: TokenBreakdown::default(),
                created_at: now,
            },
            attachment_ids: plan.attachment_ids.clone(),
            links: plan
                .attachment_ids
                .iter()
                .map(|attachment_id| NewMessageAttachment {
                    tenant_id: chat.tenant_id,
                    chat_id: chat.id,
                    message_id: user_message_id,
                    attachment_id: *attachment_id,
                    now,
                })
                .collect(),
        };

        // The first statement of the transaction is a write: a SQLite
        // transaction that starts with a read and then upgrades fails with
        // SQLITE_BUSY_SNAPSHOT when another writer committed in between.
        // Retryable contention errors rerun the whole transaction.
        let committed = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let input = input.clone();
                Box::pin(async move { reserve_turn_tx(tx, &input).await })
            })
            .await;
        match committed {
            Ok(()) => {}
            Err(DomainError::Conflict { .. }) => {
                // The only unique indexes the first statement can violate are
                // `(chat_id, request_id)` and the one-running-turn-per-chat
                // index; both are classified, never surfaced as a raw
                // unique violation.
                let conn = self.db.conn()?;
                return match resolve_lost_insert(&conn, tenant_scope, chat.id, request_id).await? {
                    Contention::SameKey(turn) => Ok(Reserved::Existing(Box::new(turn))),
                    Contention::Parallel => Err(DomainError::TurnAlreadyRunning),
                };
            }
            Err(error) => return Err(error),
        }
        Ok(Reserved::Admitted(Box::new(AdmittedTurn {
            plan,
            chat: chat.clone(),
            request_id,
            turn_id,
            assistant_message_id: Uuid::new_v4(),
            user_created_at: now,
        })))
    }

    /// The reserve of a retry / edit turn, whose `running` row the mutation
    /// commit already inserted: one transaction writes the preflight columns
    /// of the row and books the quota reserve with its limit re-check (429
    /// `tokens` rolls both back). It is the last step before the provider call.
    pub(super) async fn reserve_started(
        &self,
        ctx: &SecurityContext,
        scopes: &TurnScopes,
        chat: &ChatRow,
        turn_id: Uuid,
        plan: &TurnPlan,
    ) -> Result<(), DomainError> {
        let now = now_utc();
        let reserve = plan.reserve;
        let columns = TurnPreflightColumns {
            effective_model: plan.entry.id.clone(),
            policy_version_applied: plan.policy_version,
            reserve_tokens: reserve.reserve_tokens,
            reserved_credits_micro: reserve.reserved_credits_micro,
            max_output_tokens_applied: reserve.max_output_tokens_applied,
            minimal_generation_floor_applied: minimal_generation_floor_applied(
                self.config.estimation_budgets.minimal_generation_floor,
                reserve.max_output_tokens_applied,
            ),
            now,
        };
        let quota = QuotaReserve {
            scope: scopes.quota.clone(),
            owner: QuotaOwner {
                tenant_id: chat.tenant_id,
                user_id: ctx.subject_id(),
            },
            tier: plan.tier,
            starts: plan.starts,
            credits_micro: reserve.reserved_credits_micro,
            limits: plan.limits.clone(),
        };
        let tenant_scope = scopes.tenant.clone();
        // The first statement is a write (see `reserve`).
        self.db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (columns, quota, tenant_scope) =
                    (columns.clone(), quota.clone(), tenant_scope.clone());
                Box::pin(async move {
                    if !tx
                        .record_turn_reserve(&tenant_scope, turn_id, &columns)
                        .await?
                    {
                        return Err(DomainError::internal(format!(
                            "turn {turn_id} is no longer running"
                        )));
                    }
                    reserve_quota_tx(tx, &quota, columns.now).await
                })
            })
            .await
    }

    /// Hands a committed turn to its provider task and returns the event
    /// stream. The bounded channel is the only coupling between the provider
    /// and the client: a slow reader blocks the provider task, a dropped
    /// reader cancels the turn.
    pub(super) fn launch(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        scopes: TurnScopes,
        admitted: AdmittedTurn,
    ) -> TurnEvents {
        let AdmittedTurn {
            plan,
            chat,
            request_id,
            turn_id,
            assistant_message_id,
            user_created_at,
        } = admitted;
        let capacity = usize::from(self.config.streaming.sse_channel_capacity);
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        let files: Vec<FileTarget> = plan.files;
        let (summary_applied, trigger) = (plan.summary_applied, plan.trigger);
        let ledger = TurnLedger {
            tenant_id: chat.tenant_id,
            user_id: ctx.subject_id(),
            chat_id: chat.id,
            turn_id,
            request_id,
            selected_model: chat.model.clone(),
            effective_model: plan.entry.id.clone(),
            tier: plan.tier,
            in_mult: plan.entry.input_tokens_credit_multiplier_micro,
            out_mult: plan.entry.output_tokens_credit_multiplier_micro,
            policy_version: plan.policy_version,
            reserve: TurnReserve {
                reserve_tokens: plan.reserve.reserve_tokens,
                max_output_tokens_applied: plan.reserve.max_output_tokens_applied,
                reserved_credits_micro: plan.reserve.reserved_credits_micro,
                minimal_generation_floor_applied: minimal_generation_floor_applied(
                    self.config.estimation_budgets.minimal_generation_floor,
                    plan.reserve.max_output_tokens_applied,
                ),
            },
            starts: plan.starts,
            quota_decision: plan
                .downgrade_reason
                .map_or("allow", |reason| reason.as_str())
                .to_owned(),
            limits: plan.limits,
        };
        let run = TurnRun {
            ctx: ctx.clone(),
            scope: scopes.tenant,
            quota_scope: scopes.quota,
            chat_id: chat.id,
            turn_id,
            request_id,
            assistant_message_id,
            user_created_at,
            selected_model: chat.model,
            effective_model: plan.entry.id,
            downgrade_reason: plan.downgrade_reason,
            request: plan.request,
            files,
            ledger,
            summary_applied,
            trigger,
            started: Instant::now(),
        };
        tokio::spawn(Arc::clone(self).run_turn(run, sender));
        TurnEvents::live(receiver)
    }
}

/// The reserve transaction body. Order matters: the `chat_turns` insert comes
/// first, so a lost race on `(chat_id, request_id)` or on the
/// one-running-turn-per-chat index fails before anything else was written,
/// and it is the first (write) statement of the SQLite transaction.
async fn reserve_turn_tx(tx: &DbTx<'_>, input: &ReserveInput) -> Result<(), DomainError> {
    tx.insert_turn(&input.tenant_scope, input.turn.clone())
        .await?;
    reserve_quota_tx(tx, &input.quota, input.turn.now).await?;
    tx.insert_message(&input.tenant_scope, input.message.clone())
        .await?;
    if !input.attachment_ids.is_empty() {
        // Re-validated inside the transaction: a failed check rolls back the
        // turn and the message too.
        let ready = tx
            .find_ready_attachments(
                &input.tenant_scope,
                input.chat_id,
                input.user_id,
                &input.attachment_ids,
            )
            .await?;
        if ready.len() != input.attachment_ids.len() {
            return Err(DomainError::InvalidAttachment);
        }
        tx.link_attachments(&input.tenant_scope, &input.links)
            .await?;
    }
    if !tx
        .touch_chat(&input.owner_scope, input.chat_id, input.turn.now)
        .await?
    {
        return Err(chat_not_found(input.chat_id));
    }
    Ok(())
}

/// A failure of a `quota_usage` statement is never a lost `chat_turns` race:
/// the upserts cannot raise a unique violation, and should the driver ever
/// report one it must not be classified as `turn_already_running`.
fn quota_failure(error: DomainError) -> DomainError {
    match error {
        DomainError::Conflict { code } => {
            DomainError::internal(format!("quota_usage write conflicted: {code}"))
        }
        other => other,
    }
}

/// Books the reserve in every bucket and period of the decision, then re-reads
/// the rows (the increments hold the row locks, so the read sees every reserve
/// committed before it) and rejects with 429 `tokens` when a bucket is over its
/// limit. The cascade is not run again: the request is not downgraded here.
async fn reserve_quota_tx(
    tx: &DbTx<'_>,
    quota: &QuotaReserve,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    tx.reserve_quota(
        &quota.scope,
        quota.owner,
        Bucket::of_tier(quota.tier),
        quota.starts,
        quota.credits_micro,
        now,
    )
    .await
    .map_err(quota_failure)?;
    let rows = tx
        .quota_rows(&quota.scope, quota.owner, quota.starts)
        .await
        .map_err(quota_failure)?;
    let usage = QuotaUsage::from_rows(&rows);
    // `spent + reserved` already contains this turn's reserve: nothing more is
    // added by the check.
    if tier_available(quota.tier, &usage, &quota.limits, 0) {
        Ok(())
    } else {
        Err(DomainError::QuotaExceeded { scope: "tokens" })
    }
}

#[cfg(test)]
#[path = "reserve_tests.rs"]
mod tests;
