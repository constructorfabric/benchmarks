//! Quota preflight: kill switches, downgrade cascade with per-candidate reserve, daily tool
//! quotas (DESIGN "Downgrade Decision Flow", sections 5.4.1, 5.4.2, 5.5.6).

use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits, credits_micro_checked,
};
use opentelemetry::KeyValue;
use time::OffsetDateTime;
use uuid::Uuid;

use super::estimate::estimate_text_tokens;
use super::periods::{Bucket, Period, PeriodStarts, period_starts};
use super::{QuotaService, Usage, limit_of, lock_order};
use crate::config::QuotaConfig;
use crate::domain::error::DomainError;
use crate::infra::db::repo::quota_usage as repo;
use crate::infra::db::tx::write_tx;

const MODEL_DISABLED: &str = "model_disabled";
const FORCE_STANDARD_TIER: &str = "force_standard_tier";
const DISABLE_PREMIUM_TIER: &str = "disable_premium_tier";
const PREMIUM_QUOTA_EXHAUSTED: &str = "premium_quota_exhausted";

/// Inputs of [`super::QuotaService::preflight`].
#[allow(clippy::struct_excessive_bools)] // independent request facts
#[derive(Debug, Clone)]
pub struct PreflightInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// The chat's model id (may be disabled or missing from the catalog).
    pub selected_model: String,
    pub snapshot: PolicySnapshot,
    pub limits: UserLimits,
    /// UTF-8 bytes of the user message.
    pub message_bytes: usize,
    /// `input_tokens + output_tokens` of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    pub image_count: u32,
    /// The chat has at least one ready document for file search.
    pub chat_has_ready_documents: bool,
    /// The chat has at least one ready code-interpreter file.
    pub chat_has_ready_ci_files: bool,
    pub web_search_requested: bool,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor` of the gear configuration.
    pub minimal_generation_floor: u32,
    pub now: OffsetDateTime,
}

/// Quota decision of a preflight that admits the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// The selected model runs.
    Allow,
    /// Another model runs (see `downgrade_reason`).
    Downgrade,
}

impl QuotaDecision {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Tools sent to the provider for the turn (the same gates decide the reserve surcharges).
#[allow(clippy::struct_excessive_bools)] // one flag per tool
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolGates {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Reserve of the effective model, persisted on the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveAmounts {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i32,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// `credits_micro(estimated_input_tokens, max_output_tokens_applied, in_mult, out_mult)`.
    pub reserved_credits_micro: i64,
    /// `min(minimal_generation_floor, max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: i32,
}

/// Result of an admitted preflight.
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub effective_model: ModelCatalogEntry,
    pub effective_is_premium: bool,
    pub quota_decision: QuotaDecision,
    /// `premium_quota_exhausted`, `force_standard_tier`, `disable_premium_tier` or
    /// `model_disabled`; `None` for `Allow`.
    pub downgrade_reason: Option<&'static str>,
    pub reserve: ReserveAmounts,
    pub policy_version: i64,
    pub periods: PeriodStarts,
    pub limits: UserLimits,
    pub tools: ToolGates,
    pub vision_supported: bool,
}

impl QuotaService {
    /// Admits a turn: picks the effective model with its reserve, or rejects it.
    ///
    /// Order: `web_search` requested under `disable_web_search` is rejected first; then the
    /// downgrade cascade (each tier's candidate checked with the reserve it would book, against
    /// every bucket and period of the tier); then the daily web-search and code-interpreter
    /// quotas of the tools the effective model is sent with. The bucket rows are read in one
    /// transaction (`FOR UPDATE` on `PostgreSQL`); nothing is written.
    ///
    /// # Errors
    /// `FeatureDisabled{web_search}`, `QuotaExceeded{tokens|web_search|code_interpreter}`,
    /// `Internal` on a database error.
    pub async fn preflight(&self, input: PreflightInput) -> Result<PreflightDecision, DomainError> {
        if input.web_search_requested && input.snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                subject: "web_search",
            });
        }
        let periods = period_starts(input.now);
        let scope = repo::owner_scope(input.tenant_id, input.user_id);
        let rows = write_tx(&self.db, move |tx| {
            let scope = scope.clone();
            Box::pin(async move { repo::load_current(tx, &scope, &periods, true).await })
        })
        .await?;
        let result = decide(&input, &Usage(rows), periods, &self.cfg);
        self.record(&input, &result);
        result
    }

    fn record(&self, input: &PreflightInput, result: &Result<PreflightDecision, DomainError>) {
        let (decision, model, tier) = match result {
            Ok(d) => (
                d.quota_decision.as_str(),
                d.effective_model.id.clone(),
                tier_label(d.effective_is_premium),
            ),
            Err(DomainError::QuotaExceeded { .. }) => (
                "reject",
                input.selected_model.clone(),
                tier_label(start_tier(input).0 == ModelTier::Premium),
            ),
            Err(_) => return,
        };
        self.metrics.quota_preflight.add(
            1,
            &[
                KeyValue::new("decision", decision),
                KeyValue::new("model", model),
                KeyValue::new("tier", tier),
            ],
        );
        if let Ok(d) = result {
            #[allow(clippy::cast_precision_loss)]
            // histogram sample; token counts are far below 2^52
            self.metrics
                .quota_estimated_tokens
                .record(d.reserve.reserve_tokens as f64, &[]);
        }
    }
}

const fn tier_label(premium: bool) -> &'static str {
    if premium { "premium" } else { "standard" }
}

/// Starting tier of the cascade and the initial downgrade reason (DESIGN step 1).
fn start_tier(input: &PreflightInput) -> (ModelTier, Option<&'static str>) {
    match selected(input) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some(MODEL_DISABLED)),
        None => (ModelTier::Premium, Some(MODEL_DISABLED)),
    }
}

fn selected(input: &PreflightInput) -> Option<&ModelCatalogEntry> {
    input
        .snapshot
        .model_catalog
        .iter()
        .find(|m| m.id == input.selected_model)
}

/// The cascade and tool-quota checks over the rows read by the preflight.
fn decide(
    input: &PreflightInput,
    usage: &Usage,
    periods: PeriodStarts,
    cfg: &QuotaConfig,
) -> Result<PreflightDecision, DomainError> {
    let ks = input.snapshot.kill_switches;
    let (start, mut reason) = start_tier(input);
    let cascade: &[ModelTier] = match start {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };

    let mut chosen = None;
    for &tier in cascade {
        let premium = tier == ModelTier::Premium;
        if premium && ks.force_standard_tier {
            reason.get_or_insert(FORCE_STANDARD_TIER);
            continue;
        }
        if premium && ks.disable_premium_tier {
            reason.get_or_insert(DISABLE_PREMIUM_TIER);
            continue;
        }
        let Some(candidate) = candidate(input, tier) else {
            continue;
        };
        let tools = tool_gates(input, candidate, ks);
        let reserve = reserve_of(input, candidate, tools);
        if tier_available(
            usage,
            &input.limits,
            premium,
            reserve.reserved_credits_micro,
        ) {
            chosen = Some((candidate, tools, reserve));
            break;
        }
        if premium {
            reason.get_or_insert(PREMIUM_QUOTA_EXHAUSTED);
        }
    }
    let Some((model, tools, reserve)) = chosen else {
        return Err(DomainError::QuotaExceeded { scope: "tokens" });
    };

    let daily_total = usage.row(Period::Daily, Bucket::Total);
    let web_calls = daily_total.map_or(0, |r| r.web_search_calls);
    if tools.web_search && i64::from(web_calls) >= i64::from(cfg.web_search_daily_quota) {
        return Err(DomainError::QuotaExceeded {
            scope: "web_search",
        });
    }
    let ci_calls = daily_total.map_or(0, |r| r.code_interpreter_calls);
    if tools.code_interpreter && i64::from(ci_calls) >= i64::from(cfg.code_interpreter_daily_quota)
    {
        return Err(DomainError::QuotaExceeded {
            scope: "code_interpreter",
        });
    }

    let quota_decision = if model.id == input.selected_model && reason.is_none() {
        QuotaDecision::Allow
    } else {
        QuotaDecision::Downgrade
    };
    Ok(PreflightDecision {
        effective_model: model.clone(),
        effective_is_premium: model.is_premium(),
        quota_decision,
        downgrade_reason: match quota_decision {
            QuotaDecision::Allow => None,
            QuotaDecision::Downgrade => reason,
        },
        reserve,
        policy_version: input.snapshot.policy_version,
        periods,
        limits: input.limits,
        tools,
        vision_supported: model.supports_vision(),
    })
}

/// Candidate of `tier` among its enabled models: the selected model, else the tenant default,
/// else the first in catalog order.
fn candidate(input: &PreflightInput, tier: ModelTier) -> Option<&ModelCatalogEntry> {
    let in_tier = |m: &&ModelCatalogEntry| m.enabled && m.tier == tier;
    let catalog = &input.snapshot.model_catalog;
    selected(input)
        .filter(in_tier)
        .or_else(|| {
            catalog
                .iter()
                .filter(in_tier)
                .find(|m| m.preference.as_ref().is_some_and(|p| p.is_default))
        })
        .or_else(|| catalog.iter().find(in_tier))
}

/// Tools sent with `model`: by the chat's ready attachments, the request, the model's tool
/// support and the kill switches.
fn tool_gates(input: &PreflightInput, model: &ModelCatalogEntry, ks: KillSwitches) -> ToolGates {
    let support = model.tool_support();
    ToolGates {
        file_search: input.chat_has_ready_documents
            && support.file_search
            && !ks.disable_file_search,
        web_search: input.web_search_requested && support.web_search,
        code_interpreter: input.chat_has_ready_ci_files
            && support.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

/// The reserve `model` would book (DESIGN 5.4.1). A reserve whose credits cannot be computed is
/// `i64::MAX`, which makes the candidate unavailable.
fn reserve_of(
    input: &PreflightInput,
    model: &ModelCatalogEntry,
    tools: ToolGates,
) -> ReserveAmounts {
    let b = &model.estimation_budgets;
    let surcharge = |on: bool, tokens: u32| if on { i64::from(tokens) } else { 0 };
    let estimated_input_tokens = estimate_text_tokens(input.message_bytes, b)
        .saturating_add(input.prior_context_tokens.max(0))
        .saturating_add(
            i64::from(input.image_count).saturating_mul(i64::from(b.image_token_budget)),
        )
        .saturating_add(surcharge(tools.file_search, b.tool_surcharge_tokens))
        .saturating_add(surcharge(tools.web_search, b.web_search_surcharge_tokens))
        .saturating_add(surcharge(
            tools.code_interpreter,
            b.code_interpreter_surcharge_tokens,
        ));
    let max_output = model
        .max_output_tokens
        .min(input.streaming_max_output_tokens);
    let floor = input.minimal_generation_floor.min(max_output);
    let reserved_credits_micro = credits_micro_checked(
        estimated_input_tokens,
        i64::from(max_output),
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|err| {
        tracing::warn!(
            model = %model.id,
            error = %err,
            "reserve of a cascade candidate cannot be computed; candidate unavailable"
        );
        i64::MAX
    });
    ReserveAmounts {
        estimated_input_tokens,
        max_output_tokens_applied: i32::try_from(max_output).unwrap_or(i32::MAX),
        reserve_tokens: estimated_input_tokens.saturating_add(i64::from(max_output)),
        reserved_credits_micro,
        minimal_generation_floor_applied: i32::try_from(floor).unwrap_or(i32::MAX),
    }
}

/// `spent + reserved + reserve <= limit` for every bucket of the tier and every period.
fn tier_available(usage: &Usage, limits: &UserLimits, premium: bool, reserve: i64) -> bool {
    lock_order(premium).into_iter().all(|(period, bucket)| {
        usage
            .used(period, bucket)
            .checked_add(reserve)
            .is_some_and(|total| total <= limit_of(limits, bucket, period))
    })
}
