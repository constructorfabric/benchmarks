//! Quota settlement of a finalized turn (DESIGN 5.4.4, 5.7 "`FinalizeTurn`
//! Invariant", 5.8, 5.9): the shared step every terminal path uses inside its
//! CAS-guarded transaction.
//!
//! [`charge_for`] is pure and runs before the transaction. [`settle_turn`] runs
//! inside it, only for the finalizer that won the CAS: it releases the reserve,
//! commits the charge, derives the post-settlement warnings and enqueues the
//! usage and audit messages, so there is no committed debit without its usage
//! message. The orphan watchdog reuses both with a ledger rebuilt from the
//! persisted `chat_turns` row and the snapshot of `policy_version_applied`.

use mini_chat_sdk::{
    AuditEvent, ModelTier, RequesterType, SettlementMethod, TerminalState, TurnAuditEvent,
    UsageEvent, UsageTokens, UserLimits, turn_dedupe_key,
};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{
    AuditPort, QuotaOwner, QuotaRepository, QuotaSettlement, TurnLedgerRepository, UsagePort,
};
use crate::domain::provider::TokenBreakdown;
use crate::domain::quota::{
    Billing, Bucket, Charge, CreditsError, PeriodStarts, QuotaUsage, QuotaWarning, TurnReserve,
    derive_billing, quota_status, quota_warnings, settle_actual, settle_estimated,
};
use crate::domain::turns::{ToolCounts, TurnState};

/// Everything the settlement of a turn needs, fixed at preflight (or rebuilt
/// from the persisted turn by the watchdog).
#[derive(Debug, Clone)]
pub struct TurnLedger {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// `chat_turns.id`.
    pub turn_id: Uuid,
    /// `chat_turns.request_id`.
    pub request_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// `chat_turns.effective_model`.
    pub effective_model: String,
    /// Tier of the effective model: the buckets the turn reserved from.
    pub tier: ModelTier,
    /// Input multiplier of the effective model under `policy_version`.
    pub in_mult: u64,
    /// Output multiplier of the effective model under `policy_version`.
    pub out_mult: u64,
    /// `chat_turns.policy_version_applied`.
    pub policy_version: u64,
    /// The persisted reserve columns.
    pub reserve: TurnReserve,
    /// The period starts of the reserve.
    pub starts: PeriodStarts,
    /// `allow` or the downgrade reason (audit `quota_decision`).
    pub quota_decision: String,
    /// The limits of `policy_version` (for the post-settlement warnings).
    pub limits: UserLimits,
}

/// How a turn ended.
#[derive(Debug, Clone)]
pub struct TerminalFacts {
    /// Terminal state of the turn.
    pub state: TurnState,
    /// `chat_turns.error_code`.
    pub error_code: Option<String>,
    /// Provider usage, when it reported any.
    pub usage: Option<(u64, u64)>,
    /// Cache / reasoning breakdown of the provider usage.
    pub breakdown: TokenBreakdown,
    /// Time from the start of the provider task to the finalization.
    pub latency_ms: u64,
    /// Finalization time (the event timestamp).
    pub now: OffsetDateTime,
}

/// The billing and charge of an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// Billing outcome derivation.
    pub billing: Billing,
    /// What is debited.
    pub charge: Charge,
}

/// Derives the billing of a terminal outcome and computes its charge: actual
/// (with the bounded overshoot rule), estimated, or released.
///
/// # Errors
/// [`CreditsError`] when the provider usage or a multiplier is out of range;
/// the caller must then leave the turn unfinalized (DESIGN 5.3 "Overflow
/// Protection").
pub fn charge_for(
    ledger: &TurnLedger,
    facts: &TerminalFacts,
    overshoot_tolerance_factor: f64,
) -> Result<Settlement, CreditsError> {
    let billing = derive_billing(facts.state, facts.error_code.as_deref(), facts.usage);
    let charge = match billing.settlement_method {
        // A completed turn without any usage object is charged zero.
        SettlementMethod::Actual => Charge {
            breakdown: facts.breakdown,
            ..settle_actual(
                &ledger.reserve,
                facts.usage.unwrap_or((0, 0)),
                ledger.in_mult,
                ledger.out_mult,
                overshoot_tolerance_factor,
            )?
        },
        SettlementMethod::Released => Charge::released(),
        SettlementMethod::Estimated | SettlementMethod::None => {
            settle_estimated(&ledger.reserve, ledger.in_mult, ledger.out_mult)?
        }
    };
    Ok(Settlement { billing, charge })
}

/// Where a settlement reads and writes.
#[derive(Debug, Clone)]
pub struct SettlementScopes {
    /// Tenant scope of the turn row.
    pub turn: AccessScope,
    /// Tenant + owner scope of the quota rows.
    pub quota: AccessScope,
}

/// The outbox ports of the finalization (absent in a service without outbox).
#[derive(Clone, Copy)]
pub struct OutboxPorts<'a> {
    /// Usage queue.
    pub usage: Option<&'a dyn UsagePort>,
    /// Audit queue.
    pub audit: Option<&'a dyn AuditPort>,
}

/// What the settlement transaction produced.
pub struct Settled {
    /// Wake of the enqueued messages; fire it after the commit.
    pub wake: Wake,
    /// Post-settlement warnings (only when a threshold was asked for).
    pub warnings: Vec<QuotaWarning>,
}

fn quota_settlement(
    settlement: &Settlement,
    ledger: &TurnLedger,
    tools: &ToolCounts,
) -> QuotaSettlement {
    QuotaSettlement {
        release_credits_micro: ledger.reserve.reserved_credits_micro,
        spent_credits_micro: settlement.charge.credits_micro,
        input_tokens: settlement.charge.input_tokens,
        output_tokens: settlement.charge.output_tokens,
        web_search_calls: tools.web_search,
        code_interpreter_calls: tools.code_interpreter,
    }
}

fn usage_tokens(charge: &Charge) -> Option<UsageTokens> {
    (charge.method == SettlementMethod::Actual).then_some(UsageTokens {
        input_tokens: charge.input_tokens,
        output_tokens: charge.output_tokens,
        cache_read_input_tokens: charge.breakdown.cache_read,
        cache_write_input_tokens: charge.breakdown.cache_write,
        reasoning_tokens: charge.breakdown.reasoning,
    })
}

fn usage_event(
    ledger: &TurnLedger,
    facts: &TerminalFacts,
    settlement: &Settlement,
    tools: &ToolCounts,
) -> UsageEvent {
    UsageEvent {
        tenant_id: ledger.tenant_id,
        // A turn without a requester (watchdog path) carries the nil user.
        user_id: (!ledger.user_id.is_nil()).then_some(ledger.user_id),
        chat_id: ledger.chat_id,
        turn_id: Some(ledger.turn_id),
        request_id: ledger.request_id,
        effective_model: ledger.effective_model.clone(),
        selected_model: ledger.selected_model.clone(),
        terminal_state: settlement.billing.terminal_state,
        billing_outcome: settlement.billing.billing_outcome,
        usage: usage_tokens(&settlement.charge),
        actual_credits_micro: settlement.charge.credits_micro,
        settlement_method: settlement.billing.settlement_method,
        policy_version_applied: ledger.policy_version,
        web_search_calls: tools.web_search,
        code_interpreter_calls: tools.code_interpreter,
        file_search_calls: tools.file_search,
        timestamp: facts.now,
        requester_type: RequesterType::User,
        dedupe_key: turn_dedupe_key(ledger.tenant_id, ledger.turn_id, ledger.request_id),
        system_task_type: None,
    }
}

fn audit_event(
    ledger: &TurnLedger,
    facts: &TerminalFacts,
    settlement: &Settlement,
    tools: &ToolCounts,
) -> AuditEvent {
    let event = TurnAuditEvent {
        tenant_id: ledger.tenant_id,
        user_id: ledger.user_id,
        chat_id: ledger.chat_id,
        request_id: ledger.request_id,
        selected_model: ledger.selected_model.clone(),
        effective_model: ledger.effective_model.clone(),
        usage: usage_tokens(&settlement.charge),
        latency_ms: facts.latency_ms,
        web_search_calls: tools.web_search,
        file_search_calls: tools.file_search,
        quota_decision: ledger.quota_decision.clone(),
        error_code: facts.error_code.clone(),
        timestamp: facts.now,
    };
    if settlement.billing.terminal_state == TerminalState::Completed {
        AuditEvent::TurnCompleted(event)
    } else {
        AuditEvent::TurnFailed(event)
    }
}

/// The settlement step of the finalization transaction. Call it only after the
/// CAS was won: it releases the turn's reserve, commits the charge against the
/// buckets and periods of the reserve, and enqueues the usage and audit
/// messages. Any error must roll the whole transaction back.
///
/// `warning_threshold_pct` asks for the post-settlement `quota_warnings` (the
/// completed stream path); the watchdog passes `None`.
///
/// # Errors
/// `Internal` when a repository call or an enqueue fails.
pub async fn settle_turn(
    tx: &DbTx<'_>,
    scopes: &SettlementScopes,
    ledger: &TurnLedger,
    facts: &TerminalFacts,
    settlement: &Settlement,
    warning_threshold_pct: Option<u8>,
    ports: OutboxPorts<'_>,
) -> Result<Settled, DomainError> {
    let owner = QuotaOwner {
        tenant_id: ledger.tenant_id,
        user_id: ledger.user_id,
    };
    let tools = tx.tool_counts(&scopes.turn, ledger.turn_id).await?;
    tx.settle_quota(
        &scopes.quota,
        owner,
        Bucket::of_tier(ledger.tier),
        ledger.starts,
        &quota_settlement(settlement, ledger, &tools),
        facts.now,
    )
    .await?;

    let warnings = match warning_threshold_pct {
        Some(threshold) if facts.state == TurnState::Completed => {
            let rows = tx.quota_rows(&scopes.quota, owner, ledger.starts).await?;
            let status = quota_status(
                &ledger.limits,
                &QuotaUsage::from_rows(&rows),
                threshold,
                facts.now,
            );
            quota_warnings(&status)
        }
        _ => Vec::new(),
    };

    // The enqueues come last: a Wake only exists once the message is queued.
    let wake = enqueue_events(tx, ledger, facts, settlement, &tools, ports).await?;
    Ok(Settled { wake, warnings })
}

/// The enqueue step of the finalization: the usage message and the turn audit
/// message, in the caller's transaction. [`settle_turn`] ends with it; the
/// orphan watchdog calls it alone for a turn that holds no reserve (nothing to
/// settle, DESIGN 5.7 "`FinalizeTurn` Invariant").
///
/// # Errors
/// `Internal` when an enqueue fails (the transaction must roll back).
pub async fn enqueue_events(
    tx: &DbTx<'_>,
    ledger: &TurnLedger,
    facts: &TerminalFacts,
    settlement: &Settlement,
    tools: &ToolCounts,
    ports: OutboxPorts<'_>,
) -> Result<Wake, DomainError> {
    let mut wake = Wake::empty();
    if let Some(usage) = ports.usage {
        wake += usage
            .enqueue_usage(tx, &usage_event(ledger, facts, settlement, tools))
            .await?;
    }
    if let Some(audit) = ports.audit {
        wake += audit
            .enqueue_audit(tx, &audit_event(ledger, facts, settlement, tools))
            .await?;
    }
    Ok(wake)
}
