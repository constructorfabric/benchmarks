//! Test doubles and harnesses of the quota-and-billing feature: a fixed clock, a recording
//! metrics sink, a policy gateway with versioned snapshots and user limits, catalog and limit
//! builders (the DESIGN 5.10 example), `quota_usage`, `chat_turns` and `messages` seeding and
//! raw inspection that do not go through the code under test, the service rig over in-memory
//! `SQLite` with the shared outbox pipeline, the scripted policy plugin behind the real
//! gateway and the in-process REST rig of the quota status endpoint.
//!
//! If the author stage changes the constructors of `QuotaService` or `Services`,
//! [`QuotaRig::make_service`] is the only place to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::PolicyEnforcer;
use axum::Router;
use axum::middleware::from_fn;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry,
    ModelPolicyPluginError, ModelTier, PolicySnapshot, PublishError, TierLimits, UsageEvent,
    UsageTokens, UserLicenseStatus, UserLimits,
};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use serde_json::{Value, json};
use time::{Date, Month, OffsetDateTime, Time};
use toolkit::api::{OpenApiRegistryImpl, canonical_error_middleware};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_db::outbox::{OutboxHandle, Wake};
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

use super::chats::{RecordingOpenApi, a1, seed_chat};
use super::{Pdp, RecordingPdp, TENANT_A, TestDb, USER_A1, entry, raw_exec, snapshot, sqlite_db};
use crate::api::rest::register_routes;
use crate::config::OutboxConfig;
use crate::domain::authz::PepService;
use crate::domain::error::{DomainError, EnqueueSite};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::quota::{
    Clock, FinalizeOutcome, FinalizeRequest, PeriodType, PreflightDecision, PreflightRequest,
    QuotaDeps, QuotaMetrics, QuotaService, QuotaSettings, RequestFacts, ReserveReceipt,
    ReserveTurn, TurnIdentity, UsageQueue,
};
use crate::domain::services::Services;
use crate::infra::outbox::{OutboxEnqueuer, queue_specs, start_pipeline};
pub use crate::infra::outbox::{USAGE_PAYLOAD_TYPE, partition_for, validate_dedupe_key};
use crate::infra::policy_gateway::GtsModelPolicyGateway;
use crate::infra::storage::entity::{chat_turn, message, quota_usage};
use crate::infra::storage::migrations::all_migrations;
use crate::infra::storage::quota_ledger::SqlQuotaLedger;
pub use crate::infra::storage::repo_impl::ChatRecord;

// ── time ─────────────────────────────────────────────────────────────────────

/// A UTC instant.
pub fn utc(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
    utc_ms(year, month, day, hour, minute, second, 0)
}

/// A UTC instant with milliseconds.
pub fn utc_ms(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
) -> OffsetDateTime {
    Date::from_calendar_date(year, Month::try_from(month).unwrap(), day)
        .unwrap()
        .with_time(Time::from_hms_milli(hour, minute, second, millisecond).unwrap())
        .assume_utc()
}

/// The default clock instant of the rigs: `2026-02-28T15:30:00Z` (the DESIGN period example).
pub fn default_now() -> OffsetDateTime {
    utc(2026, 2, 28, 15, 30, 0)
}

/// A calendar date.
pub fn date(year: i32, month: u8, day: u8) -> Date {
    Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap()
}

/// A clock that returns a settable instant and counts its reads.
pub struct FixedClock {
    now: Mutex<OffsetDateTime>,
    reads: AtomicUsize,
}

impl FixedClock {
    pub fn at(instant: OffsetDateTime) -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(instant),
            reads: AtomicUsize::new(0),
        })
    }

    pub fn set(&self, instant: OffsetDateTime) {
        *self.now.lock().unwrap() = instant;
    }

    /// How often the clock was read.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        self.reads.fetch_add(1, Ordering::SeqCst);
        *self.now.lock().unwrap()
    }
}

/// A clock that advances by one second on every read (a second read would give another
/// instant).
pub struct TickingClock {
    next: Mutex<OffsetDateTime>,
}

impl TickingClock {
    pub fn from(instant: OffsetDateTime) -> Arc<Self> {
        Arc::new(Self {
            next: Mutex::new(instant),
        })
    }
}

impl Clock for TickingClock {
    fn now(&self) -> OffsetDateTime {
        let mut next = self.next.lock().unwrap();
        let current = *next;
        *next = current + time::Duration::seconds(1);
        current
    }
}

// ── metrics sink ─────────────────────────────────────────────────────────────

/// One recorded call of the [`QuotaMetrics`] port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricCall {
    Preflight {
        decision: String,
        model: String,
        tier: String,
    },
    EstimatedTokens(i64),
    Reserve(PeriodType),
    Commit(PeriodType),
    Overshoot(PeriodType),
    ActualTokens(i64),
    CodeInterpreterCalls {
        model: String,
        calls: u64,
    },
}

/// A [`QuotaMetrics`] sink that records every call in order.
#[derive(Default)]
pub struct RecordingMetrics {
    calls: Mutex<Vec<MetricCall>>,
}

impl RecordingMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn calls(&self) -> Vec<MetricCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    fn push(&self, call: MetricCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl QuotaMetrics for RecordingMetrics {
    fn preflight(&self, decision: &str, model: &str, tier: &str) {
        self.push(MetricCall::Preflight {
            decision: decision.to_owned(),
            model: model.to_owned(),
            tier: tier.to_owned(),
        });
    }

    fn estimated_tokens(&self, reserve_tokens: i64) {
        self.push(MetricCall::EstimatedTokens(reserve_tokens));
    }

    fn reserve(&self, period: PeriodType) {
        self.push(MetricCall::Reserve(period));
    }

    fn commit(&self, period: PeriodType) {
        self.push(MetricCall::Commit(period));
    }

    fn overshoot(&self, period: PeriodType) {
        self.push(MetricCall::Overshoot(period));
    }

    fn actual_tokens(&self, tokens: i64) {
        self.push(MetricCall::ActualTokens(tokens));
    }

    fn code_interpreter_calls(&self, model: &str, calls: u64) {
        self.push(MetricCall::CodeInterpreterCalls {
            model: model.to_owned(),
            calls,
        });
    }
}

// ── catalog and limits (the DESIGN 5.10 example) ─────────────────────────────

pub const PREMIUM: &str = "premium-1";
pub const STANDARD: &str = "standard-1";

/// The estimation budgets of the example entries: no overhead and no margin, so a message of
/// 4000 bytes estimates exactly 1000 tokens.
pub fn plain_budgets() -> Value {
    json!({
        "bytes_per_token_conservative": 4,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 0,
        "image_token_budget": 1000,
        "tool_surcharge_tokens": 500,
        "web_search_surcharge_tokens": 500,
        "code_interpreter_surcharge_tokens": 1000
    })
}

/// A catalog entry of the example: the given multiplier on both sides, `max_output_tokens` 500,
/// the plain budgets and no input limit.
pub fn model(id: &str, tier: ModelTier, multiplier: u64) -> ModelCatalogEntry {
    entry(
        id,
        tier,
        true,
        &json!({
            "input_tokens_credit_multiplier_micro": multiplier,
            "output_tokens_credit_multiplier_micro": multiplier,
            "max_output_tokens": 500,
            "max_input_tokens": 0,
            "estimation_budgets": plain_budgets(),
        }),
    )
}

/// The premium model of the example: 2.5 credits per 1K tokens, accepts images.
pub fn premium_model() -> ModelCatalogEntry {
    let mut premium = model(PREMIUM, ModelTier::Premium, 2_500_000_000);
    premium.multimodal_capabilities = vec!["VISION_INPUT".to_owned()];
    premium
}

/// The standard model of the example: 1.0 credit per 1K tokens, no image input.
pub fn standard_model() -> ModelCatalogEntry {
    model(STANDARD, ModelTier::Standard, 1_000_000_000)
}

/// The example catalog: premium then standard, no kill switch, policy version 1.
pub fn design_catalog() -> PolicySnapshot {
    snapshot(vec![premium_model(), standard_model()])
}

/// A snapshot with the given entries and policy version.
pub fn catalog_v(version: u64, entries: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    let mut snap = snapshot(entries);
    snap.policy_version = version;
    snap
}

/// The user limits of the example (`standard` is the `total` bucket, `premium` the
/// `tier:premium` bucket): daily and monthly of each.
pub fn limits(
    user_id: Uuid,
    version: u64,
    standard: (u64, u64),
    premium: (u64, u64),
) -> UserLimits {
    UserLimits {
        user_id,
        policy_version: version,
        standard: TierLimits {
            limit_daily_credits_micro: standard.0,
            limit_monthly_credits_micro: standard.1,
        },
        premium: TierLimits {
            limit_daily_credits_micro: premium.0,
            limit_monthly_credits_micro: premium.1,
        },
    }
}

/// The DESIGN 5.10 limits: premium 22M daily and 300M monthly, total 60M daily and 600M monthly.
pub fn design_limits(user_id: Uuid) -> UserLimits {
    limits(
        user_id,
        1,
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    )
}

/// The request of the example: 4000 message bytes (1000 estimated tokens), no images, no
/// attachments, no web search.
pub fn plain_facts() -> RequestFacts {
    RequestFacts {
        message_bytes: 4000,
        image_count: 0,
        ready_documents: 0,
        ready_xlsx_attachments: 0,
        web_search_enabled: false,
    }
}

/// The settings of the tests: the defaults of the configuration.
pub fn default_settings() -> QuotaSettings {
    QuotaSettings {
        overshoot_tolerance_factor: 1.10,
        warning_threshold_pct: 80,
        web_search_daily_quota: 75,
        code_interpreter_daily_quota: 50,
        streaming_max_output_tokens: 32_768,
        minimal_generation_floor: 50,
    }
}

// ── policy gateway with versions and limits ──────────────────────────────────

/// A [`ModelPolicyGateway`] with versioned snapshots and per-user limits that counts and
/// records every call and can fail each kind of call.
pub struct QuotaGateway {
    snapshots: Mutex<BTreeMap<u64, PolicySnapshot>>,
    current: Mutex<u64>,
    standard: Mutex<(u64, u64)>,
    premium: Mutex<(u64, u64)>,
    pub current_calls: Mutex<Vec<Uuid>>,
    pub version_calls: Mutex<Vec<(Uuid, u64)>>,
    pub limits_calls: Mutex<Vec<(Uuid, u64)>>,
    pub fail_current: AtomicBool,
    pub fail_version: AtomicBool,
    pub fail_limits: AtomicBool,
}

impl QuotaGateway {
    /// Serve `snapshot` as the current version with the given limits (`standard`, `premium`).
    pub fn serving(
        snapshot: PolicySnapshot,
        standard: (u64, u64),
        premium: (u64, u64),
    ) -> Arc<Self> {
        let version = snapshot.policy_version;
        Arc::new(Self {
            snapshots: Mutex::new(BTreeMap::from([(version, snapshot)])),
            current: Mutex::new(version),
            standard: Mutex::new(standard),
            premium: Mutex::new(premium),
            current_calls: Mutex::new(Vec::new()),
            version_calls: Mutex::new(Vec::new()),
            limits_calls: Mutex::new(Vec::new()),
            fail_current: AtomicBool::new(false),
            fail_version: AtomicBool::new(false),
            fail_limits: AtomicBool::new(false),
        })
    }

    /// The example catalog with the DESIGN 5.10 limits.
    pub fn design() -> Arc<Self> {
        Self::serving(
            design_catalog(),
            (60_000_000, 600_000_000),
            (22_000_000, 300_000_000),
        )
    }

    /// Serve one more version (not current).
    pub fn add_version(&self, snapshot: PolicySnapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .insert(snapshot.policy_version, snapshot);
    }

    /// Make `version` the current one.
    pub fn set_current(&self, version: u64) {
        *self.current.lock().unwrap() = version;
    }

    pub fn set_limits(&self, standard: (u64, u64), premium: (u64, u64)) {
        *self.standard.lock().unwrap() = standard;
        *self.premium.lock().unwrap() = premium;
    }

    pub fn current_call_count(&self) -> usize {
        self.current_calls.lock().unwrap().len()
    }

    pub fn version_call_count(&self) -> usize {
        self.version_calls.lock().unwrap().len()
    }

    pub fn limits_call_count(&self) -> usize {
        self.limits_calls.lock().unwrap().len()
    }

    /// Every call of every kind.
    pub fn total_calls(&self) -> usize {
        self.current_call_count() + self.version_call_count() + self.limits_call_count()
    }

    fn unavailable(what: &str) -> DomainError {
        DomainError::PolicyUnavailable {
            cause: format!("quota gateway `{what}` failed with provider-secret-detail"),
        }
    }
}

#[async_trait]
impl ModelPolicyGateway for QuotaGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        self.current_calls.lock().unwrap().push(user_id);
        if self.fail_current.load(Ordering::SeqCst) {
            return Err(Self::unavailable("current_snapshot"));
        }
        let version = *self.current.lock().unwrap();
        self.snapshots
            .lock()
            .unwrap()
            .get(&version)
            .cloned()
            .ok_or_else(|| Self::unavailable("current_snapshot"))
    }

    async fn snapshot_of_version(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.version_calls
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        if self.fail_version.load(Ordering::SeqCst) {
            return Err(Self::unavailable("snapshot_of_version"));
        }
        self.snapshots
            .lock()
            .unwrap()
            .get(&policy_version)
            .cloned()
            .ok_or_else(|| Self::unavailable("snapshot_of_version"))
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        self.limits_calls
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        if self.fail_limits.load(Ordering::SeqCst) {
            return Err(Self::unavailable("user_limits"));
        }
        Ok(limits(
            user_id,
            policy_version,
            *self.standard.lock().unwrap(),
            *self.premium.lock().unwrap(),
        ))
    }
}

// ── usage queue doubles ──────────────────────────────────────────────────────

/// A usage queue whose enqueue always fails.
pub struct FailingUsageQueue {
    pub calls: AtomicUsize,
    oversized: bool,
}

impl FailingUsageQueue {
    pub fn internal() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            oversized: false,
        })
    }

    pub fn payload_too_large() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            oversized: true,
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl UsageQueue for FailingUsageQueue {
    async fn enqueue_usage(
        &self,
        _tx: &DbTx<'_>,
        _event: &UsageEvent,
    ) -> Result<Wake, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.oversized {
            Err(DomainError::OutboxPayloadTooLarge {
                site: EnqueueSite::TurnFinalize,
            })
        } else {
            Err(DomainError::internal(
                "forced usage enqueue failure with secret-queue-detail",
            ))
        }
    }
}

// ── seeding and inspection of quota_usage ────────────────────────────────────

const TS: &str = "2026-01-01 00:00:00+00:00";

/// The SQL blob literal of a UUID.
pub fn blob(id: Uuid) -> String {
    format!("x'{}'", id.simple())
}

/// A `quota_usage` row to seed.
#[derive(Debug, Clone)]
pub struct SeedRow {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period: &'static str,
    pub start: String,
    pub bucket: &'static str,
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

/// A row of user 1 of tenant A, all counters 0.
pub fn row(period: &'static str, start: &str, bucket: &'static str) -> SeedRow {
    SeedRow {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        period,
        start: start.to_owned(),
        bucket,
        spent: 0,
        reserved: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
    }
}

impl SeedRow {
    pub fn spent(mut self, credits: i64) -> Self {
        self.spent = credits;
        self
    }

    pub fn reserved(mut self, credits: i64) -> Self {
        self.reserved = credits;
        self
    }

    pub fn web(mut self, calls: i32) -> Self {
        self.web_search_calls = calls;
        self
    }

    pub fn code(mut self, calls: i32) -> Self {
        self.code_interpreter_calls = calls;
        self
    }

    pub fn of(mut self, tenant_id: Uuid, user_id: Uuid) -> Self {
        self.tenant_id = tenant_id;
        self.user_id = user_id;
        self
    }
}

/// Insert the rows.
pub async fn seed_rows(db: &TestDb, rows: &[SeedRow]) {
    for seed in rows {
        raw_exec(
            &db.raw,
            &format!(
                "INSERT INTO quota_usage (id, tenant_id, user_id, period_type, period_start, \
                 bucket, spent_credits_micro, reserved_credits_micro, web_search_calls, \
                 code_interpreter_calls, updated_at) \
                 VALUES ({}, {}, {}, '{}', '{}', '{}', {}, {}, {}, {}, '{TS}')",
                blob(Uuid::new_v4()),
                blob(seed.tenant_id),
                blob(seed.user_id),
                seed.period,
                seed.start,
                seed.bucket,
                seed.spent,
                seed.reserved,
                seed.web_search_calls,
                seed.code_interpreter_calls,
            ),
        )
        .await
        .expect("seed quota row");
    }
}

/// Every counter of one `quota_usage` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRowView {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period: String,
    pub start: String,
    pub bucket: String,
    pub spent: i64,
    pub reserved: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub file_search_calls: i32,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
    pub rag_retrieval_calls: i32,
    pub image_inputs: i32,
    pub image_upload_bytes: i64,
}

impl From<quota_usage::Model> for QuotaRowView {
    fn from(row: quota_usage::Model) -> Self {
        Self {
            tenant_id: row.tenant_id,
            user_id: row.user_id,
            period: row.period_type,
            start: row.period_start.to_string(),
            bucket: row.bucket,
            spent: row.spent_credits_micro,
            reserved: row.reserved_credits_micro,
            calls: row.calls,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            file_search_calls: row.file_search_calls,
            web_search_calls: row.web_search_calls,
            code_interpreter_calls: row.code_interpreter_calls,
            rag_retrieval_calls: row.rag_retrieval_calls,
            image_inputs: row.image_inputs,
            image_upload_bytes: row.image_upload_bytes,
        }
    }
}

/// Every `quota_usage` row of the database, ordered by user, period, start and bucket.
pub async fn all_quota_rows(db: &TestDb) -> Vec<QuotaRowView> {
    let conn = db.provider.conn().unwrap();
    let mut rows: Vec<QuotaRowView> = quota_usage::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
        .into_iter()
        .map(QuotaRowView::from)
        .collect();
    rows.sort_by(|a, b| {
        (a.tenant_id, a.user_id, &a.period, &a.start, &a.bucket).cmp(&(
            b.tenant_id,
            b.user_id,
            &b.period,
            &b.start,
            &b.bucket,
        ))
    });
    rows
}

/// The rows of one user of tenant A.
pub async fn quota_rows_of(db: &TestDb, user_id: Uuid) -> Vec<QuotaRowView> {
    all_quota_rows(db)
        .await
        .into_iter()
        .filter(|row| row.user_id == user_id && row.tenant_id == TENANT_A)
        .collect()
}

/// The row of user 1 of tenant A for `(period, start, bucket)`, or `None`.
pub async fn quota_row(
    db: &TestDb,
    period: &str,
    start: &str,
    bucket: &str,
) -> Option<QuotaRowView> {
    quota_rows_of(db, USER_A1)
        .await
        .into_iter()
        .find(|row| row.period == period && row.start == start && row.bucket == bucket)
}

/// The `reserved_credits_micro` of a row, 0 when absent.
pub async fn reserved_of(db: &TestDb, period: &str, start: &str, bucket: &str) -> i64 {
    quota_row(db, period, start, bucket)
        .await
        .map_or(0, |row| row.reserved)
}

/// The `spent_credits_micro` of a row, 0 when absent.
pub async fn spent_of(db: &TestDb, period: &str, start: &str, bucket: &str) -> i64 {
    quota_row(db, period, start, bucket)
        .await
        .map_or(0, |row| row.spent)
}

// ── seeding and inspection of chat_turns and messages ────────────────────────

/// The preflight columns of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveCols {
    pub policy_version: i64,
    pub effective_model: String,
    pub reserve_tokens: i64,
    pub max_output_tokens: i64,
    pub reserved_credits_micro: i64,
    pub floor: i64,
}

/// The preflight columns the pipeline writes from a decision.
pub fn reserve_cols_of(decision: &PreflightDecision) -> ReserveCols {
    ReserveCols {
        policy_version: i64::try_from(decision.policy_version_applied).unwrap(),
        effective_model: decision.effective_model.clone(),
        reserve_tokens: decision.reserve_tokens,
        max_output_tokens: decision.max_output_tokens_applied,
        reserved_credits_micro: decision.reserved_credits_micro,
        floor: decision.minimal_generation_floor_applied,
    }
}

/// A `chat_turns` row to insert.
#[derive(Debug, Clone)]
pub struct TurnSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub user_id: Option<Uuid>,
    pub state: &'static str,
    pub reserve: Option<ReserveCols>,
    pub started_at: OffsetDateTime,
    pub last_progress_at: Option<OffsetDateTime>,
    pub deleted_at: Option<OffsetDateTime>,
}

impl TurnSeed {
    /// A `running` turn of `chat` owned by its user, without reserve columns.
    pub fn running(chat: &ChatRecord, started_at: OffsetDateTime) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id: Uuid::new_v4(),
            user_id: Some(chat.user_id),
            state: "running",
            reserve: None,
            started_at,
            last_progress_at: None,
            deleted_at: None,
        }
    }

    pub fn reserved(mut self, cols: ReserveCols) -> Self {
        self.reserve = Some(cols);
        self
    }

    pub fn without_user(mut self) -> Self {
        self.user_id = None;
        self
    }
}

/// Insert a turn through the Secure ORM, inside a transaction or on a connection.
pub async fn insert_turn(runner: &impl DBRunner, seed: &TurnSeed) {
    let cols = seed.reserve.as_ref();
    let row = chat_turn::ActiveModel {
        id: Set(seed.id),
        tenant_id: Set(seed.tenant_id),
        chat_id: Set(seed.chat_id),
        request_id: Set(seed.request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(seed.user_id),
        state: Set(seed.state.to_owned()),
        provider_name: Set(None),
        provider_response_id: Set(None),
        assistant_message_id: Set(None),
        error_code: Set(None),
        reserve_tokens: Set(cols.map(|c| c.reserve_tokens)),
        max_output_tokens_applied: Set(cols.map(|c| i32::try_from(c.max_output_tokens).unwrap())),
        reserved_credits_micro: Set(cols.map(|c| c.reserved_credits_micro)),
        policy_version_applied: Set(cols.map(|c| c.policy_version)),
        effective_model: Set(cols.map(|c| c.effective_model.clone())),
        minimal_generation_floor_applied: Set(cols.map(|c| i32::try_from(c.floor).unwrap())),
        error_detail: Set(None),
        deleted_at: Set(seed.deleted_at),
        replaced_by_request_id: Set(None),
        started_at: Set(seed.started_at),
        last_progress_at: Set(seed.last_progress_at),
        web_search_enabled: Set(false),
        web_search_completed_count: Set(0),
        code_interpreter_completed_count: Set(0),
        file_search_completed_count: Set(0),
        completed_at: Set(None),
        updated_at: Set(seed.started_at),
    };
    secure_insert::<chat_turn::Entity>(row, &AccessScope::for_tenant(seed.tenant_id), runner)
        .await
        .expect("insert turn");
}

/// Seed a turn on a connection.
pub async fn seed_turn(db: &TestDb, seed: &TurnSeed) -> Uuid {
    insert_turn(&db.provider.conn().unwrap(), seed).await;
    seed.id
}

/// The turn row, whatever its state.
pub async fn turn_row(db: &TestDb, turn_id: Uuid) -> chat_turn::Model {
    let conn = db.provider.conn().unwrap();
    chat_turn::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
        .one(&conn)
        .await
        .unwrap()
        .expect("turn row")
}

/// Set `last_progress_at` and `deleted_at` of a turn through the Secure ORM (the same time
/// storage as every other write).
pub async fn touch_turn(
    db: &TestDb,
    turn_id: Uuid,
    last_progress_at: Option<OffsetDateTime>,
    deleted_at: Option<OffsetDateTime>,
) {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;
    let conn = db.provider.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::LastProgressAt,
            Expr::value(last_progress_at),
        )
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(deleted_at))
        .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
}

/// Seed an assistant (or user) message with token counts and an explicit `created_at`.
pub async fn seed_message_with_tokens(
    db: &TestDb,
    chat: &ChatRecord,
    role: &str,
    created_at: OffsetDateTime,
    input_tokens: i64,
    output_tokens: i64,
    deleted: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    let conn = db.provider.conn().unwrap();
    let row = message::ActiveModel {
        id: Set(id),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(None),
        role: Set(role.to_owned()),
        content: Set("text".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(input_tokens),
        output_tokens: Set(output_tokens),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(created_at),
        deleted_at: Set(deleted.then_some(created_at)),
    };
    secure_insert::<message::Entity>(row, &AccessScope::for_tenant(chat.tenant_id), &conn)
        .await
        .expect("seed message");
    id
}

/// Every message of the usage queue of the shared outbox, as the typed events.
pub async fn usage_events(db: &TestDb, queue: &str) -> Vec<UsageEvent> {
    super::chats::outbox_rows(db)
        .await
        .into_iter()
        .filter(|row| row.queue == queue)
        .map(|row| serde_json::from_value(row.payload).expect("usage event payload"))
        .collect()
}

/// Every message of the usage queue with its partition and payload type.
pub async fn usage_rows(db: &TestDb, queue: &str) -> Vec<super::chats::OutboxRow> {
    super::chats::outbox_rows(db)
        .await
        .into_iter()
        .filter(|row| row.queue == queue)
        .collect()
}

/// A raw outbox payload as JSON (for the field-presence assertions).
pub async fn usage_payloads(db: &TestDb, queue: &str) -> Vec<Value> {
    usage_rows(db, queue)
        .await
        .into_iter()
        .map(|row| row.payload)
        .collect()
}

// ── the service rig ──────────────────────────────────────────────────────────

/// What the usage queue of the rig is.
pub enum UsageQueueKind {
    /// The real transactional enqueuer over a running outbox pipeline.
    Real,
    /// A double.
    Custom(Arc<dyn UsageQueue>),
}

/// How to build a [`QuotaRig`].
pub struct QuotaRigOptions {
    pub pdp: Pdp,
    pub gateway: Arc<QuotaGateway>,
    pub queue: UsageQueueKind,
    pub settings: QuotaSettings,
    pub clock: Arc<dyn Clock>,
    pub outbox: OutboxConfig,
    /// A metrics sink other than the recording one (for example the real OTLP instruments).
    pub metrics: Option<Arc<dyn QuotaMetrics>>,
}

impl QuotaRigOptions {
    /// The DESIGN example gateway, the real queue, default settings, the default instant.
    pub fn new() -> Self {
        Self {
            pdp: Pdp::TenantOnly,
            gateway: QuotaGateway::design(),
            queue: UsageQueueKind::Real,
            settings: default_settings(),
            clock: FixedClock::at(default_now()),
            outbox: OutboxConfig::default(),
            metrics: None,
        }
    }
}

impl Default for QuotaRigOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The quota service over in-memory `SQLite` and the shared outbox pipeline.
pub struct QuotaRig {
    pub db: TestDb,
    pub service: Arc<QuotaService>,
    pub gateway: Arc<QuotaGateway>,
    pub clock: Arc<dyn Clock>,
    pub metrics: Arc<RecordingMetrics>,
    sink: Arc<dyn QuotaMetrics>,
    pub pdp: Arc<RecordingPdp>,
    pub pep: Arc<PepService>,
    pub queue: Arc<dyn UsageQueue>,
    pub outbox: OutboxConfig,
    pub settings: QuotaSettings,
    /// The transactional enqueuer over the outbox of the rig (bound to the running pipeline
    /// unless the usage queue is a custom double).
    pub enqueuer: Arc<OutboxEnqueuer>,
    _pipeline: Option<OutboxHandle>,
}

/// A rig with the DESIGN example gateway and the real queue.
pub async fn quota_rig() -> QuotaRig {
    quota_rig_with(QuotaRigOptions::new()).await
}

/// A rig built from `options`.
pub async fn quota_rig_with(options: QuotaRigOptions) -> QuotaRig {
    let db = sqlite_db(all_migrations()).await;
    let specs = queue_specs(&options.outbox, 300);
    let enqueuer = Arc::new(OutboxEnqueuer::new(specs.clone()));
    let (queue, pipeline): (Arc<dyn UsageQueue>, Option<OutboxHandle>) = match options.queue {
        UsageQueueKind::Real => {
            let handle = start_pipeline(db.db.clone(), &specs)
                .await
                .expect("pipeline starts");
            enqueuer.bind(handle.outbox());
            (enqueuer.clone(), Some(handle))
        }
        UsageQueueKind::Custom(queue) => (queue, None),
    };
    let pdp = RecordingPdp::new(options.pdp);
    let pep = Arc::new(PepService::new(PolicyEnforcer::new(pdp.clone())));
    let metrics = RecordingMetrics::new();
    let sink: Arc<dyn QuotaMetrics> = options.metrics.unwrap_or_else(|| metrics.clone());
    let service = Arc::new(QuotaService::new(QuotaDeps {
        pep: Arc::clone(&pep),
        gateway: options.gateway.clone(),
        db: Arc::clone(&db.provider),
        ledger: Arc::new(SqlQuotaLedger),
        queue: Arc::clone(&queue),
        metrics: Arc::clone(&sink),
        clock: Arc::clone(&options.clock),
        settings: options.settings,
    }));
    QuotaRig {
        db,
        service,
        gateway: options.gateway,
        clock: options.clock,
        metrics,
        sink,
        pdp,
        pep,
        queue,
        outbox: options.outbox,
        settings: options.settings,
        enqueuer,
        _pipeline: pipeline,
    }
}

impl QuotaRig {
    /// A fresh service over the collaborators of the rig (the rig's metrics sink included).
    pub fn make_service(&self) -> QuotaService {
        QuotaService::new(QuotaDeps {
            pep: Arc::clone(&self.pep),
            gateway: self.gateway.clone(),
            db: Arc::clone(&self.db.provider),
            ledger: Arc::new(SqlQuotaLedger),
            queue: Arc::clone(&self.queue),
            metrics: Arc::clone(&self.sink),
            clock: Arc::clone(&self.clock),
            settings: self.settings,
        })
    }

    /// The name of the usage queue.
    pub fn usage_queue(&self) -> String {
        self.outbox.queue_name.clone()
    }

    /// The usage events enqueued so far.
    pub async fn events(&self) -> Vec<UsageEvent> {
        usage_events(&self.db, &self.usage_queue()).await
    }

    /// A live chat of `who` on `model`.
    pub async fn chat(&self, who: &SecurityContext, model: &str) -> ChatRecord {
        seed_chat(
            &self.db,
            who,
            Uuid::new_v4(),
            Some("quota"),
            model,
            super::chats::at(0),
        )
        .await
    }

    /// A live chat of user 1 of tenant A.
    pub async fn chat_a1(&self, model: &str) -> ChatRecord {
        self.chat(&a1(), model).await
    }

    /// The preflight of `model` for a turn of `chat` with `facts`, as `who`.
    pub async fn preflight_as(
        &self,
        who: &SecurityContext,
        chat: &ChatRecord,
        model: &str,
        facts: RequestFacts,
    ) -> Result<PreflightDecision, DomainError> {
        self.service
            .preflight(
                who,
                &PreflightRequest {
                    chat_id: chat.id,
                    selected_model: model.to_owned(),
                    facts,
                },
            )
            .await
    }

    /// The preflight as user 1 of tenant A.
    pub async fn preflight(
        &self,
        chat: &ChatRecord,
        model: &str,
        facts: RequestFacts,
    ) -> Result<PreflightDecision, DomainError> {
        self.preflight_as(&a1(), chat, model, facts).await
    }

    /// The identity of a new turn of `chat`.
    pub fn identity(chat: &ChatRecord) -> TurnIdentity {
        TurnIdentity {
            tenant_id: chat.tenant_id,
            user_id: chat.user_id,
            chat_id: chat.id,
            request_id: Uuid::new_v4(),
        }
    }

    /// The send path: one transaction that writes the reserve and then inserts the `running`
    /// turn with the preflight columns of the decision and `started_at` = the preflight
    /// instant. Returns the receipt and the turn id; an error rolls back both.
    pub async fn reserve_new(
        &self,
        identity: &TurnIdentity,
        decision: &PreflightDecision,
    ) -> Result<(ReserveReceipt, Uuid), DomainError> {
        let service = Arc::clone(&self.service);
        let identity = *identity;
        let decision = decision.clone();
        let seed = TurnSeed {
            id: Uuid::new_v4(),
            tenant_id: identity.tenant_id,
            chat_id: identity.chat_id,
            request_id: identity.request_id,
            user_id: Some(identity.user_id),
            state: "running",
            reserve: Some(reserve_cols_of(&decision)),
            started_at: decision.started_at,
            last_progress_at: None,
            deleted_at: None,
        };
        let turn_id = seed.id;
        let receipt = self
            .db
            .provider
            .transaction(move |tx| {
                Box::pin(async move {
                    let receipt = service
                        .write_reserve(tx, &identity, &decision, &ReserveTurn::New)
                        .await?;
                    insert_turn(tx, &seed).await;
                    Ok(receipt)
                })
            })
            .await?;
        Ok((receipt, turn_id))
    }

    /// Retry and edit: one transaction that fills the NULL preflight columns of `turn_id`.
    pub async fn reserve_existing(
        &self,
        identity: &TurnIdentity,
        decision: &PreflightDecision,
        turn_id: Uuid,
    ) -> Result<ReserveReceipt, DomainError> {
        let service = Arc::clone(&self.service);
        let identity = *identity;
        let decision = decision.clone();
        self.db
            .provider
            .transaction(move |tx| {
                Box::pin(async move {
                    service
                        .write_reserve(tx, &identity, &decision, &ReserveTurn::Existing { turn_id })
                        .await
                })
            })
            .await
    }

    /// Preflight and reserve (send path) as user 1 of tenant A: the booked turn.
    pub async fn book(&self, chat: &ChatRecord, model: &str, facts: RequestFacts) -> BookedTurn {
        let decision = self.preflight(chat, model, facts).await.expect("preflight");
        let identity = Self::identity(chat);
        let (receipt, turn_id) = self
            .reserve_new(&identity, &decision)
            .await
            .expect("reserve");
        BookedTurn {
            identity,
            decision,
            receipt,
            turn_id,
        }
    }

    /// The finalization settlement in its own transaction (the caller's transaction of the
    /// pipeline); an `Err` rolls everything back. The wake of a settled turn is returned with
    /// the outcome and never fired.
    pub async fn finalize(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeOutcome, DomainError> {
        let service = Arc::clone(&self.service);
        let request = request.clone();
        self.db
            .provider
            .transaction(move |tx| {
                Box::pin(async move { service.finalize_turn(tx, &request).await })
            })
            .await
    }
}

/// A turn booked through preflight and the reserve write.
pub struct BookedTurn {
    pub identity: TurnIdentity,
    pub decision: PreflightDecision,
    pub receipt: ReserveReceipt,
    pub turn_id: Uuid,
}

/// The provider usage of the example turn (900 input and 300 output tokens).
pub fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

// ── the scripted policy plugin behind the real gateway ───────────────────────

/// How the plugin answers `publish_usage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishScript {
    Accept,
    Transient,
    Permanent,
}

/// A model-policy plugin that records the published events and answers `publish_usage` by
/// script; every other call fails.
pub struct ScriptedPolicyPlugin {
    script: Mutex<PublishScript>,
    published: Mutex<Vec<UsageEvent>>,
}

impl ScriptedPolicyPlugin {
    pub fn new(script: PublishScript) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            published: Mutex::new(Vec::new()),
        })
    }

    pub fn set_script(&self, script: PublishScript) {
        *self.script.lock().unwrap() = script;
    }

    pub fn published(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for ScriptedPolicyPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("not scripted".to_owned()))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("not scripted".to_owned()))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Err(ModelPolicyPluginError::Internal("not scripted".to_owned()))
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().unwrap().push(event);
        match *self.script.lock().unwrap() {
            PublishScript::Accept => Ok(()),
            PublishScript::Transient => Err(PublishError::Transient("backend busy".to_owned())),
            PublishScript::Permanent => Err(PublishError::Permanent("rejected".to_owned())),
        }
    }

    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        Ok(UserLicenseStatus { active: false })
    }
}

/// The real gateway over a registered scripted plugin.
pub struct PluginGateway {
    pub gateway: Arc<GtsModelPolicyGateway>,
    pub plugin: Arc<ScriptedPolicyPlugin>,
}

/// A real [`GtsModelPolicyGateway`] whose plugin is registered in types-registry and the hub.
pub fn plugin_gateway(script: PublishScript) -> PluginGateway {
    let (id, content) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        "acme.core._.usage_policy.v1",
        "constructorfabric",
        100,
    )
    .expect("plugin registration");
    let id: String = id.as_ref().to_owned();
    let registry =
        Arc::new(MockTypesRegistryClient::new().with_instances([make_test_instance(&id, content)]));
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry;
    hub.register::<dyn TypesRegistryClient>(dynamic);
    let plugin = ScriptedPolicyPlugin::new(script);
    let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = plugin.clone();
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), client);
    PluginGateway {
        gateway: Arc::new(GtsModelPolicyGateway::new(
            hub,
            "constructorfabric".to_owned(),
        )),
        plugin,
    }
}

/// A real gateway with no plugin registered (every resolution fails).
pub fn unresolvable_gateway() -> Arc<GtsModelPolicyGateway> {
    let registry = Arc::new(MockTypesRegistryClient::new());
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry;
    hub.register::<dyn TypesRegistryClient>(dynamic);
    Arc::new(GtsModelPolicyGateway::new(
        hub,
        "constructorfabric".to_owned(),
    ))
}

// ── the REST rig of the quota status endpoint ────────────────────────────────

/// The in-process REST harness of `GET /v1/quota/status` over the quota rig.
pub struct StatusRig {
    pub router: Router,
    pub openapi: Arc<RecordingOpenApi>,
    pub rig: QuotaRig,
}

/// A status rig with the DESIGN example gateway and the given PDP behavior.
pub async fn status_rig(pdp: Pdp) -> StatusRig {
    status_rig_with(QuotaRigOptions {
        pdp,
        ..QuotaRigOptions::new()
    })
    .await
}

/// A status rig built from `options`.
pub async fn status_rig_with(options: QuotaRigOptions) -> StatusRig {
    let rig = quota_rig_with(options).await;
    let gateway: Arc<dyn ModelPolicyGateway> = rig.gateway.clone();
    let services =
        Arc::new(Services::new(Arc::clone(&rig.pep), gateway).with_quota(rig.make_service()));
    let openapi = Arc::new(RecordingOpenApi {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = register_routes(Router::new(), openapi.as_ref(), "/mini-chat", services)
        .layer(from_fn(canonical_error_middleware));
    StatusRig {
        router,
        openapi,
        rig,
    }
}

impl StatusRig {
    /// `GET /mini-chat/v1/quota/status` as `who`.
    pub async fn get_status(&self, who: &SecurityContext) -> super::chats::Reply {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let response = self
            .router
            .clone()
            .oneshot(super::chats::request("GET", STATUS_URI, who))
            .await
            .unwrap();
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        let content_type = header("content-type").unwrap_or_default();
        let location = header("location");
        let retry_after = header("retry-after");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        super::chats::Reply {
            status,
            content_type,
            location,
            retry_after,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }
}

/// The URI of the quota status endpoint.
pub const STATUS_URI: &str = "/mini-chat/v1/quota/status";
