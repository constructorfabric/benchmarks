//! The turn finalization port of the send setup (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the terminal transition of a committed turn
//! that never reached a provider response (a pre-provider cancellation, a provider task that
//! could not start).
//!
//! The port is implemented by the infrastructure layer over one finalization transaction, in
//! which `QuotaService::finalize_turn` performs the CAS on `state = 'running'`, the billing
//! derivation, the settlement and the usage message enqueue. Slice S2 extends it with the
//! outcomes that carry provider usage and accumulated text.

use async_trait::async_trait;
use mini_chat_sdk::UsageTokens;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::quota::{PeriodStarts, ToolCallCounts};
use crate::domain::streaming::plan::ProviderCallPlan;
use crate::domain::turns::TurnState;

/// The committed turn to finalize: its identity and what the settlement needs of the preflight.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTarget {
    pub tenant_id: Uuid,
    pub turn_id: Uuid,
    /// `chats.model`: the model the chat selected.
    pub selected_model: String,
    /// The daily and monthly period starts of the preflight the reserve was booked on.
    pub period_starts: PeriodStarts,
}

impl TurnTarget {
    /// The turn of a provider call plan.
    #[must_use]
    pub fn of_plan(plan: &ProviderCallPlan) -> Self {
        Self {
            tenant_id: plan.tenant_id,
            turn_id: plan.turn_id,
            selected_model: plan.selected_model.clone(),
            period_starts: plan.decision.period_starts,
        }
    }
}

/// The terminal outcome of a turn without provider usage and without accumulated text, so no
/// assistant message is written.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The client went away: `cancelled`, settled `estimated`, one `aborted` usage event.
    Cancelled,
    /// `failed` with a stored error code (never an SSE-only code), settled by the outcome rules
    /// of `cpt-cf-mini-chat-feature-quota-and-billing`: `estimated` without provider usage.
    Failed { code: StreamErrorCode },
}

/// What a finalization reports.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFinalization {
    /// This call won the CAS: the turn is terminal and settled.
    Finalized,
    /// Another finalizer won the CAS: nothing changed.
    AlreadyFinalized,
}

/// The outcome of a stream terminal (slice S2): what the provider task reached.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// `response.completed` or `response.incomplete` (or the adapter equivalent): the assistant
    /// message is inserted with the accumulated text, possibly empty.
    Completed,
    /// A terminal failure with a stored error code (never an SSE-only code) and the sanitized
    /// `error_detail`; partial text of a failed turn is discarded.
    Failed {
        code: StreamErrorCode,
        detail: String,
    },
    /// The client went away: a non-empty text is persisted as a partial assistant message.
    Cancelled,
}

/// What the stream finalization needs besides the plan (slice S2).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct StreamTerminal {
    pub outcome: StreamOutcome,
    /// The provider usage when known (never for `Cancelled`, never for a tool-limit breach).
    pub usage: Option<UsageTokens>,
    /// The provider response id, kept internally (`messages` and `chat_turns`).
    pub provider_response_id: Option<String>,
    /// The accumulated assistant text.
    pub text: String,
    /// The in-memory completed tool counts (`file_search` includes `search_knowledge`).
    pub tool_counts: ToolCallCounts,
    /// The provider `incomplete` reason (logging and the metric label only, never stored as
    /// `error_code`).
    pub incomplete_reason: Option<String>,
}

/// The committed terminal state of a turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedTurn {
    /// `Completed`, `Failed` (also the `message_persistence_failed` downgrade) or `Cancelled`.
    pub state: TurnState,
    /// The stored `error_code` of a failed turn, `None` otherwise.
    pub error_code: Option<String>,
    /// The persisted assistant (or partial) message.
    pub assistant_message_id: Option<Uuid>,
}

/// What a stream finalization reports.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFinalization {
    /// This call won the CAS and committed the terminal state with its settlement.
    Committed(CommittedTurn),
    /// Another finalizer won the CAS: the transaction rolled back, nothing changed.
    AlreadyFinalized,
}

/// The finalization of a committed turn: one transaction that commits the terminal state, the
/// settlement and the usage message together or not at all; the outbox wakes are fired after
/// the commit.
#[async_trait]
pub trait TurnFinalizer: Send + Sync {
    /// Move the `running` turn to the terminal state of `outcome`.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the transaction fails; it rolled back and the turn stays `running`
    /// (the orphan watchdog settles it).
    async fn finalize(
        &self,
        target: &TurnTarget,
        outcome: TurnOutcome,
    ) -> Result<TurnFinalization, DomainError>;

    /// The stream finalization of slice S2 (`cpt-cf-mini-chat-algo-message-streaming-finalization`):
    /// one transaction with the assistant (or partial) message insert, the CAS and settlement
    /// through `QuotaService::finalize_turn`, the turn audit event and, for a completed turn
    /// whose trigger fires, the thread-summary work; a failed message insert of a completed
    /// outcome is downgraded to `failed` with `message_persistence_failed`; the wakes and the
    /// metrics follow the commit.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the transaction failed (it rolled back and the turn stays
    /// `running`).
    async fn finalize_stream(
        &self,
        plan: &ProviderCallPlan,
        terminal: &StreamTerminal,
    ) -> Result<StreamFinalization, DomainError>;
}
