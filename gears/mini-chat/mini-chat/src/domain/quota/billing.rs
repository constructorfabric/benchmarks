//! Billing outcome derivation (DESIGN section 5.8 "Normative Billing Outcome Derivation").
//!
//! The single mapping from a turn's terminal state, error code and reported usage to the usage
//! event's `billing_outcome` and the settlement method. Used by every finalization path.

use serde::Serialize;

use crate::infra::db::TurnState;
use crate::infra::db::repo::turns::ORPHAN_TIMEOUT;
use crate::infra::llm::types::ProviderUsage;

/// `UsageEvent.billing_outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// `UsageEvent.settlement_method`: how the quota settlement charges the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettlementMethod {
    /// Provider-reported usage.
    Actual,
    /// `credits(estimated_input_tokens, minimal_generation_floor_applied)`.
    Estimated,
    /// The provider was never called: no charge.
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Failures after the provider call started: actual when usage was reported, else estimated.
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

/// Failures before the provider call: nothing is charged.
const PRE_PROVIDER_CODES: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Derives the billing outcome and settlement method of a finalized turn.
#[must_use]
pub fn derive_billing(
    state: TurnState,
    error_code: Option<&str>,
    usage: Option<&ProviderUsage>,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        TurnState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        TurnState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        TurnState::Failed => match error_code {
            Some(ORPHAN_TIMEOUT) => (BillingOutcome::Aborted, SettlementMethod::Estimated),
            Some(code) if POST_PROVIDER_CODES.contains(&code) => {
                let method = if usage.is_some_and(has_usage) {
                    SettlementMethod::Actual
                } else {
                    SettlementMethod::Estimated
                };
                (BillingOutcome::Failed, method)
            }
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            other => {
                tracing::error!(
                    error_code = ?other,
                    "unknown_error_code: failed turn settled with the estimated formula"
                );
                (BillingOutcome::Failed, SettlementMethod::Estimated)
            }
        },
        TurnState::Running => {
            tracing::error!("billing derivation for a running turn; settled as unknown failure");
            (BillingOutcome::Failed, SettlementMethod::Estimated)
        }
    }
}

/// The provider reported at least one non-zero count.
fn has_usage(u: &ProviderUsage) -> bool {
    [
        u.input_tokens,
        u.output_tokens,
        u.cache_read_input_tokens,
        u.cache_write_input_tokens,
        u.reasoning_tokens,
    ]
    .iter()
    .any(|&n| n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOME_USAGE: ProviderUsage = ProviderUsage {
        input_tokens: 10,
        output_tokens: 2,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    const ZERO_USAGE: ProviderUsage = ProviderUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };

    #[test]
    fn billing_derivation_table() {
        use BillingOutcome::{Aborted, Completed, Failed};
        use SettlementMethod::{Actual, Estimated, Released};
        use TurnState as S;

        type Row<'a> = (
            TurnState,
            Option<&'a str>,
            Option<&'a ProviderUsage>,
            BillingOutcome,
            SettlementMethod,
        );
        let rows: Vec<Row<'_>> = vec![
            // completed: always actual, even without usage
            (S::Completed, None, Some(&SOME_USAGE), Completed, Actual),
            (S::Completed, None, None, Completed, Actual),
            (S::Completed, None, Some(&ZERO_USAGE), Completed, Actual),
            // provider failures
            (
                S::Failed,
                Some("provider_error"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (S::Failed, Some("provider_error"), None, Failed, Estimated),
            (
                S::Failed,
                Some("provider_error"),
                Some(&ZERO_USAGE),
                Failed,
                Estimated,
            ),
            (S::Failed, Some("provider_timeout"), None, Failed, Estimated),
            (
                S::Failed,
                Some("provider_timeout"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (S::Failed, Some("rate_limited"), None, Failed, Estimated),
            (
                S::Failed,
                Some("rate_limited"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            // tool / agentic / persistence failures mirror provider failures
            (
                S::Failed,
                Some("web_search_calls_exceeded"),
                None,
                Failed,
                Estimated,
            ),
            (
                S::Failed,
                Some("web_search_calls_exceeded"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (
                S::Failed,
                Some("code_interpreter_calls_exceeded"),
                None,
                Failed,
                Estimated,
            ),
            (
                S::Failed,
                Some("code_interpreter_calls_exceeded"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (
                S::Failed,
                Some("agentic_iterations_exceeded"),
                None,
                Failed,
                Estimated,
            ),
            (
                S::Failed,
                Some("agentic_iterations_exceeded"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (
                S::Failed,
                Some("unexpected_tool_use"),
                None,
                Failed,
                Estimated,
            ),
            (
                S::Failed,
                Some("unexpected_tool_use"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            (
                S::Failed,
                Some("message_persistence_failed"),
                None,
                Failed,
                Estimated,
            ),
            (
                S::Failed,
                Some("message_persistence_failed"),
                Some(&SOME_USAGE),
                Failed,
                Actual,
            ),
            // pre-provider failures: released, whatever the usage
            (
                S::Failed,
                Some("context_length_exceeded"),
                None,
                Failed,
                Released,
            ),
            (S::Failed, Some("validation_error"), None, Failed, Released),
            (S::Failed, Some("input_too_long"), None, Failed, Released),
            (
                S::Failed,
                Some("turn_setup_failed"),
                Some(&SOME_USAGE),
                Failed,
                Released,
            ),
            // orphan watchdog and cancellation: aborted, estimated (usage ignored)
            (S::Failed, Some("orphan_timeout"), None, Aborted, Estimated),
            (
                S::Failed,
                Some("orphan_timeout"),
                Some(&SOME_USAGE),
                Aborted,
                Estimated,
            ),
            (S::Cancelled, None, None, Aborted, Estimated),
            (S::Cancelled, None, Some(&SOME_USAGE), Aborted, Estimated),
            // unknown or missing code
            (
                S::Failed,
                Some("something_new"),
                Some(&SOME_USAGE),
                Failed,
                Estimated,
            ),
            (S::Failed, None, None, Failed, Estimated),
        ];
        for (state, code, usage, outcome, method) in rows {
            assert_eq!(
                derive_billing(state, code, usage),
                (outcome, method),
                "{state} {code:?} {usage:?}"
            );
        }
    }

    #[test]
    fn serialized_as_lowercase_strings() {
        assert_eq!(
            serde_json::to_value([
                BillingOutcome::Completed,
                BillingOutcome::Failed,
                BillingOutcome::Aborted
            ])
            .unwrap(),
            serde_json::json!(["completed", "failed", "aborted"])
        );
        assert_eq!(
            serde_json::to_value([
                SettlementMethod::Actual,
                SettlementMethod::Estimated,
                SettlementMethod::Released
            ])
            .unwrap(),
            serde_json::json!(["actual", "estimated", "released"])
        );
        assert_eq!(BillingOutcome::Aborted.as_str(), "aborted");
        assert_eq!(SettlementMethod::Released.as_str(), "released");
    }
}
