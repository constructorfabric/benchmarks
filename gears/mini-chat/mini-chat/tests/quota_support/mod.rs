//! S4 (quota and billing) harness on top of the S1 fakes, the S2 chat harness and the S3 stream
//! harness pieces.
//!
//! Kept apart from `common`, `chat_support` and `stream_support` so the S1 - S3 test binaries
//! (which never declare `mod quota_support;`) compile independently of the S4 production items.
//! Everything here relies only on the items listed in `S4-test-interface.md`. Test files declare
//! `mod common; mod chat_support; mod stream_support; mod quota_support;`.
//!
//! * [`QuotaPolicy`] is a scriptable model-policy plugin: versioned snapshots (catalog + kill
//!   switches), per-user limits and a recording `publish_usage` whose answers can be scripted.
//! * [`RecordingAudit`] is an audit plugin that records every `emit` and can be scripted to fail.
//! * [`QuotaHarness`] wires the real `StreamService` (with the usage and audit outbox adapters),
//!   `TurnsService` and `QuotaStatusService` over the S2 chat harness database, together with a
//!   second running outbox that serves the usage queue (`UsageOutboxHandler` over the model-policy
//!   gateway) and the audit queue (`AuditOutboxHandler` over the audit gateway).
//! * Seeds and readers for `quota_usage` and a reference implementation of the credit arithmetic
//!   (DESIGN 5.3) that is independent of the production code.

#![allow(
    dead_code,
    unused_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value,
    clippy::integer_division
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::{Request, header};
use mini_chat::api::rest::{register_quota_routes, register_stream_routes};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::quota_status::QuotaStatusService;
use mini_chat::domain::streaming::StreamService;
use mini_chat::domain::turns::TurnsService;
use mini_chat::infra::audit::AuditGateway;
use mini_chat::infra::db::entity::quota_usage;
use mini_chat::infra::db::entity::{chat_turns, chat_vector_stores};
use mini_chat::infra::db::timestamp::to_db;
use mini_chat::infra::llm::LlmProviders;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat::infra::outbox::{AuditOutbox, AuditOutboxHandler, UsageOutbox, UsageOutboxHandler};
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, KillSwitches, MiniChatAuditPluginClientV1,
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::prelude::Date as DbDate;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{OutboxHandle, OutboxProfile, Partitions};
use toolkit_db::secure::{
    SecureEntityExt, SecureInsertExt, SecureOnConflict, SecureUpdateExt, secure_insert,
};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

pub use crate::chat_support::{
    AttachmentSeed, ChatHarness, ChatHarnessOptions, ChatSeed, MessageSeed, parse_ts, ts,
};
pub use crate::common::{
    FakePdp, PdpMode, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem, ctx, ctx_a,
};
pub use crate::stream_support::{
    FakeOagw, LiveSse, Opts, Piece, ProviderReply, RawStream, Sent, TurnSeed, assert_conflict, ev,
    eventually, failed_stream, failed_stream_with_details, file_search_pieces, hex, merge,
    model_entries, responses_stream, stream_config, stream_model_json, web_annotation,
    web_search_pieces,
};

// ── catalog ──────────────────────────────────────────────────────────────────

pub const PREMIUM: &str = "gpt-premium";
pub const STANDARD: &str = "gpt-standard";
pub const STANDARD_2: &str = "gpt-standard-2";
pub const DISABLED: &str = "gpt-off";

/// Credit multipliers of the quota catalog (micro-credits per 1M tokens).
pub const PREMIUM_IN: u64 = 3_000_000;
pub const PREMIUM_OUT: u64 = 15_000_000;
pub const STANDARD_IN: u64 = 1_000_000;
pub const STANDARD_OUT: u64 = 3_000_000;

/// Catalog of the quota tests:
/// * `gpt-premium` - premium, default model, vision, every tool, 3 / 15 multipliers;
/// * `gpt-standard` - standard, no vision, no web search (file search only), 1 / 3 multipliers;
/// * `gpt-off` - standard, disabled.
///
/// Every entry has the estimation budgets of `stream_model_json` (4 bytes per token, overhead 100,
/// margin 10 %, image 1000, file search 500, web search 500, code interpreter 1000) and
/// `max_output_tokens` 4096.
pub fn quota_models() -> Vec<Value> {
    let mut premium = stream_model_json(PREMIUM, "Premium");
    premium["input_tokens_credit_multiplier_micro"] = json!(PREMIUM_IN);
    premium["output_tokens_credit_multiplier_micro"] = json!(PREMIUM_OUT);
    let mut standard = stream_model_json(STANDARD, "Standard");
    standard["input_tokens_credit_multiplier_micro"] = json!(STANDARD_IN);
    standard["output_tokens_credit_multiplier_micro"] = json!(STANDARD_OUT);
    standard["multimodal_capabilities"] = json!(["RAG"]);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    let mut off = stream_model_json(DISABLED, "Standard");
    off["enabled"] = json!(false);
    vec![premium, standard, off]
}

/// A second enabled standard model (`gpt-standard-2`, preference `is_default`), for the candidate
/// selection rules of the downgrade flow.
pub fn standard_two() -> Value {
    let mut model = stream_model_json(STANDARD_2, "Standard");
    model["input_tokens_credit_multiplier_micro"] = json!(2_000_000);
    model["output_tokens_credit_multiplier_micro"] = json!(6_000_000);
    model["multimodal_capabilities"] = json!(["RAG", "VISION_INPUT"]);
    model["preference"] = json!({"is_default": true, "sort_order": 5});
    model
}

pub fn find_model(models: &[Value], id: &str) -> Value {
    models
        .iter()
        .find(|m| m["id"] == json!(id))
        .unwrap_or_else(|| panic!("no model {id} in the catalog"))
        .clone()
}

pub fn mults_of(model: &Value) -> (u64, u64) {
    (
        model["input_tokens_credit_multiplier_micro"]
            .as_u64()
            .unwrap(),
        model["output_tokens_credit_multiplier_micro"]
            .as_u64()
            .unwrap(),
    )
}

// ── reference arithmetic (DESIGN 5.3, 5.4.1, 5.5.4), independent of the production code ───────

/// `ceil_div(i * in_mult, 1e6) + ceil_div(o * out_mult, 1e6)`, per-component rounding.
pub fn ref_credits(input_tokens: u64, output_tokens: u64, in_mult: u64, out_mult: u64) -> i64 {
    let part = |tokens: u64, mult: u64| -> i128 {
        (i128::from(tokens) * i128::from(mult) + 999_999) / 1_000_000
    };
    i64::try_from(part(input_tokens, in_mult) + part(output_tokens, out_mult)).unwrap()
}

/// DESIGN 5.5.4 with the budgets of `stream_model_json` (4 bytes per token, overhead 100, 10 %).
pub fn ref_estimate(text: &str) -> u64 {
    let bytes = u64::try_from(text.len()).unwrap();
    let base = bytes.div_ceil(4) + 100;
    (base * 110).div_ceil(100)
}

/// The reserve a model books for `text` (no tools, no images) under the S3 estimation budgets:
/// `(estimated_input_tokens, max_output_tokens_applied, reserve_tokens, reserved_credits_micro)`.
pub fn ref_reserve(
    model: &Value,
    streaming_max_output: u32,
    text: &str,
    prior_context: u64,
    surcharge_tokens: u64,
) -> (u64, u32, u64, i64) {
    let max_out = u32::try_from(model["max_output_tokens"].as_u64().unwrap())
        .unwrap()
        .min(streaming_max_output);
    let est_in = ref_estimate(text) + prior_context + surcharge_tokens;
    let (im, om) = mults_of(model);
    (
        est_in,
        max_out,
        est_in + u64::from(max_out),
        ref_credits(est_in, u64::from(max_out), im, om),
    )
}

// ── limits ───────────────────────────────────────────────────────────────────

/// Per-user limits the policy plugin answers (`standard` = bucket `total`, `premium` =
/// bucket `tier:premium`; micro-credits).
#[derive(Clone, Copy, Debug)]
pub struct LimitsSpec {
    pub standard_daily: u64,
    pub standard_monthly: u64,
    pub premium_daily: u64,
    pub premium_monthly: u64,
}

impl LimitsSpec {
    /// Far above anything a test spends.
    pub const fn generous() -> Self {
        Self {
            standard_daily: 1_000_000_000_000,
            standard_monthly: 10_000_000_000_000,
            premium_daily: 1_000_000_000_000,
            premium_monthly: 10_000_000_000_000,
        }
    }

    pub const fn new(sd: u64, sm: u64, pd: u64, pm: u64) -> Self {
        Self {
            standard_daily: sd,
            standard_monthly: sm,
            premium_daily: pd,
            premium_monthly: pm,
        }
    }

    pub fn to_user_limits(self, user_id: Uuid, policy_version: u64) -> UserLimits {
        serde_json::from_value(json!({
            "user_id": user_id,
            "policy_version": policy_version,
            "standard": {"limit_daily_credits_micro": self.standard_daily,
                         "limit_monthly_credits_micro": self.standard_monthly},
            "premium": {"limit_daily_credits_micro": self.premium_daily,
                        "limit_monthly_credits_micro": self.premium_monthly},
        }))
        .expect("user limits")
    }
}

// ── scriptable model-policy plugin ───────────────────────────────────────────

pub struct QuotaPolicy {
    versions: Mutex<BTreeMap<u64, PolicySnapshot>>,
    current: AtomicU64,
    limits: Mutex<LimitsSpec>,
    published: Mutex<Vec<UsageEvent>>,
    publish_script: Mutex<VecDeque<Result<(), PublishError>>>,
    /// Number of `publish_usage` calls, failed ones included.
    pub publish_calls: AtomicUsize,
    /// Every `policy_version` asked of `get_policy_snapshot`, in call order.
    pub snapshot_requests: Mutex<Vec<u64>>,
    /// Every `(user_id, policy_version)` asked of `get_user_limits`.
    pub limit_requests: Mutex<Vec<(Uuid, u64)>>,
}

fn snapshot_json(version: u64, models: &[Value], switches: &KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: version,
        model_catalog: model_entries(models),
        kill_switches: switches.clone(),
    }
}

pub fn no_switches() -> KillSwitches {
    crate::stream_support::no_kill_switches()
}

impl QuotaPolicy {
    pub fn new(models: &[Value], version: u64, limits: LimitsSpec) -> Arc<Self> {
        let mut versions = BTreeMap::new();
        versions.insert(version, snapshot_json(version, models, &no_switches()));
        Arc::new(Self {
            versions: Mutex::new(versions),
            current: AtomicU64::new(version),
            limits: Mutex::new(limits),
            published: Mutex::new(Vec::new()),
            publish_script: Mutex::new(VecDeque::new()),
            publish_calls: AtomicUsize::new(0),
            snapshot_requests: Mutex::new(Vec::new()),
            limit_requests: Mutex::new(Vec::new()),
        })
    }

    pub fn current_version(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// Replace the catalog of the current version in place.
    pub fn set_catalog(&self, models: &[Value]) {
        let version = self.current_version();
        let mut all = self.versions.lock().unwrap();
        let snapshot = all.get_mut(&version).expect("current snapshot");
        snapshot.model_catalog = model_entries(models);
    }

    /// Turn the listed kill switches on (the others off) in the current version.
    pub fn set_kill_switches(&self, patch: Value) {
        let mut all = serde_json::to_value(no_switches()).unwrap();
        merge(&mut all, &patch);
        let switches: KillSwitches = serde_json::from_value(all).expect("kill switches");
        let version = self.current_version();
        let mut versions = self.versions.lock().unwrap();
        versions
            .get_mut(&version)
            .expect("current snapshot")
            .kill_switches = switches;
    }

    /// Publish a new policy version (new immutable snapshot) and make it the current one. The
    /// older snapshots stay available by version.
    pub fn publish_version(&self, version: u64, models: &[Value]) {
        let switches = self
            .versions
            .lock()
            .unwrap()
            .get(&self.current_version())
            .expect("current snapshot")
            .kill_switches
            .clone();
        self.versions
            .lock()
            .unwrap()
            .insert(version, snapshot_json(version, models, &switches));
        self.current.store(version, Ordering::SeqCst);
    }

    pub fn set_limits(&self, limits: LimitsSpec) {
        *self.limits.lock().unwrap() = limits;
    }

    /// The next `publish_usage` calls answer these results (FIFO); `Ok` afterwards.
    pub fn script_publish(&self, results: Vec<Result<(), PublishError>>) {
        *self.publish_script.lock().unwrap() = results.into();
    }

    /// Every usage event the plugin accepted (`Ok`), in delivery order.
    pub fn published(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }

    /// Wait (15 s) until at least `n` events were accepted; returns all of them.
    pub async fn wait_published(&self, n: usize) -> Vec<UsageEvent> {
        for _ in 0..300 {
            let got = self.published();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} usage event(s) delivered to the plugin, got {:?}",
            self.published()
        );
    }

    /// Give the pipeline time, then assert nothing (more than `n`) was delivered.
    pub async fn assert_published_stays(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            self.published().len(),
            n,
            "unexpected usage events: {:?}",
            self.published()
        );
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for QuotaPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        Ok(
            serde_json::from_value(json!({"policy_version": self.current_version(),
                   "generated_at": "2026-02-27T13:00:00Z"}))
            .expect("policy version"),
        )
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.snapshot_requests.lock().unwrap().push(policy_version);
        self.versions
            .lock()
            .unwrap()
            .get(&policy_version)
            .cloned()
            .ok_or(ModelPolicyPluginError::PolicyVersionNotFound(
                policy_version,
            ))
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.limit_requests
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        Ok(self
            .limits
            .lock()
            .unwrap()
            .to_user_limits(user_id, policy_version))
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.publish_calls.fetch_add(1, Ordering::SeqCst);
        let scripted = self.publish_script.lock().unwrap().pop_front();
        if let Some(Err(error)) = scripted {
            return Err(error);
        }
        self.published.lock().unwrap().push(payload);
        Ok(())
    }
}

// ── recording audit plugin ───────────────────────────────────────────────────

#[derive(Default)]
pub struct RecordingAudit {
    events: Mutex<Vec<AuditEvent>>,
    script: Mutex<VecDeque<Result<(), AuditPluginError>>>,
    pub calls: AtomicUsize,
}

impl RecordingAudit {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().clone()
    }

    /// The next `emit` calls answer these results (FIFO); `Ok` afterwards.
    pub fn script(&self, results: Vec<Result<(), AuditPluginError>>) {
        *self.script.lock().unwrap() = results.into();
    }

    /// Wait (15 s) until at least `n` events were accepted; returns all of them.
    pub async fn wait_events(&self, n: usize) -> Vec<AuditEvent> {
        for _ in 0..300 {
            let got = self.events();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} audit event(s) delivered to the plugin, got {:?}",
            self.events()
        );
    }

    pub async fn assert_events_stay(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            self.events().len(),
            n,
            "unexpected audit events: {:?}",
            self.events()
        );
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for RecordingAudit {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let scripted = self.script.lock().unwrap().pop_front();
        if let Some(Err(error)) = scripted {
            return Err(error);
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

/// An audit gateway over a `ClientHub` that holds `plugin` for `vendor` (types-registry mock).
pub fn audit_gateway_for(
    vendor: &str,
    plugin: Option<Arc<dyn MiniChatAuditPluginClientV1>>,
) -> Arc<AuditGateway> {
    let hub = Arc::new(ClientHub::new());
    let mut registry = MockTypesRegistryClient::new();
    if let Some(plugin) = plugin {
        let (id, payload) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
            "cf.core._.quota_test_audit.v1",
            vendor,
            100,
        )
        .expect("registration payload");
        let id: &str = id.as_ref();
        registry = registry.with_instances([make_test_instance(id, payload)]);
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(id), plugin);
    }
    let dyn_registry: Arc<dyn TypesRegistryClient> = Arc::new(registry);
    hub.register::<dyn TypesRegistryClient>(dyn_registry);
    Arc::new(AuditGateway::new(hub, vendor))
}

// ── provider-call observer ───────────────────────────────────────────────────

/// What the database held at the instant a provider request arrived.
#[derive(Clone, Debug)]
pub struct ProviderCallSnapshot {
    pub turns: Vec<chat_turns::Model>,
    pub usage: Vec<quota_usage::Model>,
}

/// A `ServiceGatewayClientV1` that snapshots `chat_turns` and `quota_usage` before it hands every
/// `proxy_request` to the scripted [`FakeOagw`]: it proves what was already committed when the
/// provider was called ("reserve before execute", DESIGN 5.4 steps 3 - 4).
pub struct ObservingOagw {
    pub inner: Arc<FakeOagw>,
    db: Arc<DBProvider<DomainError>>,
    snapshots: Mutex<Vec<ProviderCallSnapshot>>,
}

impl ObservingOagw {
    pub fn new(inner: Arc<FakeOagw>, db: Arc<DBProvider<DomainError>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            db,
            snapshots: Mutex::new(Vec::new()),
        })
    }

    pub fn snapshots(&self) -> Vec<ProviderCallSnapshot> {
        self.snapshots.lock().unwrap().clone()
    }

    pub fn snapshot(&self, n: usize) -> ProviderCallSnapshot {
        self.snapshots()
            .get(n)
            .cloned()
            .unwrap_or_else(|| panic!("no provider call #{n} was observed"))
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for ObservingOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.inner.create_upstream(ctx, req).await
    }

    async fn get_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.inner.get_upstream(ctx, id).await
    }

    async fn list_upstreams(
        &self,
        ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        self.inner.list_upstreams(ctx, query).await
    }

    async fn update_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.inner.update_upstream(ctx, id, req).await
    }

    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.inner.delete_upstream(ctx, id).await
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.inner.create_route(ctx, req).await
    }

    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.inner.get_route(ctx, id).await
    }

    async fn list_routes(
        &self,
        ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        self.inner.list_routes(ctx, upstream_id, query).await
    }

    async fn update_route(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.inner.update_route(ctx, id, req).await
    }

    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.inner.delete_route(ctx, id).await
    }

    async fn resolve_proxy_target(
        &self,
        ctx: SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        self.inner
            .resolve_proxy_target(ctx, alias, method, path)
            .await
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let conn = self.db.conn().expect("db connection");
        let turns = chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns");
        let usage = quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage");
        self.snapshots
            .lock()
            .unwrap()
            .push(ProviderCallSnapshot { turns, usage });
        self.inner.proxy_request(ctx, req).await
    }
}

// ── the harness ──────────────────────────────────────────────────────────────

pub struct QuotaOptions {
    pub models: Vec<Value>,
    pub pdp: Arc<FakePdp>,
    /// Merged into the gear configuration (`quota`, `streaming`, `outbox`, ...).
    pub config: Value,
    pub limits: LimitsSpec,
    pub policy_version: u64,
    /// `false`: the usage queue is not registered under its configured name, so every usage
    /// enqueue fails (`QueueNotRegistered`) - the finalization transaction must then roll back.
    pub register_usage_queue: bool,
    /// `false`: the same for the audit queue.
    pub register_audit_queue: bool,
}

impl Default for QuotaOptions {
    fn default() -> Self {
        Self {
            models: quota_models(),
            pdp: FakePdp::allow(),
            config: json!({}),
            limits: LimitsSpec::generous(),
            policy_version: 1,
            register_usage_queue: true,
            register_audit_queue: true,
        }
    }
}

pub struct QuotaHarness {
    /// The S2 harness: database, chat routes, seeds.
    pub chat: ChatHarness,
    pub router: Router,
    /// The scripted provider (answers, recorded requests, pauses).
    pub oagw: Arc<FakeOagw>,
    /// The same provider seen through the database observer (state at each provider call).
    pub observed: Arc<ObservingOagw>,
    pub policy: Arc<QuotaPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub config: Arc<MiniChatConfig>,
    pub pdp: Arc<FakePdp>,
    _outbox: OutboxHandle,
}

impl QuotaHarness {
    pub async fn new() -> Self {
        Self::with(QuotaOptions::default()).await
    }

    pub async fn with_limits(limits: LimitsSpec) -> Self {
        Self::with(QuotaOptions {
            limits,
            ..Default::default()
        })
        .await
    }

    pub async fn with_config(config: Value) -> Self {
        Self::with(QuotaOptions {
            config,
            ..Default::default()
        })
        .await
    }

    pub async fn with(opts: QuotaOptions) -> Self {
        let chat = crate::chat_support::chat_harness_with(ChatHarnessOptions {
            models: model_entries(&opts.models),
            pdp: opts.pdp.clone(),
            ..Default::default()
        })
        .await;
        let oagw = FakeOagw::new();
        let policy = QuotaPolicy::new(&opts.models, opts.policy_version, opts.limits);
        let audit = Arc::new(RecordingAudit::default());
        let config = stream_config(&opts.config);

        let gateway = Arc::new(ModelPolicyGateway::from_client(policy.clone()));
        let audit_plugin: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        let audit_gateway = audit_gateway_for(&config.vendor, Some(audit_plugin));
        let partitions = u16::try_from(config.outbox.num_partitions).expect("partitions");
        let usage_queue = if opts.register_usage_queue {
            config.outbox.queue_name.clone()
        } else {
            "mini-chat.unrelated_usage".to_owned()
        };
        let audit_queue = if opts.register_audit_queue {
            config.outbox.audit_queue_name.clone()
        } else {
            "mini-chat.unrelated_audit".to_owned()
        };
        // The S2 harness already runs an outbox on this database; SQLite answers `database is
        // locked` (no busy wait) to a registration that races its workers, so starting ours is
        // retried.
        let mut attempts = 0;
        let outbox = loop {
            let started = toolkit_db::outbox::Outbox::builder(chat.db.db())
                .profile(OutboxProfile::low_latency())
                .queue(&usage_queue, Partitions::of(partitions))
                .leased(UsageOutboxHandler::new(gateway.clone()))
                .queue(&audit_queue, Partitions::of(partitions))
                .leased(AuditOutboxHandler::new(audit_gateway.clone()))
                .start()
                .await;
            match started {
                Ok(outbox) => break outbox,
                Err(error) if attempts < 40 => {
                    attempts += 1;
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("usage / audit outbox starts: {error:?}"),
            }
        };
        let usage_port = Arc::new(UsageOutbox::new(
            outbox.outbox().clone(),
            config.outbox.queue_name.clone(),
            partitions,
        ));
        let audit_port = Arc::new(AuditOutbox::new(
            outbox.outbox().clone(),
            config.outbox.audit_queue_name.clone(),
            partitions,
        ));

        let observed = ObservingOagw::new(oagw.clone(), chat.db.clone());
        let oagw_client: Arc<dyn ServiceGatewayClientV1> = observed.clone();
        let llm = Arc::new(LlmProviders::new(oagw_client, &config));
        let enforcer = opts.pdp.enforcer();
        let stream = Arc::new(
            StreamService::new(
                chat.db.clone(),
                gateway.clone(),
                llm,
                config.clone(),
                enforcer.clone(),
            )
            .with_outbox(usage_port, audit_port),
        );
        let turns = Arc::new(TurnsService::new(chat.db.clone(), enforcer.clone()));
        let quota = Arc::new(QuotaStatusService::new(
            chat.db.clone(),
            gateway,
            config.clone(),
            enforcer,
        ));
        let router = register_stream_routes(
            chat.router.clone(),
            chat.openapi.as_ref(),
            stream,
            turns,
            &chat.prefix,
        );
        let router = register_quota_routes(router, chat.openapi.as_ref(), quota, &chat.prefix);
        Self {
            chat,
            router,
            oagw,
            observed,
            policy,
            audit,
            config,
            pdp: opts.pdp,
            _outbox: outbox,
        }
    }

    // ── URIs ─────────────────────────────────────────────────────────────────

    pub fn stream_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages:stream", self.chat.prefix)
    }

    pub fn status_uri(&self) -> String {
        format!("{}/v1/quota/status", self.chat.prefix)
    }

    // ── seeds ────────────────────────────────────────────────────────────────

    /// A chat of `USER_A` on `gpt-premium`.
    pub async fn seed_chat(&self) -> Uuid {
        self.seed_chat_on(PREMIUM).await
    }

    pub async fn seed_chat_on(&self, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(model).at(0))
            .await
    }

    /// A chat of another user (`user`, `tenant`) on `model`.
    pub async fn seed_chat_of(&self, user: Uuid, tenant: Uuid, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::new(tenant, user).with_model(model).at(0))
            .await
    }

    /// One completed assistant message of `chat` with the given token counts: the
    /// `prior_context_tokens` proxy of DESIGN 5.4.1 (`input + output` of the newest assistant
    /// message with non-zero counts).
    pub async fn seed_prior_answer(&self, chat: Uuid, input: i64, output: i64) {
        let rid = Uuid::new_v4();
        self.chat
            .seed_message(
                MessageSeed::user(chat)
                    .request(Some(rid))
                    .content("earlier question")
                    .at(1),
            )
            .await;
        self.chat
            .seed_message(
                MessageSeed::assistant(chat)
                    .request(Some(rid))
                    .content("earlier answer")
                    .model(PREMIUM)
                    .tokens(input, output)
                    .at(2),
            )
            .await;
    }

    /// A ready document attachment of `chat` plus the `chat_vector_stores` row that makes the
    /// `file_search` tool (and its reserve surcharge) available. Returns the attachment id.
    pub async fn seed_document_with_store(&self, chat: Uuid) -> Uuid {
        let attachment = self
            .chat
            .seed_attachment(AttachmentSeed::document(chat, "doc.pdf"))
            .await;
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<chat_vector_stores::Entity>(
            chat_vector_stores::ActiveModel {
                id: sea_orm::ActiveValue::Set(Uuid::new_v4()),
                tenant_id: sea_orm::ActiveValue::Set(TENANT_A),
                chat_id: sea_orm::ActiveValue::Set(chat),
                vector_store_id: sea_orm::ActiveValue::Set(Some("vs_quota_test_store".to_owned())),
                provider: sea_orm::ActiveValue::Set("openai".to_owned()),
                file_count: sea_orm::ActiveValue::Set(1),
                created_at: sea_orm::ActiveValue::Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed vector store");
        attachment
    }

    /// A ready image attachment of `chat`.
    pub async fn seed_image(&self, chat: Uuid) -> Uuid {
        self.chat
            .seed_attachment(AttachmentSeed::image(chat, "photo.png"))
            .await
    }

    /// Insert one `quota_usage` row, or - when a turn already created the row of this unique key
    /// `(tenant, user, period, period start, bucket)` (DESIGN 3.7: one row per bucket) - add the
    /// seeded amounts on top of it. A plain insert would be a unique violation.
    pub async fn seed_usage(&self, seed: UsageSeed) {
        use quota_usage::Column;
        let conn = self.chat.db.conn().expect("db connection");
        let model = quota_usage::ActiveModel {
            id: sea_orm::ActiveValue::Set(Uuid::new_v4()),
            tenant_id: sea_orm::ActiveValue::Set(seed.tenant),
            user_id: sea_orm::ActiveValue::Set(seed.user),
            period_type: sea_orm::ActiveValue::Set(seed.period.to_owned()),
            period_start: sea_orm::ActiveValue::Set(if seed.stale {
                previous_period_start(seed.period)
            } else {
                period_start_today(seed.period)
            }),
            bucket: sea_orm::ActiveValue::Set(seed.bucket.to_owned()),
            spent_credits_micro: sea_orm::ActiveValue::Set(seed.spent),
            reserved_credits_micro: sea_orm::ActiveValue::Set(seed.reserved),
            calls: sea_orm::ActiveValue::Set(seed.calls),
            input_tokens: sea_orm::ActiveValue::Set(0),
            output_tokens: sea_orm::ActiveValue::Set(0),
            file_search_calls: sea_orm::ActiveValue::Set(0),
            web_search_calls: sea_orm::ActiveValue::Set(seed.web_search_calls),
            code_interpreter_calls: sea_orm::ActiveValue::Set(seed.code_interpreter_calls),
            rag_retrieval_calls: sea_orm::ActiveValue::Set(0),
            image_inputs: sea_orm::ActiveValue::Set(0),
            image_upload_bytes: sea_orm::ActiveValue::Set(0),
            updated_at: sea_orm::ActiveValue::Set(None),
        };
        let table = quota_usage::Entity;
        let add = |column: Column, by: i64| {
            sea_orm::sea_query::ExprTrait::add(Expr::col((table, column)), by)
        };
        let on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
            Column::TenantId,
            Column::UserId,
            Column::PeriodType,
            Column::PeriodStart,
            Column::Bucket,
        ])
        .value(
            Column::SpentCreditsMicro,
            add(Column::SpentCreditsMicro, seed.spent),
        )
        .and_then(|c| {
            c.value(
                Column::ReservedCreditsMicro,
                add(Column::ReservedCreditsMicro, seed.reserved),
            )
        })
        .and_then(|c| c.value(Column::Calls, add(Column::Calls, i64::from(seed.calls))))
        .and_then(|c| {
            c.value(
                Column::WebSearchCalls,
                add(Column::WebSearchCalls, i64::from(seed.web_search_calls)),
            )
        })
        .and_then(|c| {
            c.value(
                Column::CodeInterpreterCalls,
                add(
                    Column::CodeInterpreterCalls,
                    i64::from(seed.code_interpreter_calls),
                ),
            )
        })
        .expect("seed on-conflict clause");
        quota_usage::Entity::insert(model)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .expect("seed scope")
            .on_conflict(on_conflict)
            .exec(&conn)
            .await
            .expect("seed quota_usage");
    }

    // ── sending ──────────────────────────────────────────────────────────────

    /// `POST messages:stream` as `ctx` with a JSON body; the body is not read.
    pub async fn open(&self, chat: Uuid, ctx: &SecurityContext, body: &Value) -> RawStream {
        let mut req = Request::builder()
            .method("POST")
            .uri(self.stream_uri(chat))
            .header(header::CONTENT_TYPE, "application/json")
            .body(AxumBody::from(body.to_string()))
            .expect("request");
        req.extensions_mut().insert(ctx.clone());
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        RawStream::from_response(resp)
    }

    /// Send as `USER_A`, read the whole SSE answer.
    pub async fn send(&self, chat: Uuid, body: &Value) -> Sent {
        self.open(chat, &ctx_a(), body).await.collect().await
    }

    pub async fn say(&self, chat: Uuid, text: &str) -> Sent {
        self.send(chat, &json!({"content": text})).await
    }

    pub async fn say_with(&self, chat: Uuid, text: &str, request_id: Uuid) -> Sent {
        self.send(chat, &json!({"content": text, "request_id": request_id}))
            .await
    }

    /// A message with `web_search.enabled = true`.
    pub async fn say_web(&self, chat: Uuid, text: &str) -> Sent {
        self.send(
            chat,
            &json!({"content": text, "web_search": {"enabled": true}}),
        )
        .await
    }

    /// Send as `USER_A` and expect a JSON rejection (no SSE stream).
    pub async fn reject(&self, chat: Uuid, body: Value) -> Resp {
        self.reject_as(chat, &ctx_a(), body).await
    }

    pub async fn reject_as(&self, chat: Uuid, ctx: &SecurityContext, body: Value) -> Resp {
        let raw = self.open(chat, ctx, &body).await;
        assert!(
            !raw.is_sse(),
            "expected a JSON rejection, got an SSE stream ({})",
            raw.content_type()
        );
        raw.into_resp().await
    }

    /// `GET /v1/chats/{chat}/turns/{request_id}` as `ctx`.
    pub async fn turn_status(&self, chat: Uuid, request_id: Uuid, ctx: &SecurityContext) -> Resp {
        crate::common::get(
            &self.router,
            &format!("{}/v1/chats/{chat}/turns/{request_id}", self.chat.prefix),
            ctx,
        )
        .await
    }

    /// `GET /v1/quota/status` as `ctx`.
    pub async fn status_as(&self, ctx: &SecurityContext) -> Resp {
        crate::common::get(&self.router, &self.status_uri(), ctx).await
    }

    /// `GET /v1/quota/status` as `USER_A`; must be 200.
    pub async fn status(&self) -> Value {
        let resp = self.status_as(&ctx_a()).await;
        assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
        resp.json()
    }

    // ── readers (unscoped: they assert what the service did) ─────────────────

    pub async fn all_turns(&self) -> Vec<mini_chat::infra::db::entity::chat_turns::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        mini_chat::infra::db::entity::chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
    }

    pub async fn turn_of(
        &self,
        request_id: Uuid,
    ) -> Option<mini_chat::infra::db::entity::chat_turns::Model> {
        self.all_turns()
            .await
            .into_iter()
            .find(|t| t.request_id == request_id)
    }

    /// Wait (10 s) until the turn of `request_id` reached `state`.
    pub async fn wait_turn_state(
        &self,
        request_id: Uuid,
        state: &str,
    ) -> mini_chat::infra::db::entity::chat_turns::Model {
        let mut last = None;
        for _ in 0..1000 {
            if let Some(turn) = self.turn_of(request_id).await {
                if turn.state == state {
                    return turn;
                }
                last = Some(turn.state.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("turn {request_id} never reached `{state}` (last state {last:?})");
    }

    /// Finalize the turn of `request_id` behind the stream's back (what the orphan watchdog does):
    /// `running` -> `state` with `error_code`, `completed_at` set. The provider task's own CAS
    /// then loses.
    pub async fn finalize_externally(
        &self,
        request_id: Uuid,
        state: &str,
        error_code: Option<&str>,
    ) {
        let conn = self.chat.db.conn().expect("db connection");
        let now = to_db(OffsetDateTime::now_utc());
        let updated = chat_turns::Entity::update_many()
            .col_expr(chat_turns::Column::State, Expr::value(state))
            .col_expr(
                chat_turns::Column::ErrorCode,
                Expr::value(error_code.map(str::to_owned)),
            )
            .col_expr(chat_turns::Column::CompletedAt, Expr::value(now))
            .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
            .filter(chat_turns::Column::RequestId.eq(request_id))
            .filter(chat_turns::Column::State.eq("running"))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("finalize externally");
        assert_eq!(updated.rows_affected, 1, "the turn was not running");
    }

    pub async fn usage_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage")
    }

    /// The row of `USER_A` for (`bucket`, `period`) in today's / this month's period.
    pub async fn bucket(&self, bucket: &str, period: &str) -> Option<quota_usage::Model> {
        self.bucket_of(USER_A, bucket, period).await
    }

    pub async fn bucket_of(
        &self,
        user: Uuid,
        bucket: &str,
        period: &str,
    ) -> Option<quota_usage::Model> {
        self.usage_rows().await.into_iter().find(|row| {
            row.user_id == user
                && row.bucket == bucket
                && row.period_type == period
                && row.period_start == period_start_today(period)
        })
    }

    /// `(spent, reserved)` of `USER_A`'s bucket row (zeros when there is none).
    pub async fn amounts(&self, bucket: &str, period: &str) -> (i64, i64) {
        self.bucket(bucket, period).await.map_or((0, 0), |r| {
            (r.spent_credits_micro, r.reserved_credits_micro)
        })
    }

    /// Counts of every table a rejected / replayed request must not touch:
    /// `(messages, turns, quota rows, chats)`.
    pub async fn row_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.chat.all_messages().await.len(),
            self.all_turns().await.len(),
            self.usage_rows().await.len(),
            self.chat.all_chats().await.len(),
        )
    }

    /// Snapshot of every `quota_usage` row as comparable tuples.
    pub async fn usage_fingerprint(&self) -> Vec<(String, String, i64, i64, i32, i32)> {
        let mut rows: Vec<_> = self
            .usage_rows()
            .await
            .into_iter()
            .map(|r| {
                (
                    format!("{}/{}", r.user_id, r.bucket),
                    r.period_type,
                    r.spent_credits_micro,
                    r.reserved_credits_micro,
                    r.calls,
                    r.web_search_calls,
                )
            })
            .collect();
        rows.sort();
        rows
    }

    /// Every `reserved_credits_micro` of every bucket row is back to zero.
    pub async fn assert_nothing_reserved(&self) {
        for row in self.usage_rows().await {
            assert_eq!(
                row.reserved_credits_micro, 0,
                "reserve of {}/{} is still held: {row:?}",
                row.bucket, row.period_type
            );
        }
    }
}

// ── quota_usage seeds ────────────────────────────────────────────────────────

/// A `quota_usage` row for today's (daily) / this month's (monthly) period.
pub struct UsageSeed {
    pub user: Uuid,
    pub tenant: Uuid,
    pub bucket: &'static str,
    pub period: &'static str,
    pub spent: i64,
    pub reserved: i64,
    pub calls: i32,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
    /// `true`: the row of the previous period (yesterday / last month), which no current turn or
    /// status call may read.
    pub stale: bool,
}

impl UsageSeed {
    fn of(bucket: &'static str, period: &'static str) -> Self {
        Self {
            user: USER_A,
            tenant: TENANT_A,
            bucket,
            period,
            spent: 0,
            reserved: 0,
            calls: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            stale: false,
        }
    }

    pub fn total(period: &'static str) -> Self {
        Self::of("total", period)
    }

    pub fn premium(period: &'static str) -> Self {
        Self::of("tier:premium", period)
    }

    pub fn spent(mut self, value: i64) -> Self {
        self.spent = value;
        self
    }

    pub fn reserved(mut self, value: i64) -> Self {
        self.reserved = value;
        self
    }

    pub fn web_search_calls(mut self, value: i32) -> Self {
        self.web_search_calls = value;
        self
    }

    pub fn code_interpreter_calls(mut self, value: i32) -> Self {
        self.code_interpreter_calls = value;
        self
    }

    /// The row of the previous period.
    pub fn stale(mut self) -> Self {
        self.stale = true;
        self
    }

    pub fn user(mut self, user: Uuid, tenant: Uuid) -> Self {
        self.user = user;
        self.tenant = tenant;
        self
    }
}

/// UTC date of the period `period` ("daily" / "monthly") that contains now.
pub fn period_start_today(period: &str) -> DbDate {
    let now = OffsetDateTime::now_utc();
    let day = if period == "monthly" { 1 } else { now.day() };
    DbDate::from_ymd_opt(now.year(), u32::from(u8::from(now.month())), u32::from(day))
        .expect("valid date")
}

/// First day of the period before the current `period` ("daily": yesterday, "monthly": the 1st
/// of last month).
pub fn previous_period_start(period: &str) -> DbDate {
    let today = OffsetDateTime::now_utc().date();
    let last_day_before = if period == "monthly" {
        today
            .replace_day(1)
            .expect("first of month")
            .previous_day()
            .expect("previous day")
    } else {
        today.previous_day().expect("previous day")
    };
    let day = if period == "monthly" {
        1
    } else {
        last_day_before.day()
    };
    DbDate::from_ymd_opt(
        last_day_before.year(),
        u32::from(u8::from(last_day_before.month())),
        u32::from(day),
    )
    .expect("valid date")
}

/// Exhaust a bucket: `spent == limit`.
pub async fn exhaust(h: &QuotaHarness, bucket: &'static str, period: &'static str, limit: u64) {
    let seed = if bucket == "total" {
        UsageSeed::total(period)
    } else {
        UsageSeed::premium(period)
    };
    h.seed_usage(seed.spent(i64::try_from(limit).unwrap()))
        .await;
}

// ── wire helpers ─────────────────────────────────────────────────────────────

/// The `tiers[]` entry named `tier` of a quota status body.
pub fn tier_of(body: &Value, tier: &str) -> Option<Value> {
    body["tiers"]
        .as_array()
        .expect("tiers array")
        .iter()
        .find(|t| t["tier"] == json!(tier))
        .cloned()
}

/// The `periods[]` entry named `period` of a tier entry.
pub fn period_of(tier: &Value, period: &str) -> Option<Value> {
    tier["periods"]
        .as_array()
        .expect("periods array")
        .iter()
        .find(|p| p["period"] == json!(period))
        .cloned()
}

/// `status[tier][period]` or panic.
pub fn status_period(body: &Value, tier: &str, period: &str) -> Value {
    let t = tier_of(body, tier).unwrap_or_else(|| panic!("tier {tier} missing in {body}"));
    period_of(&t, period).unwrap_or_else(|| panic!("period {period} missing in {t}"))
}

/// The `quota_warnings[]` entry of a `done` payload for (`tier`, `period`).
pub fn warning_of(done: &Value, tier: &str, period: &str) -> Option<Value> {
    done["quota_warnings"]
        .as_array()?
        .iter()
        .find(|w| w["tier"] == json!(tier) && w["period"] == json!(period))
        .cloned()
}

/// A completed answer (`chunks`) reporting `input` / `output` tokens.
pub fn answer_with_usage(input: u64, output: u64) -> ProviderReply {
    ProviderReply::text_with(&["Hello", " there"], &Opts::default().usage(input, output))
}

/// Assert a 429 `resource_exhausted` problem whose first violation has `subject`.
pub fn assert_exhausted(resp: &Resp, subject: &str) -> Value {
    let problem = assert_problem(resp, 429, "resource_exhausted");
    let violations = problem["context"]["violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no violations in {}", resp.raw));
    assert_eq!(
        violations[0]["subject"],
        json!(subject),
        "quota scope: {}",
        resp.raw
    );
    assert_eq!(violations[0]["description"], json!("quota_exceeded"));
    problem
}

/// `dedupe_key` of DESIGN 5.6: `{tenant}/{turn}/{request}` in the simple UUID form.
pub fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!("{}/{}/{}", hex(tenant), hex(turn), hex(request))
}
