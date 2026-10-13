//! `GET /mini-chat/v1/quota/status` (DESIGN §3.2 "Quota Status Endpoint").

use std::sync::Arc;

use axum::Extension;
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::{ApiResult, StatusCode};
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_security::SecurityContext;

use super::{License, V1};
use crate::domain::quota::{self, PeriodStatus, QuotaStatus, TierStatus};
use crate::domain::services::AppServices;

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    Premium,
    Total,
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

/// One period of a tier.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Per-tier quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Quota status of the authenticated user.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<&PeriodStatus> for QuotaPeriodStatus {
    fn from(p: &PeriodStatus) -> Self {
        Self {
            period: if p.period == quota::PERIOD_DAILY { QuotaPeriod::Daily } else { QuotaPeriod::Monthly },
            limit_credits_micro: p.limit_credits_micro,
            used_credits_micro: p.used_credits_micro,
            remaining_credits_micro: p.remaining_credits_micro,
            remaining_percentage: p.remaining_percentage,
            next_reset: p.next_reset,
            warning: p.warning,
            exhausted: p.exhausted,
        }
    }
}

impl From<&TierStatus> for QuotaTierStatus {
    fn from(t: &TierStatus) -> Self {
        Self {
            tier: if t.tier == "premium" { QuotaTier::Premium } else { QuotaTier::Total },
            periods: t.periods.iter().map(QuotaPeriodStatus::from).collect(),
        }
    }
}

impl From<QuotaStatus> for QuotaStatusResponse {
    fn from(s: QuotaStatus) -> Self {
        Self {
            tiers: s.tiers.iter().map(QuotaTierStatus::from).collect(),
            warning_threshold_pct: u32::from(s.warning_threshold_pct),
        }
    }
}

/// Handler of `mini_chat.get_quota_status`.
///
/// # Errors
/// Canonical errors (403 denied, 503 PDP unavailable, 500).
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
) -> ApiResult<axum::Json<QuotaStatusResponse>> {
    let status = quota::quota_status(&app, &ctx).await?;
    Ok(axum::Json(QuotaStatusResponse::from(status)))
}

/// Registers this area's routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    OperationBuilder::get(format!("{V1}/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag("Mini Chat Quotas")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}
