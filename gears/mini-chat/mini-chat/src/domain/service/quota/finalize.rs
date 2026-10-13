//! Billing outcome derivation and the CAS-guarded finalization contract
//! (`cpt-cf-mini-chat-algo-quota-billing-billing-outcome`,
//! `cpt-cf-mini-chat-algo-quota-billing-finalize-turn`,
//! `cpt-cf-mini-chat-state-quota-billing-turn-billing`).
//!
//! The helper settles a turn that took a reserve exactly once: the CAS on
//! `chat_turns.state`, the quota settlement and the usage event are one
//! transaction. The caller (stream pipeline, mutation path or orphan watchdog)
//! fetches the snapshot before the transaction opens and passes it in.

use mini_chat_sdk::{BillingOutcome, SettlementMethod, TerminalState, UsageTokens};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::periods::PeriodKeys;
use super::policy::SettlementSnapshot;
use super::settle::{FallbackCause, ToolCallCounts};
use crate::domain::service::workers::summary_trigger::TriggerResult;

/// `error_code` of a turn finalized by the orphan watchdog.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// The terminal classification chosen by the caller. The billing outcome is
/// derived from it, never from a re-read of `chat_turns.state`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalClassification {
    /// Provider `response.completed` or `response.incomplete`.
    Completed,
    /// Failed with this `error_code` (`orphan_timeout` included).
    Failed {
        /// The persisted `error_code`.
        error_code: String,
    },
    /// Client disconnect.
    Cancelled,
}

/// The derived outcome.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingDerivation {
    /// `completed`, `failed` or `cancelled`.
    pub terminal_state: TerminalState,
    /// `completed`, `failed` or `aborted`.
    pub billing_outcome: BillingOutcome,
    /// `actual`, `estimated` or `released`.
    pub settlement_method: SettlementMethod,
    /// The error code is none of the taxonomy (the caller logs it at `error`).
    pub unknown_error_code: bool,
}

/// The billing outcome table: `completed` is `completed` / `completed` /
/// `actual`; post-provider error codes are `failed` / `failed` with `actual`
/// when the usage object has `input_tokens` or `output_tokens` above 0 and
/// `estimated` otherwise; pre-provider codes are `failed` / `failed` /
/// `released`; `cancelled` is `cancelled` / `aborted` / `estimated` (usage is
/// ignored); `orphan_timeout` is `failed` / `aborted` / `estimated`; any other
/// code (`quota_exceeded` included) is `failed` / `failed` / `estimated` with
/// `unknown_error_code`.
#[must_use]
pub fn derive_billing_outcome(
    classification: &TerminalClassification,
    usage: Option<&UsageTokens>,
) -> BillingDerivation {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-derive-from-trigger
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-return-outcome
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-match-classification
    match classification {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-completed
        // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-completed
        TerminalClassification::Completed => derived(
            TerminalState::Completed,
            BillingOutcome::Completed,
            SettlementMethod::Actual,
        ),
        // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-completed
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-completed
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-cancelled
        // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-aborted
        TerminalClassification::Cancelled => derived(
            TerminalState::Cancelled,
            BillingOutcome::Aborted,
            SettlementMethod::Estimated,
        ),
        // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-aborted
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-cancelled
        TerminalClassification::Failed { error_code } => derive_failed(error_code.as_str(), usage),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-match-classification
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-return-outcome
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-derive-from-trigger
}

/// The derivation of a failed turn by its `error_code`.
fn derive_failed(code: &str, usage: Option<&UsageTokens>) -> BillingDerivation {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-orphan
    // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-aborted
    if code == ORPHAN_TIMEOUT {
        return derived(
            TerminalState::Failed,
            BillingOutcome::Aborted,
            SettlementMethod::Estimated,
        );
    }
    // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-aborted
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-orphan
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-post-provider
    // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-failed-post
    if POST_PROVIDER_CODES.contains(&code) {
        let has_tokens =
            usage.is_some_and(|usage| usage.input_tokens > 0 || usage.output_tokens > 0);
        let method = if has_tokens {
            SettlementMethod::Actual
        } else {
            SettlementMethod::Estimated
        };
        return derived(TerminalState::Failed, BillingOutcome::Failed, method);
    }
    // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-failed-post
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-post-provider
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-pre-provider
    // @cpt-begin:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-failed-pre
    if PRE_PROVIDER_CODES.contains(&code) {
        return derived(
            TerminalState::Failed,
            BillingOutcome::Failed,
            SettlementMethod::Released,
        );
    }
    // @cpt-end:cpt-cf-mini-chat-state-quota-billing-turn-billing:p1:inst-to-failed-pre
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-pre-provider
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-unknown-code
    BillingDerivation {
        unknown_error_code: true,
        ..derived(
            TerminalState::Failed,
            BillingOutcome::Failed,
            SettlementMethod::Estimated,
        )
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-billing-outcome:p1:inst-case-unknown-code
}

/// A derivation of a known taxonomy entry.
const fn derived(
    terminal_state: TerminalState,
    billing_outcome: BillingOutcome,
    settlement_method: SettlementMethod,
) -> BillingDerivation {
    BillingDerivation {
        terminal_state,
        billing_outcome,
        settlement_method,
        unknown_error_code: false,
    }
}

/// Error codes of a failure after the provider was called.
const POST_PROVIDER_CODES: [&str; 8] = [
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Error codes of a failure before the provider was called.
const PRE_PROVIDER_CODES: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Which finalizer is calling.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeVariant {
    /// The stream (or mutation) pipeline: the preflight period starts and the
    /// selected model are known.
    Stream {
        /// The tenant of the turn: the CAS and the turn load are scoped to it,
        /// so a turn of another tenant is never finalized (`Lost`).
        tenant_id: Uuid,
        /// The period starts of the preflight.
        period_keys: PeriodKeys,
        /// `chats.model` at preflight (`selected_model` of the usage event).
        selected_model: String,
    },
    /// The orphan watchdog: no usage, `selected_model` is the effective
    /// model, the period starts come from `chat_turns.started_at`, and the CAS
    /// also requires `deleted_at IS NULL` and `COALESCE(last_progress_at,
    /// started_at) <= stale_cutoff`; it sets `failed` / `orphan_timeout`.
    Orphan {
        /// The stale-progress cutoff.
        stale_cutoff: OffsetDateTime,
    },
}

/// The terminal columns the caller chooses (written by the CAS together with
/// `completed_at` and `updated_at`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalColumns {
    /// Terminal `state`.
    pub state: TerminalState,
    /// `error_code`.
    pub error_code: Option<String>,
    /// `error_detail`.
    pub error_detail: Option<String>,
    /// `assistant_message_id` (the caller inserts the message in the same
    /// transaction).
    pub assistant_message_id: Option<Uuid>,
    /// `provider_response_id`.
    pub provider_response_id: Option<String>,
}

/// Everything the finalization helper needs.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeRequest {
    /// The turn.
    pub turn_id: Uuid,
    /// Stream or orphan.
    pub variant: FinalizeVariant,
    /// The caller's terminal classification.
    pub classification: TerminalClassification,
    /// The terminal columns.
    pub columns: TerminalColumns,
    /// Provider usage (stream variant only).
    pub usage: Option<UsageTokens>,
    /// Completed tool calls (stream variant; the orphan variant reads the
    /// `*_completed_count` columns of the row instead).
    pub tool_calls: ToolCallCounts,
    /// The snapshot of the turn's `policy_version_applied`, fetched before the
    /// transaction opens. The stream variant passes the preflight snapshot.
    pub snapshot: SettlementSnapshot,
    /// Current time.
    pub now: OffsetDateTime,
}

impl FinalizeRequest {
    /// An orphan-variant request: classification `failed` / `orphan_timeout`,
    /// terminal columns `failed` / `orphan_timeout` without a message.
    #[must_use]
    pub fn orphan(
        turn_id: Uuid,
        stale_cutoff: OffsetDateTime,
        snapshot: SettlementSnapshot,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            turn_id,
            variant: FinalizeVariant::Orphan { stale_cutoff },
            classification: TerminalClassification::Failed {
                error_code: ORPHAN_TIMEOUT.to_owned(),
            },
            columns: TerminalColumns {
                state: TerminalState::Failed,
                error_code: Some(ORPHAN_TIMEOUT.to_owned()),
                error_detail: None,
                assistant_message_id: None,
                provider_response_id: None,
            },
            usage: None,
            tool_calls: ToolCallCounts::default(),
            snapshot,
            now,
        }
    }
}

/// A won finalization.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeWon {
    /// Terminal state of the usage event.
    pub terminal_state: TerminalState,
    /// Billing outcome of the usage event.
    pub billing_outcome: BillingOutcome,
    /// Settlement method applied (`estimated` after a fallback; `estimated`
    /// for the no-reserve branch).
    pub settlement_method: SettlementMethod,
    /// Credits debited (0 when nothing was settled).
    pub committed_credits_micro: i64,
    /// `quota_usage` rows were settled (false for the corrupt-data branch
    /// without `requester_user_id` and for the no-reserve branch).
    pub settled: bool,
    /// One usage event was enqueued (false only for the corrupt-data branch
    /// without `requester_user_id`).
    pub usage_event_enqueued: bool,
    /// Actual input tokens (telemetry; 0 unless `actual`).
    pub actual_input_tokens: i64,
    /// Actual output tokens (telemetry; 0 unless `actual`).
    pub actual_output_tokens: i64,
    /// `chat_turns.reserve_tokens` (0 when NULL).
    pub reserve_tokens: i64,
    /// The overshoot cap applied.
    pub overshoot_capped: bool,
    /// The fallback cause, if the deterministic fallback was taken.
    pub fallback: Option<FallbackCause>,
    /// `chat_turns.effective_model`, when set.
    pub effective_model: Option<String>,
    /// Completed code interpreter calls of the turn.
    pub code_interpreter_calls: u32,
    /// The outcome of the thread-summary trigger evaluated in the closure of the
    /// finalization transaction by the caller; `None` when none ran.
    pub summary_trigger: Option<TriggerResult>,
}

/// Result of the finalization helper.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum FinalizeResult {
    /// Another finalizer owns the turn: nothing changed, nothing is enqueued.
    Lost,
    /// This call finalized the turn.
    Won(Box<FinalizeWon>),
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
