//! The turn mutation metrics (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics`):
//! the `result` classes of `mini_chat_turn_mutation` and the port that records the series.

use std::time::Duration;

use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::mutations::MutationOp;

/// The values of the `result` label of `mini_chat_turn_mutation`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationResult {
    Ok,
    NotFound,
    Forbidden,
    NotLatestTurn,
    TurnRunning,
    GenerationInProgress,
    PreflightRejected,
    Error,
}

impl MutationResult {
    /// The label value: `ok`, `not_found`, `forbidden`, `not_latest_turn`, `turn_running`,
    /// `generation_in_progress`, `preflight_rejected` or `error`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::Forbidden => "forbidden",
            Self::NotLatestTurn => "not_latest_turn",
            Self::TurnRunning => "turn_running",
            Self::GenerationInProgress => "generation_in_progress",
            Self::PreflightRejected => "preflight_rejected",
            Self::Error => "error",
        }
    }
}

/// The `result` class of a rejected mutation; `at_preflight` tells a rejection of the mutation
/// preflight (quota, kill switch, image guards, input limit, model) from the other rejections.
#[must_use]
pub fn classify_rejection(error: &DomainError, at_preflight: bool) -> MutationResult {
    use crate::domain::error::ConflictReason;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-counter
    match error {
        DomainError::NotFound { .. } => MutationResult::NotFound,
        DomainError::AuthzDenied => MutationResult::Forbidden,
        DomainError::Conflict {
            reason: ConflictReason::NotLatestTurn,
        } => MutationResult::NotLatestTurn,
        DomainError::Conflict {
            reason: ConflictReason::GenerationInProgress,
        } => MutationResult::GenerationInProgress,
        DomainError::TurnNotTerminal => MutationResult::TurnRunning,
        DomainError::QuotaExceeded { .. }
        | DomainError::FeatureDisabled { .. }
        | DomainError::TooManyImages
        | DomainError::VisionNotSupported
        | DomainError::InputTooLong
        | DomainError::InvalidModel { .. }
            if at_preflight =>
        {
            MutationResult::PreflightRejected
        }
        _ => MutationResult::Error,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-counter
}

// @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-cardinality
// @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-no-reaction-metric
/// The recording port of the mutation series. Reactions have no metric.
pub trait MutationMetrics: Send + Sync {
    /// `mini_chat_turn_mutation{op, result}`, once per request.
    fn mutation(&self, op: MutationOp, result: MutationResult);

    /// `mini_chat_turn_mutation_latency_ms{op}`, once per committed mutation: from the start of
    /// the preview to the commit.
    fn mutation_latency(&self, op: MutationOp, elapsed: Duration);
}
// @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-no-reaction-metric
// @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-metrics:p1:inst-met-cardinality
