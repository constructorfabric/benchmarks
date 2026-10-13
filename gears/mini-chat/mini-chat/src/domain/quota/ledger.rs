//! Reserve and settlement writes on `quota_usage` (DESIGN section 3.7 "Commit semantics",
//! 5.4.3 - 5.4.5).

use mini_chat_sdk::credits_micro_checked;
use opentelemetry::KeyValue;
use toolkit_db::DbTx;
use uuid::Uuid;

use super::billing::SettlementMethod;
use super::periods::{Bucket, Period, PeriodStarts};
use super::preflight::PreflightDecision;
use super::{QuotaService, Usage, bucket_key, limit_of, lock_order};
use crate::domain::error::DomainError;
use crate::infra::db::repo::quota_usage::{self as repo, BucketDelta};
use crate::infra::llm::types::ProviderUsage;

/// Inputs of [`super::QuotaService::settle`], all taken from the turn's persisted preflight values
/// and the policy snapshot of `policy_version_applied`.
#[derive(Debug, Clone)]
pub struct SettleInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// The effective model was premium at preflight: `tier:premium` rows are settled too.
    pub is_premium: bool,
    /// Period starts of the preflight (never recomputed).
    pub periods: PeriodStarts,
    pub turn_reserved_credits_micro: i64,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub minimal_generation_floor_applied: i64,
    pub in_mult: i64,
    pub out_mult: i64,
    pub method: SettlementMethod,
    /// Provider usage (`actual` settlements; `None` counts as zero usage).
    pub usage: Option<ProviderUsage>,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Metrics of a reserve or settlement, collected inside the transaction and recorded by
/// [`QuotaService::record_facts`] only after it committed: a write transaction may run several times
/// (contention retry) or roll back, and metrics are not transactional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[must_use = "record the facts with `QuotaService::record_facts` after the transaction committed"]
pub struct QuotaMetricsFacts {
    reserved: bool,
    /// Actual token count of an actual settlement.
    actual_tokens: Option<i64>,
    /// The actual tokens exceeded the turn's reserve.
    overshoot: bool,
}

/// Outcome of a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleResult {
    /// Credits added to `spent_credits_micro` (the usage event's `actual_credits_micro`).
    pub committed_credits_micro: i64,
    /// The overshoot exceeded `quota.overshoot_tolerance_factor`: the charge was capped at the
    /// reserve.
    pub overshoot_capped: bool,
    /// To be recorded after the commit.
    pub metrics: QuotaMetricsFacts,
}

impl QuotaService {
    /// Records the metrics of a committed reserve or settlement.
    pub fn record_facts(&self, facts: QuotaMetricsFacts) {
        if facts.reserved {
            for period in Period::ALL {
                self.metrics
                    .quota_reserve
                    .add(1, &[KeyValue::new("period", period.as_str())]);
            }
        }
        if let Some(actual_tokens) = facts.actual_tokens {
            #[allow(clippy::cast_precision_loss)]
            // histogram sample; token counts are far below 2^52
            self.metrics
                .quota_actual_tokens
                .record(actual_tokens as f64, &[]);
            for period in Period::ALL {
                let labels = [KeyValue::new("period", period.as_str())];
                self.metrics.quota_commit.add(1, &labels);
                if facts.overshoot {
                    self.metrics.quota_overshoot.add(1, &labels);
                }
            }
        }
    }

    /// Books the decision's reserve inside the caller's transaction: increments
    /// `reserved_credits_micro` of the `total` rows (and `tier:premium` rows of a premium turn)
    /// of both periods, then re-reads the rows and checks `spent + reserved <= limit` for each.
    ///
    /// The increments come first, so the transaction holds the write lock (`SQLite`) or the row
    /// locks (`PostgreSQL`) before the re-check: concurrent reserves are serialized and the
    /// re-check sees every reserve committed before it.
    ///
    /// # Errors
    /// `QuotaExceeded{tokens}` when a bucket is over its limit (the caller rolls back),
    /// `Internal` on a database error.
    ///
    /// Returns the metrics facts; the caller records them after the commit.
    pub async fn reserve(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        user_id: Uuid,
        d: &PreflightDecision,
    ) -> Result<QuotaMetricsFacts, DomainError> {
        let rows = lock_order(d.effective_is_premium);
        let delta = BucketDelta {
            reserved_credits_micro: d.reserve.reserved_credits_micro,
            ..BucketDelta::default()
        };
        for &(period, bucket) in &rows {
            let key = bucket_key(tenant_id, user_id, d.periods, period, bucket);
            repo::add(tx, &key, &delta).await?;
        }
        let scope = repo::owner_scope(tenant_id, user_id);
        let usage = Usage(repo::load_current(tx, &scope, &d.periods, false).await?);
        for &(period, bucket) in &rows {
            if usage.used(period, bucket) > limit_of(&d.limits, bucket, period) {
                return Err(DomainError::QuotaExceeded { scope: "tokens" });
            }
        }
        Ok(QuotaMetricsFacts {
            reserved: true,
            ..QuotaMetricsFacts::default()
        })
    }

    /// Settles a turn inside the caller's (finalization) transaction, on the rows of the
    /// preflight periods:
    /// - `total`: `reserved -= turn reserve`, `spent += committed`, `calls += 1`, tokens (actual
    ///   only), web search / code interpreter calls (not on released);
    /// - `tier:premium` (premium turns): `reserved -= turn reserve`, `spent += committed`,
    ///   `calls += 1`.
    ///
    /// Committed credits: actual usage (capped at the turn's reserve when the token overshoot
    /// exceeds `quota.overshoot_tolerance_factor`), `credits(estimated_input, floor)` for
    /// estimated, 0 for released.
    ///
    /// # Errors
    /// `Internal` when the credits cannot be computed or on a database error.
    pub async fn settle(
        &self,
        tx: &DbTx<'_>,
        input: SettleInput,
    ) -> Result<SettleResult, DomainError> {
        let usage = input.usage.unwrap_or_default();
        let actual = input.method == SettlementMethod::Actual;
        let (committed, overshoot_capped) = match input.method {
            SettlementMethod::Actual => self.actual_charge(&input, &usage)?,
            SettlementMethod::Estimated => {
                let estimated_input = input
                    .reserve_tokens
                    .saturating_sub(input.max_output_tokens_applied);
                (
                    credits(
                        estimated_input,
                        input.minimal_generation_floor_applied,
                        input.in_mult,
                        input.out_mult,
                    )?,
                    false,
                )
            }
            SettlementMethod::Released => (0, false),
        };

        let release = input.turn_reserved_credits_micro.saturating_neg();
        let tool_calls = |n: i64| {
            if input.method == SettlementMethod::Released {
                0
            } else {
                i32::try_from(n).unwrap_or(i32::MAX)
            }
        };
        let total = BucketDelta {
            spent_credits_micro: committed,
            reserved_credits_micro: release,
            calls: 1,
            input_tokens: if actual { usage.input_tokens } else { 0 },
            output_tokens: if actual { usage.output_tokens } else { 0 },
            web_search_calls: tool_calls(input.web_search_calls),
            code_interpreter_calls: tool_calls(input.code_interpreter_calls),
        };
        let premium = BucketDelta {
            spent_credits_micro: committed,
            reserved_credits_micro: release,
            calls: 1,
            ..BucketDelta::default()
        };
        for (period, bucket) in lock_order(input.is_premium) {
            let key = bucket_key(
                input.tenant_id,
                input.user_id,
                input.periods,
                period,
                bucket,
            );
            let delta = match bucket {
                Bucket::Total => &total,
                Bucket::Premium => &premium,
            };
            repo::add(tx, &key, delta).await?;
        }
        let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        Ok(SettleResult {
            committed_credits_micro: committed,
            overshoot_capped,
            metrics: QuotaMetricsFacts {
                reserved: false,
                actual_tokens: actual.then_some(actual_tokens),
                overshoot: actual && actual_tokens > input.reserve_tokens,
            },
        })
    }

    /// Committed credits of an actual settlement and whether the overshoot cap applied
    /// (DESIGN 5.4.5). Pure: the metrics are reported through [`QuotaMetricsFacts`].
    fn actual_charge(
        &self,
        input: &SettleInput,
        usage: &ProviderUsage,
    ) -> Result<(i64, bool), DomainError> {
        let actual_credits = credits(
            usage.input_tokens,
            usage.output_tokens,
            input.in_mult,
            input.out_mult,
        )?;
        let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        if actual_tokens > input.reserve_tokens
            && exceeds_tolerance(
                actual_tokens,
                input.reserve_tokens,
                self.cfg.overshoot_tolerance_factor,
            )
        {
            return Ok((input.turn_reserved_credits_micro, true));
        }
        Ok((actual_credits, false))
    }
}

/// `actual / reserve > factor`, in floating point (DESIGN 5.4.5). A zero reserve is exceeded by
/// any overshoot.
#[allow(clippy::cast_precision_loss)] // a ratio of token counts; f64 is the normative type
fn exceeds_tolerance(actual_tokens: i64, reserve_tokens: i64, factor: f64) -> bool {
    if reserve_tokens <= 0 {
        return true;
    }
    (actual_tokens as f64) / (reserve_tokens as f64) > factor
}

fn credits(input: i64, output: i64, in_mult: i64, out_mult: i64) -> Result<i64, DomainError> {
    credits_micro_checked(input, output, in_mult, out_mult).map_err(|err| {
        tracing::warn!(input, output, in_mult, out_mult, error = %err, "settlement credits cannot be computed");
        DomainError::Internal(format!("settlement credits: {err}"))
    })
}
