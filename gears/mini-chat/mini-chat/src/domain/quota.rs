//! Quota and billing (feature `cpt-cf-mini-chat-feature-quota-and-billing`): credit
//! arithmetic, period boundaries, token estimation, the reserve and the premium-to-standard
//! downgrade cascade, the daily tool quota check, the reserve write, the settlement by
//! terminal outcome, the usage event, the quota status computation and the ports the
//! infrastructure layer implements.
//!
//! The pure rules are free functions over plain values, so each is testable on its own. The
//! stateful contracts are methods of [`QuotaService`]: `preflight` (decision, writes nothing),
//! `write_reserve` and `finalize_turn` (both run inside the caller's database transaction
//! `tx`, so the streaming pipeline composes them with its own writes and fires the returned
//! `Wake` and the metrics only after the commit) and `quota_status`.
//!
//! The vocabulary types (enums with their wire strings, plain containers) carry no behavior of
//! their own; the rules live in the submodules.

pub mod availability;
pub mod billing;
pub mod cascade;
pub mod credits;
pub mod estimation;
pub mod periods;
pub mod ports;
pub mod rows;
pub mod service;
pub mod settings;
pub mod settlement;
pub mod status;
pub mod tool_quota;
pub mod usage_event;

pub use availability::{
    BucketLimit, bucket_available, checked_limits, limit_of, required_buckets, tier_available,
};
pub use billing::{
    BillingDerivation, BillingOutcome, SettlementMethod, TerminalState, derive_billing,
};
pub use cascade::{CascadeDecision, CascadeInput, DowngradeReason, run_cascade};
pub use credits::{CreditError, credits_micro};
pub use estimation::{
    RequestFacts, ReserveCalc, TokenEstimate, calculate_reserve, estimate_tokens, input_too_long,
};
pub use periods::{PeriodStarts, PeriodType, next_reset};
pub use ports::{
    Clock, QuotaLedger, QuotaMetrics, ReserveIncrement, SettleDelta, TurnPreflightColumns,
    TurnRecord, TurnTerminalUpdate, UsagePublishError, UsagePublisher, UsageQueue,
};
pub use rows::{Bucket, BucketKey, BucketRow, QuotaRows};
pub use service::{
    FinalizeOutcome, FinalizeRequest, Finalizer, PreflightDecision, PreflightRequest, QuotaDeps,
    QuotaService, ReserveReceipt, ReserveTurn, Settlement, SettlementResult, TurnIdentity,
};
pub use settings::QuotaSettings;
pub use settlement::{SettlementComputation, TurnBilling, compute_settlement};
pub use status::{
    PeriodStatus, QuotaStatus, QuotaTier, QuotaWarning, TierStatus, compute_status, quota_warnings,
};
pub use tool_quota::check_tool_quota;
pub use usage_event::{
    NoReserveUsageEventInput, SystemTaskUsageInput, ToolCallCounts, TurnUsageEventInput,
    build_no_reserve_usage_event, build_system_task_usage_event, build_turn_usage_event,
};
