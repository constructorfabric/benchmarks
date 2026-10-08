//! Preflight cascade and reserve booking (DESIGN §4 "Downgrade Decision Flow", §5.4.1–5.4.3).

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use toolkit_db::DbTx;
use uuid::Uuid;

use super::buckets::{self, Rows};
use super::{
    BUCKET_PREMIUM, BUCKET_TOTAL, EnabledTools, PERIOD_DAILY, PeriodStarts, PreflightDecision, PreflightRequest,
    QuotaDecisionKind, ToolInputs,
};
use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::quota_usage::Column;

pub const REASON_PREMIUM_EXHAUSTED: &str = "premium_quota_exhausted";
pub const REASON_FORCE_STANDARD: &str = "force_standard_tier";
pub const REASON_DISABLE_PREMIUM: &str = "disable_premium_tier";
pub const REASON_MODEL_DISABLED: &str = "model_disabled";

/// Reserve figures of one cascade candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    /// `i64::MAX` when the credit computation failed (candidate unavailable).
    pub reserved_credits_micro: i64,
}

/// Tools sent with `model` (same gates as the reserve surcharges).
pub(super) fn tools_for(model: &ModelCatalogEntry, tools: ToolInputs, ks: KillSwitches) -> EnabledTools {
    let ts = &model.general_config.tool_support;
    EnabledTools {
        file_search: tools.has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: tools.web_search_requested && ts.web_search,
        code_interpreter: tools.has_ready_code_interpreter && ts.code_interpreter && !ks.disable_code_interpreter,
    }
}

/// Reserve the candidate would book (DESIGN §5.4.1).
pub(super) fn candidate_reserve(
    cfg: &MiniChatConfig,
    model: &ModelCatalogEntry,
    req: &PreflightRequest,
    ks: KillSwitches,
) -> CandidateReserve {
    let b = &model.estimation_budgets;
    let tools = tools_for(model, req.tools, ks);
    let mut est = super::estimate_text_tokens(req.message_bytes, b);
    est = est.saturating_add(req.prior_context_tokens.max(0));
    est = est.saturating_add(i64::from(req.image_count).saturating_mul(i64::from(b.image_token_budget)));
    if tools.file_search {
        est = est.saturating_add(i64::from(b.tool_surcharge_tokens));
    }
    if tools.web_search {
        est = est.saturating_add(i64::from(b.web_search_surcharge_tokens));
    }
    if tools.code_interpreter {
        est = est.saturating_add(i64::from(b.code_interpreter_surcharge_tokens));
    }
    let max_out = i64::from(model.max_output_tokens.min(cfg.streaming.max_output_tokens));
    let credits = super::credits_micro(
        est,
        max_out,
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %model.id, error = %e, "cascade candidate reserve cannot be computed; treated as unavailable");
        i64::MAX
    });
    CandidateReserve { estimated_input_tokens: est, max_output_tokens_applied: max_out, reserved_credits_micro: credits }
}

/// Candidate of a tier: the selected model when enabled in the tier, else the tier's enabled
/// `is_default` model, else its first enabled model (catalog order).
pub(super) fn candidate_of<'a>(snapshot: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let mut enabled = snapshot.model_catalog.iter().filter(|m| m.enabled && m.tier == tier);
    if let Some(m) = enabled.clone().find(|m| m.id == selected) {
        return Some(m);
    }
    if let Some(m) = enabled.clone().find(|m| m.is_default()) {
        return Some(m);
    }
    enabled.next()
}

/// Result of the cascade (before the tool quota checks).
#[derive(Debug, Clone)]
pub(super) struct CascadeOutcome {
    pub model: ModelCatalogEntry,
    pub reserve: CandidateReserve,
    pub reason: Option<String>,
}

/// Runs the downgrade cascade over the loaded bucket rows.
pub(super) fn cascade(
    cfg: &MiniChatConfig,
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    rows: &Rows,
    req: &PreflightRequest,
    periods: PeriodStarts,
) -> Option<CascadeOutcome> {
    let ks = snapshot.kill_switches;
    let mut reason: Option<String> = None;
    let start_tier = match snapshot.model(&req.selected_model) {
        Some(m) if m.enabled => m.tier,
        Some(m) => {
            reason = Some(REASON_MODEL_DISABLED.to_owned());
            m.tier
        }
        None => {
            reason = Some(REASON_MODEL_DISABLED.to_owned());
            ModelTier::Premium
        }
    };
    let tiers: &[ModelTier] =
        if start_tier == ModelTier::Premium { &[ModelTier::Premium, ModelTier::Standard] } else { &[ModelTier::Standard] };
    for &tier in tiers {
        if tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                let r = if ks.force_standard_tier { REASON_FORCE_STANDARD } else { REASON_DISABLE_PREMIUM };
                reason = Some(r.to_owned());
            }
            continue;
        }
        let Some(model) = candidate_of(snapshot, tier, &req.selected_model) else {
            continue;
        };
        let reserve = candidate_reserve(cfg, model, req, ks);
        let mut available = rows.fits(limits, BUCKET_TOTAL, periods, reserve.reserved_credits_micro);
        if tier == ModelTier::Premium {
            available = available && rows.fits(limits, BUCKET_PREMIUM, periods, reserve.reserved_credits_micro);
        }
        if available {
            return Some(CascadeOutcome { model: model.clone(), reserve, reason });
        }
        if tier == ModelTier::Premium && reason.is_none() {
            reason = Some(REASON_PREMIUM_EXHAUSTED.to_owned());
        }
    }
    None
}

pub(super) async fn preflight(app: &AppServices, req: &PreflightRequest) -> Result<PreflightDecision, DomainError> {
    let snapshot: Arc<PolicySnapshot> = app.policy.current_snapshot(req.user_id).await?;
    if req.tools.web_search_requested && snapshot.kill_switches.disable_web_search {
        return Err(DomainError::feature_disabled("web_search"));
    }
    let limits = app.policy.user_limits(req.user_id, snapshot.policy_version).await?;
    let periods = super::period_starts(req.now);

    let (tenant_id, user_id) = (req.tenant_id, req.user_id);
    let rows = app
        .db
        .transaction(move |tx| Box::pin(async move { buckets::load_rows(tx, tenant_id, user_id, periods).await }))
        .await?;

    let cfg = &app.cfg;
    let Some(outcome) = cascade(cfg, &snapshot, &limits, &rows, req, periods) else {
        return Err(DomainError::quota_exceeded("tokens"));
    };

    let tools = tools_for(&outcome.model, req.tools, snapshot.kill_switches);
    let daily_total = rows.get(PERIOD_DAILY, BUCKET_TOTAL);
    if tools.web_search {
        let used = daily_total.map_or(0, |r| i64::from(r.web_search_calls));
        if used >= i64::from(cfg.quota.web_search_daily_quota) {
            return Err(DomainError::quota_exceeded("web_search"));
        }
    }
    if tools.code_interpreter {
        let used = daily_total.map_or(0, |r| i64::from(r.code_interpreter_calls));
        if used >= i64::from(cfg.quota.code_interpreter_daily_quota) {
            return Err(DomainError::quota_exceeded("code_interpreter"));
        }
    }

    let reserve = outcome.reserve;
    let decision = if outcome.model.id == req.selected_model && outcome.reason.is_none() {
        QuotaDecisionKind::Allow
    } else {
        QuotaDecisionKind::Downgrade
    };
    let floor = i64::from(cfg.estimation_budgets.minimal_generation_floor).min(reserve.max_output_tokens_applied);
    let policy_version = snapshot.policy_version;
    let effective_tier = outcome.model.tier;
    Ok(PreflightDecision {
        snapshot,
        limits,
        selected_model: req.selected_model.clone(),
        effective_model: outcome.model,
        effective_tier,
        decision,
        downgrade_reason: outcome.reason,
        tools,
        estimated_input_tokens: reserve.estimated_input_tokens,
        max_output_tokens_applied: reserve.max_output_tokens_applied,
        reserve_tokens: reserve.estimated_input_tokens.saturating_add(reserve.max_output_tokens_applied),
        reserved_credits_micro: reserve.reserved_credits_micro,
        minimal_generation_floor_applied: floor,
        policy_version,
        periods,
    })
}

/// Bucket names booked for a tier.
pub(super) fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    if tier == ModelTier::Premium { &[BUCKET_TOTAL, BUCKET_PREMIUM] } else { &[BUCKET_TOTAL] }
}

pub(super) async fn reserve(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    user_id: Uuid,
    decision: &PreflightDecision,
) -> Result<(), DomainError> {
    let now = crate::clock::now();
    let amount = decision.reserved_credits_micro;
    let bucket_names = buckets_for(decision.effective_tier);
    for (period_type, start) in buckets::periods_of(decision.periods) {
        for bucket in bucket_names {
            buckets::ensure_row(tx, tenant_id, user_id, period_type, start, bucket, now).await?;
            let exprs = vec![
                buckets::add_expr(Column::ReservedCreditsMicro, amount),
                (Column::UpdatedAt, sea_orm::sea_query::Expr::value(now)),
            ];
            buckets::update_row(tx, tenant_id, user_id, period_type, start, bucket, exprs).await?;
        }
    }
    let rows = buckets::load_rows(tx, tenant_id, user_id, decision.periods).await?;
    for bucket in bucket_names {
        if !rows.fits(&decision.limits, bucket, decision.periods, 0) {
            tracing::info!(%tenant_id, %user_id, bucket, "reserve re-check failed; rejecting with quota_exceeded");
            return Err(DomainError::quota_exceeded("tokens"));
        }
    }
    Ok(())
}
