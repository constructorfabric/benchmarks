//! Billing outcome derivation and settlement amounts (DESIGN §5.7–5.9).

use mini_chat_sdk::UsageTokens;

use crate::domain::credits::{CreditError, credits_micro};

/// Terminal condition of a turn as seen by the finalizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    Completed,
    Failed { error_code: String },
    Cancelled,
    Orphan,
}

impl Terminal {
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed { .. } | Self::Orphan => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Persisted reserve fields of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Actual,
    Estimated,
    Released,
}

impl Method {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Settlement of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub billing_outcome: &'static str,
    pub method: Method,
    /// Credits charged to quota and emitted as `actual_credits_micro`.
    pub committed_credits_micro: i64,
    /// Usage emitted in the usage event (`None` when unknown).
    pub event_usage: Option<UsageTokens>,
    /// Token telemetry added to bucket `total`.
    pub telemetry_input: i64,
    pub telemetry_output: i64,
    /// Whether tool call counts are added to `quota_usage`.
    pub count_tool_calls: bool,
    /// Actual tokens exceeded the reserve.
    pub overshoot: bool,
}

const POST_PROVIDER: &[&str] = &[
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];
const PRE_PROVIDER: &[&str] = &["context_length_exceeded", "validation_error", "input_too_long", "turn_setup_failed"];

fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Compute the settlement.
///
/// # Errors
/// Credit computation failure (finalization then fails).
pub fn settle(
    terminal: &Terminal,
    usage: Option<UsageTokens>,
    r: ReserveFields,
    in_mult: i64,
    out_mult: i64,
    overshoot_tolerance: f64,
) -> Result<Settlement, CreditError> {
    let actual = |u: UsageTokens, outcome: &'static str| -> Result<Settlement, CreditError> {
        let actual_tokens = u.input_tokens + u.output_tokens;
        let mut committed = credits_micro(u.input_tokens, u.output_tokens, in_mult, out_mult)?;
        let overshoot = actual_tokens > r.reserve_tokens;
        if overshoot && r.reserve_tokens > 0 {
            #[allow(clippy::cast_precision_loss)]
            let factor = actual_tokens as f64 / r.reserve_tokens as f64;
            if factor > overshoot_tolerance {
                committed = r.reserved_credits_micro;
            }
        }
        Ok(Settlement {
            billing_outcome: outcome,
            method: Method::Actual,
            committed_credits_micro: committed,
            event_usage: Some(u),
            telemetry_input: u.input_tokens,
            telemetry_output: u.output_tokens,
            count_tool_calls: true,
            overshoot,
        })
    };
    let estimated = |outcome: &'static str, event_usage: Option<UsageTokens>| -> Result<Settlement, CreditError> {
        let est_in = (r.reserve_tokens - r.max_output_tokens_applied).max(0);
        let committed = credits_micro(est_in, r.minimal_generation_floor_applied.max(0), in_mult, out_mult)?;
        Ok(Settlement {
            billing_outcome: outcome,
            method: Method::Estimated,
            committed_credits_micro: committed,
            event_usage,
            telemetry_input: 0,
            telemetry_output: 0,
            count_tool_calls: true,
            overshoot: false,
        })
    };
    match terminal {
        Terminal::Completed => actual(usage.unwrap_or_default(), "completed"),
        Terminal::Cancelled | Terminal::Orphan => estimated("aborted", None),
        Terminal::Failed { error_code } => {
            if PRE_PROVIDER.contains(&error_code.as_str()) {
                return Ok(Settlement {
                    billing_outcome: "failed",
                    method: Method::Released,
                    committed_credits_micro: 0,
                    event_usage: Some(UsageTokens::default()),
                    telemetry_input: 0,
                    telemetry_output: 0,
                    count_tool_calls: false,
                    overshoot: false,
                });
            }
            if !POST_PROVIDER.contains(&error_code.as_str()) {
                tracing::error!(error_code = %error_code, "unknown error code at settlement; settling estimated");
            }
            match usage {
                Some(u) if usage_known(Some(&u)) => actual(u, "failed"),
                _ => estimated("failed", None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: ReserveFields = ReserveFields {
        reserve_tokens: 1500,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 1_500_000,
        minimal_generation_floor_applied: 50,
    };
    const M: i64 = 1_000_000_000;

    fn u(i: i64, o: i64) -> UsageTokens {
        UsageTokens { input_tokens: i, output_tokens: o, ..Default::default() }
    }

    #[test]
    fn completed_is_actual() {
        let s = settle(&Terminal::Completed, Some(u(900, 300)), R, M, M, 1.1).expect("ok");
        assert_eq!((s.billing_outcome, s.method, s.committed_credits_micro), ("completed", Method::Actual, 1_200_000));
        let s = settle(&Terminal::Completed, None, R, M, M, 1.1).expect("ok");
        assert_eq!((s.method, s.committed_credits_micro), (Method::Actual, 0));
    }

    #[test]
    fn overshoot_beyond_tolerance_caps_at_reserve() {
        let s = settle(&Terminal::Completed, Some(u(1600, 200)), R, M, M, 1.1).expect("ok");
        assert!(s.overshoot);
        assert_eq!(s.committed_credits_micro, 1_500_000);
        let s = settle(&Terminal::Completed, Some(u(1550, 0)), R, M, M, 1.1).expect("ok");
        assert_eq!(s.committed_credits_micro, 1_550_000);
    }

    #[test]
    fn cancelled_and_orphan_are_aborted_estimated() {
        for t in [Terminal::Cancelled, Terminal::Orphan] {
            let s = settle(&t, Some(u(5, 5)), R, M, M, 1.1).expect("ok");
            assert_eq!((s.billing_outcome, s.method), ("aborted", Method::Estimated));
            // est_in = 1000, floor 50
            assert_eq!(s.committed_credits_micro, 1_050_000);
            assert!(s.event_usage.is_none());
        }
    }

    #[test]
    fn failed_classification() {
        let f = |c: &str| Terminal::Failed { error_code: c.into() };
        let s = settle(&f("provider_error"), Some(u(10, 0)), R, M, M, 1.1).expect("ok");
        assert_eq!((s.billing_outcome, s.method), ("failed", Method::Actual));
        let s = settle(&f("provider_error"), Some(u(0, 0)), R, M, M, 1.1).expect("ok");
        assert_eq!(s.method, Method::Estimated);
        let s = settle(&f("web_search_calls_exceeded"), None, R, M, M, 1.1).expect("ok");
        assert_eq!(s.method, Method::Estimated);
        let s = settle(&f("turn_setup_failed"), None, R, M, M, 1.1).expect("ok");
        assert_eq!((s.method, s.committed_credits_micro), (Method::Released, 0));
        let s = settle(&f("weird"), None, R, M, M, 1.1).expect("ok");
        assert_eq!((s.billing_outcome, s.method), ("failed", Method::Estimated));
    }
}
