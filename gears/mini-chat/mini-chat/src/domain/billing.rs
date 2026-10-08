//! Normative billing outcome derivation (DESIGN §5.8) and settlement math.

use mini_chat_sdk::usage::{billing_outcome, settlement_method};

/// Terminal turn state (`chat_turns.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TurnState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Turn Status API value.
    #[must_use]
    pub const fn api_state(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Settlement method selected for a terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Actual,
    Estimated,
    Released,
}

impl Settlement {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => settlement_method::ACTUAL,
            Self::Estimated => settlement_method::ESTIMATED,
            Self::Released => settlement_method::RELEASED,
        }
    }
}

/// Billing outcome + settlement method of one terminal condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingDecision {
    pub billing_outcome: &'static str,
    pub settlement: Settlement,
    /// The error code was not in the known taxonomy.
    pub unknown_error_code: bool,
}

/// Error codes stored in `chat_turns.error_code`.
pub mod codes {
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
    pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
    pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
    pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
    pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
    pub const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";
    pub const TURN_SETUP_FAILED: &str = "turn_setup_failed";
    pub const VALIDATION_ERROR: &str = "validation_error";
    pub const INPUT_TOO_LONG: &str = "input_too_long";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";
    /// SSE-only codes (never stored).
    pub const FINALIZATION_FAILED: &str = "finalization_failed";
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
}

/// Derive the billing outcome from the terminal condition (never from the
/// raw state string alone). `usage_known` means the provider reported usage
/// with at least one non-zero count.
#[must_use]
pub fn derive(state: TurnState, error_code: Option<&str>, usage_known: bool) -> BillingDecision {
    let post_provider = |unknown: bool| BillingDecision {
        billing_outcome: billing_outcome::FAILED,
        settlement: if usage_known && !unknown { Settlement::Actual } else { Settlement::Estimated },
        unknown_error_code: unknown,
    };
    match state {
        TurnState::Completed | TurnState::Running => BillingDecision {
            billing_outcome: billing_outcome::COMPLETED,
            settlement: Settlement::Actual,
            unknown_error_code: false,
        },
        TurnState::Cancelled => BillingDecision {
            billing_outcome: billing_outcome::ABORTED,
            settlement: Settlement::Estimated,
            unknown_error_code: false,
        },
        TurnState::Failed => match error_code {
            Some(codes::ORPHAN_TIMEOUT) => BillingDecision {
                billing_outcome: billing_outcome::ABORTED,
                settlement: Settlement::Estimated,
                unknown_error_code: false,
            },
            Some(
                codes::CONTEXT_LENGTH_EXCEEDED
                | codes::VALIDATION_ERROR
                | codes::INPUT_TOO_LONG
                | codes::TURN_SETUP_FAILED,
            ) => BillingDecision {
                billing_outcome: billing_outcome::FAILED,
                settlement: Settlement::Released,
                unknown_error_code: false,
            },
            Some(
                codes::PROVIDER_ERROR
                | codes::PROVIDER_TIMEOUT
                | codes::RATE_LIMITED
                | codes::WEB_SEARCH_CALLS_EXCEEDED
                | codes::CODE_INTERPRETER_CALLS_EXCEEDED
                | codes::AGENTIC_ITERATIONS_EXCEEDED
                | codes::UNEXPECTED_TOOL_USE
                | codes::MESSAGE_PERSISTENCE_FAILED,
            ) => post_provider(false),
            _ => post_provider(true),
        },
    }
}

/// Overshoot reconciliation of an actual settlement (DESIGN §5.4.5).
/// Returns `(committed_credits, capped)`.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "ratio comparison against a float tolerance")]
pub fn committed_credits(
    actual_tokens: i64,
    reserve_tokens: i64,
    actual_credits: i64,
    reserved_credits: i64,
    tolerance: f64,
) -> (i64, bool) {
    if reserve_tokens > 0 && actual_tokens > reserve_tokens {
        let factor = actual_tokens as f64 / reserve_tokens as f64;
        if factor > tolerance {
            return (reserved_credits, true);
        }
    }
    (actual_credits, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::many_single_char_names, reason = "short names for table-driven outcome checks")]
    fn mapping_table() {
        let c = derive(TurnState::Completed, None, false);
        assert_eq!((c.billing_outcome, c.settlement), ("completed", Settlement::Actual));
        let f = derive(TurnState::Failed, Some("provider_error"), true);
        assert_eq!((f.billing_outcome, f.settlement), ("failed", Settlement::Actual));
        let f = derive(TurnState::Failed, Some("provider_timeout"), false);
        assert_eq!((f.billing_outcome, f.settlement), ("failed", Settlement::Estimated));
        let f = derive(TurnState::Failed, Some("web_search_calls_exceeded"), false);
        assert_eq!(f.settlement, Settlement::Estimated);
        let f = derive(TurnState::Failed, Some("turn_setup_failed"), false);
        assert_eq!((f.billing_outcome, f.settlement), ("failed", Settlement::Released));
        let a = derive(TurnState::Cancelled, None, true);
        assert_eq!((a.billing_outcome, a.settlement), ("aborted", Settlement::Estimated));
        let o = derive(TurnState::Failed, Some("orphan_timeout"), false);
        assert_eq!((o.billing_outcome, o.settlement), ("aborted", Settlement::Estimated));
        let u = derive(TurnState::Failed, Some("weird"), true);
        assert!(u.unknown_error_code);
        assert_eq!(u.settlement, Settlement::Estimated);
    }

    #[test]
    fn overshoot_cap() {
        assert_eq!(committed_credits(11_500, 10_000, 3_000, 2_500, 1.10), (2_500, true));
        assert_eq!(committed_credits(10_500, 10_000, 2_600, 2_500, 1.10), (2_600, false));
        assert_eq!(committed_credits(9_000, 10_000, 2_000, 2_500, 1.10), (2_000, false));
    }

    #[test]
    fn api_state_mapping() {
        assert_eq!(TurnState::Completed.api_state(), "done");
        assert_eq!(TurnState::Failed.api_state(), "error");
        assert_eq!(TurnState::parse("cancelled"), Some(TurnState::Cancelled));
    }
}
