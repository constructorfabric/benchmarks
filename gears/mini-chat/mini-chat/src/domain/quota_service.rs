//! Quota service: credit-based per-user rate limits with the two-tier
//! downgrade cascade (premium → standard), two-phase reserve/settle
//! accounting (DESIGN.md section 5).

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::credits::{credits_micro_checked, credits_micro_or_max};
use crate::domain::services::{ServiceContext, periods};
use crate::error::DomainError;
use crate::infra::storage::repo::quota::{BucketKey, BucketState, bucket, period};

/// Quota decision of a preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaDecision {
    /// The turn uses the selected model.
    Allow,
    /// A downgrade occurred.
    Downgrade {
        /// Why: `premium_quota_exhausted` | `force_standard_tier` |
        /// `disable_premium_tier` | `model_disabled`.
        reason: &'static str,
    },
}

/// Inputs of a preflight evaluation.
#[allow(clippy::struct_excessive_bools, reason = "the four capability flags are the contract")]
pub struct PreflightInput {
    /// Tenant.
    pub tenant_id: Uuid,
    /// User.
    pub user_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// UTF-8 bytes of the current user message.
    pub content_bytes: usize,
    /// Prior context proxy: tokens of the last assistant message with usage.
    pub prior_context_tokens: i64,
    /// Number of image inputs on this turn.
    pub image_count: u32,
    /// The chat has at least one ready document.
    pub has_ready_documents: bool,
    /// The chat has at least one ready code-interpreter attachment.
    pub has_ready_code_interpreter: bool,
    /// `web_search.enabled` on the request.
    pub web_search_requested: bool,
}

/// The preflight outcome: the effective model and its reserve.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "tool-inclusion flags are the contract")]
pub struct PreflightOutcome {
    /// Effective model catalog entry (cloned from the snapshot).
    pub effective_model: ModelCatalogEntry,
    /// Policy version applied.
    pub policy_version: i64,
    /// Allow or downgrade.
    pub decision: QuotaDecision,
    /// Tier of the effective model.
    pub tier: ModelTier,
    /// Reserve tokens (`estimated_input_tokens + max_output_tokens_applied`).
    pub reserve_tokens: i64,
    /// Applied max output tokens.
    pub max_output_tokens_applied: i32,
    /// Reserved credits.
    pub reserved_credits_micro: i64,
    /// Applied minimal generation floor.
    pub minimal_generation_floor_applied: i32,
    /// Whether the `web_search` tool is sent.
    pub web_search_included: bool,
    /// Whether the `file_search` tool is sent (gated by tool support and
    /// kill switch; adapter support is decided separately).
    pub file_search_included: bool,
    /// Whether the `code_interpreter` tool is sent.
    pub code_interpreter_included: bool,
    /// Daily period start used by the reserve.
    pub daily_start: time::Date,
    /// Monthly period start used by the reserve.
    pub monthly_start: time::Date,
}

impl PreflightOutcome {
    /// Bucket rows this turn's reserve was written to.
    #[must_use]
    pub fn bucket_keys(&self) -> Vec<BucketKey> {
        let mut keys = vec![
            BucketKey {
                period_type: period::DAILY,
                period_start: self.daily_start,
                bucket: bucket::TOTAL,
            },
            BucketKey {
                period_type: period::MONTHLY,
                period_start: self.monthly_start,
                bucket: bucket::TOTAL,
            },
        ];
        if self.tier == ModelTier::Premium {
            keys.push(BucketKey {
                period_type: period::DAILY,
                period_start: self.daily_start,
                bucket: bucket::PREMIUM,
            });
            keys.push(BucketKey {
                period_type: period::MONTHLY,
                period_start: self.monthly_start,
                bucket: bucket::PREMIUM,
            });
        }
        keys
    }
}

/// The quota service.
#[allow(clippy::field_names, reason = "bucket row columns mirror the DB schema")]
pub struct QuotaService {
    ctx: Arc<ServiceContext>,
}

/// A resolved policy set for one user.
pub struct PolicySet {
    /// Policy version.
    pub version: i64,
    /// Snapshot.
    pub snapshot: PolicySnapshot,
    /// User limits.
    pub limits: UserLimits,
}

impl QuotaService {
    /// Create the service.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }

    /// Resolve the current policy set for a user.
    pub async fn current_policy(&self, user_id: Uuid) -> Result<PolicySet, DomainError> {
        let version = self
            .ctx
            .policy
            .current_policy_version(user_id)
            .await
            .map_err(crate::domain::chat_service::policy_err)?;
        let snapshot = self
            .ctx
            .policy
            .policy_snapshot(user_id, version)
            .await
            .map_err(crate::domain::chat_service::policy_err)?;
        let limits = self
            .ctx
            .policy
            .user_limits(user_id, version)
            .await
            .map_err(crate::domain::chat_service::policy_err)?;
        Ok(PolicySet {
            version,
            snapshot,
            limits,
        })
    }

    /// Policy snapshot for settlement (by the turn's applied version).
    pub async fn snapshot_for_settlement(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.ctx
            .policy
            .policy_snapshot(user_id, policy_version)
            .await
            .map_err(crate::domain::chat_service::policy_err)
    }

    /// Estimate the current user message in tokens (effective model's
    /// estimation budgets).
    #[must_use]
    #[allow(clippy::integer_division, reason = "canonical ceil formulas are integer by contract")]
    pub fn estimate_text_tokens(model: &ModelCatalogEntry, content_bytes: usize) -> i64 {
        let budgets = &model.estimation_budgets;
        let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
        let base = (i64::try_from(content_bytes).unwrap_or(i64::MAX) + bpt - 1) / bpt
            + i64::from(budgets.fixed_overhead_tokens);
        (base * (100 + i64::from(budgets.safety_margin_pct)) + 99) / 100
    }

    fn candidate_reserve(
        &self,
        m: &ModelCatalogEntry,
        input: &PreflightInput,
        kill: &KillSwitches,
    ) -> (i64, i64, i32, i64) {
        let budgets = &m.estimation_budgets;
        let text = Self::estimate_text_tokens(m, input.content_bytes);
        let mut estimated_input = text + input.prior_context_tokens;
        estimated_input += i64::from(input.image_count) * i64::from(budgets.image_token_budget);
        if input.has_ready_documents
            && m.general_config.tool_support.file_search
            && !kill.disable_file_search
        {
            estimated_input += i64::from(budgets.tool_surcharge_tokens);
        }
        if input.web_search_requested && m.general_config.tool_support.web_search {
            estimated_input += i64::from(budgets.web_search_surcharge_tokens);
        }
        if input.has_ready_code_interpreter
            && m.general_config.tool_support.code_interpreter
            && !kill.disable_code_interpreter
        {
            estimated_input += i64::from(budgets.code_interpreter_surcharge_tokens);
        }
        let max_output_applied = m
            .max_output_tokens
            .min(self.ctx.config.streaming.max_output_tokens);
        let reserve_tokens = estimated_input + i64::from(max_output_applied);
        let reserved_credits = credits_micro_or_max(
            estimated_input,
            i64::from(max_output_applied),
            m.input_tokens_credit_multiplier_micro,
            m.output_tokens_credit_multiplier_micro,
        );
        (
            estimated_input,
            reserve_tokens,
            i32::try_from(max_output_applied).unwrap_or(i32::MAX),
            reserved_credits,
        )
    }

#[allow(clippy::too_many_lines, reason = "the cascade is one linear normative algorithm")]
    /// Run the preflight: downgrade cascade, daily tool quotas.
    ///
    /// Does NOT write the reserve (that happens in the reserve transaction
    /// via [`QuotaService::write_reserve`]).
    ///
    /// # Errors
    ///
    /// - [`DomainError::QuotaExceeded`] when all tiers are exhausted
    /// - [`DomainError::Internal`] on policy resolution failure
    pub async fn preflight(
        &self,
        input: &PreflightInput,
    ) -> Result<(PolicySet, PreflightOutcome), DomainError> {
        let policy = self.current_policy(input.user_id).await?;
        let snapshot = &policy.snapshot;
        let limits = &policy.limits;
        let kill = &snapshot.kill_switches;

        let now = OffsetDateTime::now_utc();
        let daily_start = periods::daily_start(now);
        let monthly_start = periods::monthly_start(now);

        // Read all bucket rows needed for the cascade in one query.
        let keys = [
            BucketKey {
                period_type: period::DAILY,
                period_start: daily_start,
                bucket: bucket::TOTAL,
            },
            BucketKey {
                period_type: period::MONTHLY,
                period_start: monthly_start,
                bucket: bucket::TOTAL,
            },
            BucketKey {
                period_type: period::DAILY,
                period_start: daily_start,
                bucket: bucket::PREMIUM,
            },
            BucketKey {
                period_type: period::MONTHLY,
                period_start: monthly_start,
                bucket: bucket::PREMIUM,
            },
        ];
        let scope = toolkit_db::secure::AccessScope::allow_all();
        let states: std::collections::HashMap<(String, String, String), BucketState> = self
            .ctx
            .repos
            .quota
            .read_buckets(
                &self.ctx.db.conn()?,
                &scope,
                input.tenant_id,
                input.user_id,
                &keys,
            )
            .await?
            .into_iter()
            .map(|(k, s)| {
                (
                    (
                        k.period_type.to_owned(),
                        k.period_start.to_string(),
                        k.bucket.to_owned(),
                    ),
                    s,
                )
            })
            .collect();

        let bucket_ok = |bucket_name: &'static str,
                         period_type: &'static str,
                         period_start: time::Date,
                         limit: i64,
                         reserve: i64| {
            let state = states
                .get(&(
                    period_type.to_owned(),
                    period_start.to_string(),
                    bucket_name.to_owned(),
                ))
                .copied()
                .unwrap_or_default();
            state.spent_credits_micro + state.reserved_credits_micro + reserve <= limit
        };

        let tier_available = |tier: ModelTier, reserve: i64| {
            let total_ok = bucket_ok(
                bucket::TOTAL,
                period::DAILY,
                daily_start,
                limits.standard.limit_daily_credits_micro,
                reserve,
            ) && bucket_ok(
                bucket::TOTAL,
                period::MONTHLY,
                monthly_start,
                limits.standard.limit_monthly_credits_micro,
                reserve,
            );
            if tier == ModelTier::Standard {
                return total_ok;
            }
            total_ok
                && bucket_ok(
                    bucket::PREMIUM,
                    period::DAILY,
                    daily_start,
                    limits.premium.limit_daily_credits_micro,
                    reserve,
                )
                && bucket_ok(
                    bucket::PREMIUM,
                    period::MONTHLY,
                    monthly_start,
                    limits.premium.limit_monthly_credits_micro,
                    reserve,
                )
        };

        // Starting tier and initial downgrade reason.
        let selected_entry = snapshot.model(&input.selected_model);
        let (start_tier, mut reason): (ModelTier, Option<&'static str>) = match selected_entry {
            Some(e) if e.enabled => (e.tier, None),
            Some(e) => (e.tier, Some("model_disabled")),
            None => (ModelTier::Premium, Some("model_disabled")),
        };
        let cascade: &[ModelTier] = match start_tier {
            ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
            ModelTier::Standard => &[ModelTier::Standard],
        };

        let mut effective: Option<(&ModelCatalogEntry, i64, i32, i64)> = None;
        for tier in cascade {
            if *tier == ModelTier::Premium && (kill.force_standard_tier || kill.disable_premium_tier)
            {
                if reason.is_none() {
                    reason = Some(if kill.force_standard_tier {
                        "force_standard_tier"
                    } else {
                        "disable_premium_tier"
                    });
                }
                continue;
            }
            let candidate = selected_entry
                .filter(|e| e.enabled && e.tier == *tier)
                .or_else(|| {
                    snapshot
                        .model_catalog
                        .iter()
                        .find(|m| m.enabled && m.tier == *tier && m.preference.is_some_and(|p| p.is_default))
                })
                .or_else(|| {
                    snapshot
                        .model_catalog
                        .iter()
                        .find(|m| m.enabled && m.tier == *tier)
                });
            let Some(candidate) = candidate else {
                continue;
            };
            let (_, reserve_tokens, max_output_applied, reserved_credits) =
                self.candidate_reserve(candidate, input, kill);
            if tier_available(*tier, reserved_credits) {
                effective = Some((candidate, reserve_tokens, max_output_applied, reserved_credits));
                break;
            }
            if *tier == ModelTier::Premium && reason.is_none() {
                reason = Some("premium_quota_exhausted");
            }
        }

        let Some((model, reserve_tokens, max_output_applied, reserved_credits)) = effective else {
            return Err(DomainError::QuotaExceeded("tokens"));
        };

        let decision = if model.id == input.selected_model && reason.is_none() {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade {
                reason: reason.unwrap_or("premium_quota_exhausted"),
            }
        };

        // Tool inclusion gates (effective model, kill switches).
        let web_search_included =
            input.web_search_requested && model.general_config.tool_support.web_search;
        let file_search_included = input.has_ready_documents
            && model.general_config.tool_support.file_search
            && !kill.disable_file_search;
        let code_interpreter_included = input.has_ready_code_interpreter
            && model.general_config.tool_support.code_interpreter
            && !kill.disable_code_interpreter;

        // Daily tool quotas (only when the tool is sent).
        let daily_total = states
            .get(&(
                period::DAILY.to_owned(),
                daily_start.to_string(),
                bucket::TOTAL.to_owned(),
            ))
            .copied()
            .unwrap_or_default();
        if web_search_included
            && i64::from(daily_total.web_search_calls)
                >= i64::from(self.ctx.config.quota.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded("web_search"));
        }
        if code_interpreter_included
            && i64::from(daily_total.code_interpreter_calls)
                >= i64::from(self.ctx.config.quota.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded("code_interpreter"));
        }

        let floor = i64::from(self.ctx.config.estimation_budgets.minimal_generation_floor)
            .min(i64::from(max_output_applied));

        let outcome = PreflightOutcome {
            effective_model: model.clone(),
            policy_version: policy.version,
            decision,
            tier: model.tier,
            reserve_tokens,
            max_output_tokens_applied: i32::try_from(max_output_applied).unwrap_or(i32::MAX),
            reserved_credits_micro: reserved_credits,
            minimal_generation_floor_applied: i32::try_from(floor).unwrap_or(i32::MAX),
            web_search_included,
            file_search_included,
            code_interpreter_included,
            daily_start,
            monthly_start,
        };
        Ok((policy, outcome))
    }

    /// Write the reserve and re-check the limits (inside the reserve
    /// transaction). Returns the re-check failure as
    /// [`DomainError::QuotaExceeded`] (`tokens`).
    ///
    /// # Errors
    ///
    /// - [`DomainError::QuotaExceeded`] when a bucket is over its limit
    /// - [`DomainError::Internal`] on persistence failure
    pub async fn write_reserve(
        &self,
        tx: &impl toolkit_db::secure::DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        outcome: &PreflightOutcome,
    ) -> Result<(), DomainError> {
        for key in outcome.bucket_keys() {
            self.ctx
                .repos
                .quota
                .add_reserve(tx, tenant_id, user_id, key, outcome.reserved_credits_micro)
                .await?;
        }
        // Re-read and re-check (the increments hold the row locks).
        let scope = toolkit_db::secure::AccessScope::allow_all();
        let keys = outcome.bucket_keys();
        let states = self
            .ctx
            .repos
            .quota
            .read_buckets(tx, &scope, tenant_id, user_id, &keys)
            .await?;
        for (key, state) in states {
            let limit = match (key.bucket, key.period_type) {
                (bucket::TOTAL, period::DAILY) => limits.standard.limit_daily_credits_micro,
                (bucket::TOTAL, _) => limits.standard.limit_monthly_credits_micro,
                (_, period::DAILY) => limits.premium.limit_daily_credits_micro,
                (_, _) => limits.premium.limit_monthly_credits_micro,
            };
            if state.spent_credits_micro + state.reserved_credits_micro > limit {
                return Err(DomainError::QuotaExceeded("tokens"));
            }
        }
        Ok(())
    }
}

/// `Settlement` input: the terminal billing classification of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementKind {
    /// Completed (actual usage; zero usage still settles actual).
    Completed,
    /// Failed after the provider call started.
    Failed,
    /// Aborted (client disconnect, orphan watchdog) — always estimated.
    Aborted,
}

/// The computed settlement of a turn.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    /// Billing outcome string (`completed` | `failed` | `aborted`).
    pub billing_outcome: &'static str,
    /// `Settlement` method (`actual` | `estimated`).
    pub settlement_method: &'static str,
    /// Credits to commit.
    pub committed_credits_micro: i64,
    /// Actual token telemetry (only on actual settlements).
    pub usage: Option<mini_chat_sdk::UsageTokens>,
    /// Whether actual tokens exceeded the reserve (overshoot).
    pub overshoot: bool,
}

/// Compute the settlement of a turn.
///
/// # Errors
///
/// Returns [`crate::domain::credits::CreditError`] when the credit
/// computation fails (out-of-range provider usage or multipliers).
pub fn compute_settlement(
    kind: SettlementKind,
    reserve_tokens: i64,
    max_output_tokens_applied: i64,
    minimal_generation_floor_applied: i64,
    reserved_credits_micro: i64,
    usage: Option<mini_chat_sdk::UsageTokens>,
    in_mult: i64,
    out_mult: i64,
    overshoot_tolerance_factor: f64,
) -> Result<Settlement, crate::domain::credits::CreditError> {
    let estimated_input_tokens = reserve_tokens - max_output_tokens_applied;
    match kind {
        SettlementKind::Completed | SettlementKind::Failed
            if usage.is_some_and(|u| {
                kind == SettlementKind::Completed
                    || u.input_tokens > 0
                    || u.output_tokens > 0
            }) =>
        {
            let usage = usage.unwrap_or_default();
            let actual_credits = credits_micro_checked(
                usage.input_tokens,
                usage.output_tokens,
                in_mult,
                out_mult,
            )?;
            let actual_tokens = usage.input_tokens + usage.output_tokens;
            let overshoot = actual_tokens > reserve_tokens;
            #[allow(clippy::cast_precision_loss, reason = "f64 division is normative here (DESIGN 5.4.5)")]
            let committed = if overshoot
                && reserve_tokens > 0
                && (actual_tokens as f64) / (reserve_tokens as f64) > overshoot_tolerance_factor
            {
                // Overshoot beyond tolerance: cap billing at the reserve.
                reserved_credits_micro
            } else {
                actual_credits
            };
            Ok(Settlement {
                billing_outcome: if kind == SettlementKind::Completed {
                    "completed"
                } else {
                    "failed"
                },
                settlement_method: "actual",
                committed_credits_micro: committed,
                usage: Some(usage),
                overshoot,
            })
        }
        SettlementKind::Completed => {
            // Completed with no usage: charge 0 credits on the actual path.
            Ok(Settlement {
                billing_outcome: "completed",
                settlement_method: "actual",
                committed_credits_micro: 0,
                usage: None,
                overshoot: false,
            })
        }
        SettlementKind::Failed | SettlementKind::Aborted => {
            let charged_output_tokens = minimal_generation_floor_applied;
            let committed = credits_micro_checked(
                estimated_input_tokens,
                charged_output_tokens,
                in_mult,
                out_mult,
            )?;
            Ok(Settlement {
                billing_outcome: if kind == SettlementKind::Failed {
                    "failed"
                } else {
                    "aborted"
                },
                settlement_method: "estimated",
                committed_credits_micro: committed,
                usage: None,
                overshoot: false,
            })
        }
    }
}
