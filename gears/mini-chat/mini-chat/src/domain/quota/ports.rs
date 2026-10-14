//! Ports of the quota rules: the ledger over `quota_usage` and `chat_turns`, the usage queue,
//! the usage publisher, the metrics and the clock. The infrastructure layer implements them;
//! the domain imports nothing from it. Every ledger operation takes the caller's transaction
//! `tx`, so the quota writes join the transaction of the pipeline that owns the turn.

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use thiserror::Error;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quota::billing::TerminalState;
use crate::domain::quota::periods::{PeriodStarts, PeriodType};
use crate::domain::quota::rows::{BucketKey, QuotaRows};

/// The application UTC clock. The preflight reads it exactly once.
pub trait Clock: Send + Sync {
    /// The current UTC instant.
    fn now(&self) -> OffsetDateTime;
}

/// The cost metrics of the quota rules: low-cardinality labels only (`decision`, `model`,
/// `tier`, `period`), never a tenant, user, chat or request identifier.
pub trait QuotaMetrics: Send + Sync {
    /// `mini_chat_quota_preflight`: once per preflight, `decision` is `allow`, `downgrade` or `reject`.
    fn preflight(&self, decision: &str, model: &str, tier: &str);
    /// `mini_chat_quota_estimated_tokens` (histogram): the `reserve_tokens` of an allow or downgrade.
    fn estimated_tokens(&self, reserve_tokens: i64);
    /// `mini_chat_quota_reserve`: once per period after the reserve transaction committed.
    fn reserve(&self, period: PeriodType);
    /// `mini_chat_quota_commit`: once per period for an `actual` settlement.
    fn commit(&self, period: PeriodType);
    /// `mini_chat_quota_overshoot`: once per period for an actual settlement above the reserve.
    fn overshoot(&self, period: PeriodType);
    /// `mini_chat_quota_actual_tokens` (histogram): `actual` settlements only.
    fn actual_tokens(&self, tokens: i64);
    /// `mini_chat_code_interpreter_calls` by the number of calls: `actual` settlements only.
    fn code_interpreter_calls(&self, model: &str, calls: u64);
}

/// The usage queue (`outbox.queue_name`, partitioned by `tenant_id`).
#[async_trait]
pub trait UsageQueue: Send + Sync {
    /// Enqueue one usage message inside the caller's transaction; the returned [`Wake`] is
    /// fired after the commit.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge` for an oversized payload, `Internal` when the outbox is not
    /// running, the `dedupe_key` is invalid or the database fails.
    async fn enqueue_usage(&self, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError>;
}

/// Why `publish_usage` did not succeed.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UsagePublishError {
    /// The plugin cannot be resolved or the publish failed transiently: the handler retries.
    #[error("transient usage publish failure: {0}")]
    Transient(String),
    /// A permanent plugin error: the handler rejects (dead-letter).
    #[error("permanent usage publish failure: {0}")]
    Permanent(String),
}

/// Publishes a usage event to the model-policy plugin (`publish_usage`), outside any database
/// transaction.
#[async_trait]
pub trait UsagePublisher: Send + Sync {
    /// Resolve the plugin and publish the event unchanged.
    ///
    /// # Errors
    ///
    /// [`UsagePublishError`]: a resolution failure is transient.
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError>;
}

/// The result of the atomic increment of `reserved_credits_micro`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveIncrement {
    Applied,
    /// The increment would overflow the 64-bit counter: treated as an over-limit bucket.
    Overflow,
}

/// The six preflight columns of `chat_turns` plus `started_at` (the preflight instant).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPreflightColumns {
    pub policy_version_applied: i64,
    pub effective_model: String,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub started_at: OffsetDateTime,
}

/// The terminal-state-specific columns of the CAS update, supplied by the finalizing path.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTerminalUpdate {
    pub state: TerminalState,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub completed_at: OffsetDateTime,
}

/// A turn row as the settlement reads it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_user_id: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub reserve_tokens: Option<i64>,
    pub max_output_tokens_applied: Option<i64>,
    pub reserved_credits_micro: Option<i64>,
    pub policy_version_applied: Option<i64>,
    pub effective_model: Option<String>,
    pub minimal_generation_floor_applied: Option<i64>,
}

/// The change one settlement applies to one bucket row: `reserved_credits_micro -=
/// release_reserved_credits_micro`, `spent_credits_micro += spent_credits_micro`, `calls += 1`
/// and the telemetry counters added. The counters that do not apply are 0.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleDelta {
    pub release_reserved_credits_micro: i64,
    pub spent_credits_micro: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// The database operations of the quota rules, each inside the caller's transaction.
#[async_trait]
pub trait QuotaLedger: Send + Sync {
    /// The user's rows of the current periods for both buckets (`total` and `tier:premium`),
    /// read through `scope` (tenant predicate and owner predicate on `user_id`); a missing row
    /// is absent from the result.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn read_rows(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        periods: &PeriodStarts,
    ) -> Result<QuotaRows, DomainError>;

    /// `prior_context_tokens`: `input_tokens + output_tokens` of the latest live assistant
    /// message of the chat with non-zero tokens, ordered by `(created_at, id)` descending with
    /// a chronological sort key, or 0 when there is none.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn prior_context_tokens(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<i64, DomainError>;

    /// Atomically add `credits` to `reserved_credits_micro` of the row, creating it when
    /// absent; a counter overflow is [`ReserveIncrement::Overflow`], never a wrap or a 500.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn add_reserved(
        &self,
        tx: &DbTx<'_>,
        key: &BucketKey,
        credits: i64,
    ) -> Result<ReserveIncrement, DomainError>;

    /// Fill the NULL preflight columns of a `running` turn and set its `started_at`; returns
    /// whether a row was filled (`false`: the turn does not exist, is not `running` or already
    /// holds a reserve).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn fill_preflight_columns(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
        columns: &TurnPreflightColumns,
    ) -> Result<bool, DomainError>;

    /// The CAS: one conditional UPDATE to the terminal state guarded by `state = 'running'`
    /// (and, for the orphan watchdog, `deleted_at IS NULL` and `COALESCE(last_progress_at,
    /// started_at) <= stale_cutoff`); returns whether this call won (`rows_affected = 1`).
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn cas_terminal(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
        update: &TurnTerminalUpdate,
        stale_cutoff: Option<OffsetDateTime>,
    ) -> Result<bool, DomainError>;

    /// The turn row of the tenant.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn load_turn(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError>;

    /// Apply a settlement delta to one bucket row in one atomic statement.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the database fails.
    async fn settle_bucket(
        &self,
        tx: &DbTx<'_>,
        key: &BucketKey,
        delta: &SettleDelta,
    ) -> Result<(), DomainError>;
}
