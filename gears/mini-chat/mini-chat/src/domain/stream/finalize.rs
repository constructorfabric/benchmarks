//! Turn finalization: one transaction per terminal path (DESIGN section 5.7).

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{SettlementMethod, TokenUsage, TurnLatency, UsageTerminalState};
use toolkit_security::AccessScope;
use tracing::warn;
use uuid::Uuid;

use super::facts::{TerminalFacts, TurnFacts, build_events};
use super::model::{
    AssistantMessage, FinalizeCommand, FinalizeOutcome, SummaryRequest, ToolCounts,
};
use super::ports::TurnRepository;
use crate::domain::billing::{derive_billing, settle};
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::quota::{PeriodType, periods_at};
use crate::domain::turn::TurnState;

/// The terminal path of a turn, before billing is derived.
#[derive(Debug, Clone)]
pub struct Terminal {
    /// The terminal state.
    pub state: TurnState,
    /// The stored error code of a failed turn.
    pub error_code: Option<String>,
    /// Non-sensitive diagnostic text of a failed turn.
    pub error_detail: Option<String>,
    /// Provider usage, when reported.
    pub usage: Option<TokenUsage>,
    /// The provider response id (internal).
    pub response_id: Option<String>,
    /// The accumulated answer text.
    pub text: String,
    /// Persist `text` as the assistant message.
    pub persist_message: bool,
    /// Completed tool calls.
    pub tool_counts: ToolCounts,
    /// Time to the first streamed token.
    pub first_token_ms: Option<u64>,
    /// Time since the turn started.
    pub total_ms: u64,
}

/// Why a finalization did not commit.
#[derive(Debug)]
pub enum FinalizeError {
    /// The credit computation was refused (out-of-range usage or multiplier): the turn cannot be
    /// settled and stays `running` until the orphan watchdog finalizes it.
    Settlement(DomainError),
    /// The transaction failed (for example the assistant message could not be persisted).
    Transaction(DomainError),
}

impl std::fmt::Display for FinalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Settlement(err) | Self::Transaction(err) => err.fmt(f),
        }
    }
}

/// Runs the finalization transaction and records its metrics.
pub struct Finalizer {
    turns: Arc<dyn TurnRepository>,
    metrics: Arc<dyn Metrics>,
    overshoot_tolerance: f64,
}

impl Finalizer {
    /// Creates the finalizer.
    #[must_use]
    pub fn new(
        turns: Arc<dyn TurnRepository>,
        metrics: Arc<dyn Metrics>,
        overshoot_tolerance: f64,
    ) -> Self {
        Self {
            turns,
            metrics,
            overshoot_tolerance,
        }
    }

    /// CAS-finalizes the turn: settlement, assistant message and the outbox events in one
    /// transaction. `Lost` means another finalizer already finalized the turn.
    ///
    /// # Errors
    /// An error when the credit computation is refused or the transaction fails.
    pub async fn finalize(
        &self,
        scope: &AccessScope,
        facts: &TurnFacts,
        terminal: &Terminal,
    ) -> Result<FinalizeOutcome, FinalizeError> {
        let started = Instant::now();
        let now = clock::now();
        let usage_state = match terminal.state {
            TurnState::Completed => UsageTerminalState::Completed,
            TurnState::Cancelled => UsageTerminalState::Cancelled,
            TurnState::Failed | TurnState::Running => UsageTerminalState::Failed,
        };
        let (billing_outcome, method) = derive_billing(
            usage_state,
            terminal.error_code.as_deref(),
            terminal.usage.as_ref(),
        );
        let settlement = settle(
            &facts.basis,
            method,
            terminal.usage.as_ref(),
            self.overshoot_tolerance,
        )
        .map_err(|err| {
            warn!(error = %err, turn_id = %facts.turn_id, "settlement refused");
            FinalizeError::Settlement(DomainError::internal(format!("settlement: {err}")))
        })?;
        let terminal_facts = TerminalFacts {
            state: usage_state,
            error_code: terminal.error_code.as_deref(),
            usage: terminal.usage,
            billing_outcome,
            tool_counts: terminal.tool_counts,
            latency: TurnLatency {
                time_to_first_token_ms: terminal.first_token_ms,
                total_ms: terminal.total_ms,
            },
            now,
        };
        let (usage_event, audit_event) = build_events(facts, &terminal_facts, &settlement);
        let assistant_message = terminal.persist_message.then(|| AssistantMessage {
            id: facts.assistant_message_id,
            content: terminal.text.clone(),
            model: facts.effective_model.clone(),
            usage: terminal.usage,
            provider_response_id: terminal.response_id.clone(),
        });
        let command = FinalizeCommand {
            turn_id: facts.turn_id,
            tenant_id: facts.tenant_id,
            user_id: facts.user_id,
            chat_id: facts.chat_id,
            request_id: facts.request_id,
            state: terminal.state,
            error_code: terminal.error_code.clone(),
            error_detail: terminal.error_detail.clone(),
            provider_response_id: terminal.response_id.clone(),
            assistant_message,
            tool_counts: terminal.tool_counts,
            periods: periods_at(facts.started_at),
            reserved_credits_micro: facts.basis.reserved_credits_micro,
            premium: Some(facts.premium),
            book_quota: true,
            orphan_cutoff: None,
            require_unreserved: false,
            settlement,
            now,
            usage_event,
            audit_event,
            summary: (terminal.state == TurnState::Completed)
                .then_some(facts.summary_trigger)
                .flatten()
                .map(|trigger| SummaryRequest {
                    trigger,
                    system_request_id: Uuid::new_v4(),
                }),
        };
        let outcome = self
            .turns
            .finalize(scope, command)
            .await
            .map_err(FinalizeError::Transaction)?;
        if outcome == FinalizeOutcome::Committed {
            self.record(facts, &terminal_facts, &settlement, started);
        }
        Ok(outcome)
    }

    fn record(
        &self,
        facts: &TurnFacts,
        terminal: &TerminalFacts<'_>,
        settlement: &crate::domain::billing::Settlement,
        started: Instant,
    ) {
        self.metrics
            .finalization_latency_ms(started.elapsed().as_secs_f64() * 1000.0);
        if terminal.billing_outcome == mini_chat_sdk::BillingOutcome::Aborted {
            self.metrics.streams_aborted("client_disconnect");
        }
        if settlement.method == SettlementMethod::Actual {
            for period in PeriodType::ALL {
                self.metrics.quota_commit(period.as_str());
                if settlement.overshoot {
                    self.metrics.quota_overshoot(period.as_str());
                }
            }
            #[allow(
                clippy::cast_precision_loss,
                reason = "token counts are far below 2^52"
            )]
            self.metrics.quota_actual_tokens(
                settlement
                    .telemetry_input_tokens
                    .saturating_add(settlement.telemetry_output_tokens) as f64,
            );
            if terminal.tool_counts.code_interpreter > 0 {
                self.metrics.code_interpreter_calls(
                    &facts.effective_model,
                    u64::from(terminal.tool_counts.code_interpreter),
                );
            }
        }
    }
}
