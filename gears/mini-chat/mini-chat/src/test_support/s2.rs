//! Slice S2 test doubles: a scripted `authn_resolver`, a recording OAGW gateway, a scripted
//! audit plugin, audit event builders and an in-memory OTLP metrics harness.
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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use authz_resolver_sdk::AuthZResolverApi;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    QuotaDecision, TurnAuditEvent, TurnDeleteAuditEvent, TurnMutationAuditEvent, UsageTokens,
};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use secrecy::ExposeSecret;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::client_hub::ClientHub;
use toolkit::gts::PluginV1;
use toolkit_db::outbox::OutboxMessage;
use toolkit_security::SecurityContext;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};
use uuid::Uuid;

use super::{Pdp, RecordingPdp, RecordingRegistry, TENANT_A, ctx};
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
use crate::infra::metrics::MiniChatMetrics;

/// The subject of the S2S context returned by [`ScriptedAuthN::ok`].
pub const S2S_SUBJECT: Uuid = Uuid::from_u128(0x5252_0000_0000_0000_0000_0000_0000_0001);

// ── authn_resolver ───────────────────────────────────────────────────────────

/// How the scripted `authn_resolver` answers an exchange.
#[derive(Debug, Clone)]
pub enum AuthNScript {
    /// Return this security context.
    Ok(SecurityContext),
    /// `TokenAcquisitionFailed` (bad credentials).
    TokenFailure,
    /// `ServiceUnavailable`.
    Unavailable,
}

/// An `AuthNResolverClient` that answers `exchange_client_credentials` from a script and
/// records the credentials it received as `(client_id, client_secret)`.
pub struct ScriptedAuthN {
    script: AuthNScript,
    pub exchanges: Mutex<Vec<(String, String)>>,
}

impl ScriptedAuthN {
    pub fn new(script: AuthNScript) -> Arc<Self> {
        Arc::new(Self {
            script,
            exchanges: Mutex::new(Vec::new()),
        })
    }

    /// An exchange that yields the S2S context `(S2S_SUBJECT, TENANT_A)`.
    pub fn ok() -> Arc<Self> {
        Self::new(AuthNScript::Ok(s2s_ctx()))
    }

    pub fn exchange_count(&self) -> usize {
        self.exchanges.lock().unwrap().len()
    }
}

/// The S2S security context of the scripted exchange.
pub fn s2s_ctx() -> SecurityContext {
    ctx(S2S_SUBJECT, TENANT_A)
}

#[async_trait]
impl AuthNResolverClient for ScriptedAuthN {
    async fn authenticate(
        &self,
        _bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        unimplemented!("S2 tests never authenticate a bearer token")
    }

    async fn exchange_client_credentials(
        &self,
        request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        self.exchanges.lock().unwrap().push((
            request.client_id.clone(),
            request.client_secret.expose_secret().to_owned(),
        ));
        match &self.script {
            AuthNScript::Ok(security_context) => Ok(AuthenticationResult {
                security_context: security_context.clone(),
            }),
            AuthNScript::TokenFailure => Err(AuthNResolverError::TokenAcquisitionFailed(
                "bad credentials".to_owned(),
            )),
            AuthNScript::Unavailable => Err(AuthNResolverError::ServiceUnavailable(
                "idp unreachable".to_owned(),
            )),
        }
    }
}

// ── OAGW ─────────────────────────────────────────────────────────────────────

#[toolkit_canonical_errors::resource_error(toolkit_gts::gts_id!("cf.core.oagw.upstream.v1~"))]
struct OagwUpstreamScope;

#[toolkit_canonical_errors::resource_error(toolkit_gts::gts_id!("cf.core.oagw.route.v1~"))]
struct OagwRouteScope;

/// How `create_upstream` answers for one alias.
#[derive(Debug, Clone)]
pub enum UpstreamScript {
    /// Create the upstream.
    Ok,
    /// `AlreadyExists`; the existing upstream must have been added with
    /// [`RecordingOagw::with_existing`].
    AlreadyExists,
    /// The credstore secret is not readable for the first `failures` calls, then `Ok`:
    /// `FailedPrecondition` with the violation `auth.config.secret_ref` of type `STATE`.
    SecretUnreadable { failures: usize },
    /// The credstore secret never becomes readable.
    SecretUnreadableForever,
    /// `FailedPrecondition` about something other than the secret.
    OtherPrecondition,
    /// `InvalidArgument` (a deterministic misconfiguration).
    Invalid,
    /// `ServiceUnavailable`.
    Unavailable,
    /// `Internal`.
    Internal,
}

/// A recorded `create_upstream` call.
#[derive(Debug, Clone)]
pub struct UpstreamCall {
    pub alias: String,
    pub request: CreateUpstreamRequest,
    pub subject: Uuid,
    pub tenant: Uuid,
    pub at: tokio::time::Instant,
}

/// A recorded `create_route` call.
#[derive(Debug, Clone)]
pub struct RouteCall {
    pub request: CreateRouteRequest,
    pub subject: Uuid,
}

/// A `ServiceGatewayClientV1` that records `create_upstream` and `create_route` calls and
/// answers `create_upstream` per alias from a script.
pub struct RecordingOagw {
    default_script: UpstreamScript,
    scripts: Mutex<HashMap<String, UpstreamScript>>,
    failures_left: Mutex<HashMap<String, usize>>,
    existing: Mutex<Vec<Upstream>>,
    fail_routes_containing: Mutex<Option<String>>,
    rewrite_alias: Mutex<Option<String>>,
    /// `(upstream, path, method)` of the routes created so far: OAGW rejects an identical
    /// enabled route on the same upstream (`check_route_overlap`) with `AlreadyExists`.
    created_routes: Mutex<std::collections::HashSet<(Uuid, String, String)>>,
    pub upstream_calls: Mutex<Vec<UpstreamCall>>,
    pub route_calls: Mutex<Vec<RouteCall>>,
    next_route: AtomicUsize,
    /// Slice S2-provider-path: where `proxy_request` goes (the provisioning-only mock has none).
    proxy: Mutex<Option<Arc<super::provider::MockProvider>>>,
}

impl RecordingOagw {
    pub fn new(default_script: UpstreamScript) -> Arc<Self> {
        Arc::new(Self {
            default_script,
            scripts: Mutex::new(HashMap::new()),
            failures_left: Mutex::new(HashMap::new()),
            existing: Mutex::new(Vec::new()),
            fail_routes_containing: Mutex::new(None),
            rewrite_alias: Mutex::new(None),
            created_routes: Mutex::new(std::collections::HashSet::new()),
            upstream_calls: Mutex::new(Vec::new()),
            route_calls: Mutex::new(Vec::new()),
            next_route: AtomicUsize::new(0),
            proxy: Mutex::new(None),
        })
    }

    /// Answer `proxy_request` through `mock` (the provider mock records the request and
    /// streams the scripted answer).
    pub fn proxy_via(&self, mock: Arc<super::provider::MockProvider>) {
        *self.proxy.lock().unwrap() = Some(mock);
    }

    /// Every upstream and route is created.
    pub fn accepting() -> Arc<Self> {
        Self::new(UpstreamScript::Ok)
    }

    /// Answer `alias` (lowercase) with `script` instead of the default.
    pub fn script(&self, alias: &str, script: UpstreamScript) {
        if let UpstreamScript::SecretUnreadable { failures } = &script {
            self.failures_left
                .lock()
                .unwrap()
                .insert(alias.to_owned(), *failures);
        }
        self.scripts
            .lock()
            .unwrap()
            .insert(alias.to_owned(), script);
    }

    /// An upstream OAGW already holds under `alias`; returns its id.
    pub fn with_existing(&self, alias: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.existing
            .lock()
            .unwrap()
            .push(upstream_of(id, TENANT_A, alias, None));
        id
    }

    /// Fail `create_route` (invalid argument) when the route path contains `fragment`.
    pub fn fail_routes_containing(&self, fragment: &str) {
        *self.fail_routes_containing.lock().unwrap() = Some(fragment.to_owned());
    }

    /// Return upstreams whose alias is the requested one followed by `suffix` (OAGW may
    /// normalise the alias it returns).
    pub fn rewrite_returned_alias(&self, suffix: &str) {
        *self.rewrite_alias.lock().unwrap() = Some(suffix.to_owned());
    }

    pub fn upstream_call_count(&self) -> usize {
        self.upstream_calls.lock().unwrap().len()
    }

    pub fn route_call_count(&self) -> usize {
        self.route_calls.lock().unwrap().len()
    }

    /// The aliases of the `create_upstream` calls in call order.
    pub fn upstream_aliases(&self) -> Vec<String> {
        self.upstream_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.alias.clone())
            .collect()
    }

    /// The recorded `create_route` calls in call order.
    pub fn routes(&self) -> Vec<RouteCall> {
        self.route_calls.lock().unwrap().clone()
    }

    fn script_of(&self, alias: &str) -> UpstreamScript {
        self.scripts
            .lock()
            .unwrap()
            .get(alias)
            .cloned()
            .unwrap_or_else(|| self.default_script.clone())
    }
}

fn upstream_of(
    id: Uuid,
    tenant: Uuid,
    alias: &str,
    request: Option<&CreateUpstreamRequest>,
) -> Upstream {
    let server = request.map_or_else(
        || oagw_sdk::Server { endpoints: vec![] },
        |req| req.server().clone(),
    );
    Upstream {
        id,
        tenant_id: tenant,
        alias: alias.to_owned(),
        server,
        protocol: oagw_sdk::HTTP_PROTOCOL_ID.to_owned(),
        enabled: true,
        auth: request.and_then(|req| req.auth().cloned()),
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: vec![],
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for RecordingOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let alias = req.alias().unwrap_or_default().to_owned();
        self.upstream_calls.lock().unwrap().push(UpstreamCall {
            alias: alias.clone(),
            request: req.clone(),
            subject: ctx.subject_id(),
            tenant: ctx.subject_tenant_id(),
            at: tokio::time::Instant::now(),
        });
        let script = self.script_of(&alias);
        match script {
            UpstreamScript::Ok => {}
            UpstreamScript::SecretUnreadable { .. } => {
                let mut left = self.failures_left.lock().unwrap();
                let remaining = left.entry(alias.clone()).or_insert(0);
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(secret_unreadable());
                }
            }
            UpstreamScript::SecretUnreadableForever => return Err(secret_unreadable()),
            UpstreamScript::AlreadyExists => {
                return Err(OagwUpstreamScope::already_exists("upstream alias exists")
                    .with_resource(alias)
                    .create());
            }
            UpstreamScript::OtherPrecondition => {
                return Err(OagwUpstreamScope::failed_precondition()
                    .with_precondition_violation(
                        "server.endpoints",
                        "the endpoint is not allowed",
                        "STATE",
                    )
                    .create());
            }
            UpstreamScript::Invalid => {
                return Err(OagwUpstreamScope::invalid_argument()
                    .with_field_violation("server.endpoints", "invalid host", "INVALID_HOST")
                    .create());
            }
            UpstreamScript::Unavailable => {
                return Err(CanonicalError::service_unavailable()
                    .with_retry_after_seconds(5)
                    .with_detail("oagw unavailable")
                    .create());
            }
            UpstreamScript::Internal => {
                return Err(CanonicalError::internal("oagw exploded").create());
            }
        }
        let returned = match self.rewrite_alias.lock().unwrap().as_deref() {
            Some(suffix) => format!("{alias}{suffix}"),
            None => alias,
        };
        let upstream = upstream_of(
            Uuid::new_v4(),
            ctx.subject_tenant_id(),
            &returned,
            Some(&req),
        );
        self.existing.lock().unwrap().push(upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("S2 tests never read an upstream by id")
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(self.existing.lock().unwrap().clone())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("S2 tests never update an upstream")
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!("S2 tests never delete an upstream")
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.route_calls.lock().unwrap().push(RouteCall {
            request: req.clone(),
            subject: ctx.subject_id(),
        });
        let path = req
            .match_rules()
            .http
            .as_ref()
            .map(|http| http.path.clone())
            .unwrap_or_default();
        if let Some(fragment) = self.fail_routes_containing.lock().unwrap().as_deref()
            && path.contains(fragment)
        {
            return Err(OagwUpstreamScope::invalid_argument()
                .with_field_violation("match_rules", "route rejected", "INVALID_ROUTE")
                .create());
        }
        let methods = req
            .match_rules()
            .http
            .as_ref()
            .map(|http| http.methods.clone())
            .unwrap_or_default();
        {
            let mut created = self.created_routes.lock().unwrap();
            let keys: Vec<_> = methods
                .iter()
                .map(|method| (req.upstream_id(), path.clone(), format!("{method:?}")))
                .collect();
            if keys.iter().any(|key| created.contains(key)) {
                return Err(OagwRouteScope::already_exists("route overlap")
                    .with_resource(path)
                    .create());
            }
            created.extend(keys);
        }
        self.next_route.fetch_add(1, Ordering::SeqCst);
        Ok(Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: 0,
            enabled: true,
        })
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        unimplemented!("S2 tests never read a route")
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        unimplemented!("S2 tests never list routes")
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!("S2 tests never update a route")
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unimplemented!("S2 tests never delete a route")
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unimplemented!("S2 tests never resolve a proxy target")
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let mock = self.proxy.lock().unwrap().clone();
        match mock {
            Some(mock) => mock.proxy_request(ctx, req).await,
            None => unimplemented!("this test never proxies"),
        }
    }
}

/// The canonical error OAGW raises when the `secret_ref` of the auth config is not readable.
fn secret_unreadable() -> CanonicalError {
    OagwUpstreamScope::failed_precondition()
        .with_precondition_violation(
            "auth.config.secret_ref",
            "secret_ref 'cred://openai-key' is not accessible to this tenant",
            "STATE",
        )
        .create()
}

// ── platform hub ─────────────────────────────────────────────────────────────

/// The platform clients of a start test.
pub struct Platform {
    pub hub: Arc<ClientHub>,
    pub registry: Arc<RecordingRegistry>,
    pub authn: Arc<ScriptedAuthN>,
    pub oagw: Arc<RecordingOagw>,
}

/// A hub with an empty types-registry, a PDP, the scripted `authn_resolver` and the
/// recording OAGW.
pub fn platform(authn: Arc<ScriptedAuthN>, oagw: Arc<RecordingOagw>) -> Platform {
    let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
    let hub = Arc::new(ClientHub::new());
    let dynamic_registry: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(dynamic_registry);
    let pdp: Arc<dyn AuthZResolverApi> = RecordingPdp::new(Pdp::NoConstraints);
    hub.register::<dyn AuthZResolverApi>(pdp);
    let authn_dyn: Arc<dyn AuthNResolverClient> = authn.clone();
    hub.register::<dyn AuthNResolverClient>(authn_dyn);
    let oagw_dyn: Arc<dyn ServiceGatewayClientV1> = oagw.clone();
    hub.register::<dyn ServiceGatewayClientV1>(oagw_dyn);
    Platform {
        hub,
        registry,
        authn,
        oagw,
    }
}

// ── audit plugin ─────────────────────────────────────────────────────────────

/// How the scripted audit plugin answers `emit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditScript {
    Accept,
    Transient,
    Timeout,
    Permanent,
    /// Never answers (drives the 30 s plugin timeout).
    Hang,
}

/// A `MiniChatAuditPluginClientV1` that records every event it receives.
pub struct ScriptedAuditPlugin {
    script: Mutex<AuditScript>,
    pub events: Mutex<Vec<AuditEvent>>,
}

impl ScriptedAuditPlugin {
    pub fn new(script: AuditScript) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            events: Mutex::new(Vec::new()),
        })
    }

    pub fn set_script(&self, script: AuditScript) {
        *self.script.lock().unwrap() = script;
    }

    pub fn received(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for ScriptedAuditPlugin {
    async fn emit(&self, event: AuditEvent) -> Result<(), AuditPluginError> {
        self.events.lock().unwrap().push(event);
        let script = *self.script.lock().unwrap();
        match script {
            AuditScript::Accept => Ok(()),
            AuditScript::Transient => Err(AuditPluginError::Transient("backend busy".to_owned())),
            AuditScript::Timeout => Err(AuditPluginError::PluginTimeout),
            AuditScript::Permanent => Err(AuditPluginError::Permanent("rejected".to_owned())),
            AuditScript::Hang => {
                std::future::pending::<()>().await;
                unreachable!("pending never completes")
            }
        }
    }
}

/// An audit plugin instance for types-registry: `(instance id, instance)`.
pub fn audit_instance(segment: &str, vendor: &str, priority: i16) -> (String, GtsInstance) {
    let (id, content) =
        PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(segment, vendor, priority)
            .expect("plugin registration");
    let id: String = id.as_ref().to_owned();
    let instance = make_test_instance(&id, content);
    (id, instance)
}

/// A turn finalization event with the given chat id.
pub fn turn_event(chat_id: Uuid) -> TurnAuditEvent {
    TurnAuditEvent {
        tenant_id: TENANT_A,
        user_id: super::USER_A1,
        chat_id,
        turn_id: Uuid::from_u128(0x7001),
        request_id: Uuid::from_u128(0x7002),
        requester_type: "user".to_owned(),
        selected_model: "gpt-premium".to_owned(),
        effective_model: "gpt-standard".to_owned(),
        terminal_state: "completed".to_owned(),
        error_code: None,
        usage: Some(UsageTokens {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        latency_ms: 1234,
        web_search_calls: 1,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        quota_decision: QuotaDecision::Downgrade,
        downgrade_from: Some("gpt-premium".to_owned()),
        downgrade_reason: Some("premium_quota_exhausted".to_owned()),
        trace_id: None,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        prompt: String::new(),
        response: String::new(),
        attachments: vec![],
        license: None,
        quota_scope: None,
    }
}

/// One event of every type: `turn_completed`, `turn_failed`, `turn_retry`, `turn_edit`,
/// `turn_delete`; all carry `chat_id`.
pub fn all_audit_events(chat_id: Uuid) -> Vec<AuditEvent> {
    let mutation = TurnMutationAuditEvent {
        actor_user_id: super::USER_A1,
        chat_id,
        original_request_id: Uuid::from_u128(0x7003),
        new_request_id: Uuid::from_u128(0x7004),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    };
    vec![
        AuditEvent::TurnCompleted(turn_event(chat_id)),
        AuditEvent::TurnFailed(TurnAuditEvent {
            terminal_state: "failed".to_owned(),
            error_code: Some("provider_error".to_owned()),
            ..turn_event(chat_id)
        }),
        AuditEvent::TurnRetry(mutation.clone()),
        AuditEvent::TurnEdit(mutation),
        AuditEvent::TurnDelete(TurnDeleteAuditEvent {
            actor_user_id: super::USER_A1,
            chat_id,
            request_id: Uuid::from_u128(0x7005),
            timestamp: OffsetDateTime::UNIX_EPOCH,
        }),
    ]
}

/// An audit queue message carrying `payload` delivered for the `attempts`-th time (0 on the
/// first delivery).
pub fn audit_message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: AUDIT_PAYLOAD_TYPE.to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

/// The message of a well-formed event.
pub fn audit_message_of(event: &AuditEvent, attempts: i16) -> OutboxMessage {
    audit_message(
        serde_json::to_vec(event).expect("serialize event"),
        attempts,
    )
}

// ── metrics harness ──────────────────────────────────────────────────────────

/// One data point of an instrument: `(name, attributes, value)`.
pub type Point = (String, BTreeMap<String, String>, i64);

/// An in-memory OTLP pipeline with the gear instruments registered on it.
pub struct MetricsHarness {
    pub provider: SdkMeterProvider,
    pub exporter: InMemoryMetricExporter,
    pub metrics: Arc<MiniChatMetrics>,
    prefix: String,
}

/// Register the gear instruments under `prefix` (the effective prefix, e.g. `mini_chat`).
pub fn metrics_harness(prefix: &str) -> MetricsHarness {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Arc::new(MiniChatMetrics::new(&provider.meter("mini-chat"), prefix));
    MetricsHarness {
        provider,
        exporter,
        metrics,
        prefix: prefix.to_owned(),
    }
}

impl MetricsHarness {
    /// The data points of the u64 counter `name` as `(attributes, value)`.
    pub fn counter_points(&self, name: &str) -> Vec<(BTreeMap<String, String>, u64)> {
        // the exporter is cumulative and keeps every export: clear it first so that one call
        // reads exactly one snapshot, however often the harness is queried
        self.exporter.reset();
        self.provider.force_flush().expect("flush");
        let mut points = Vec::new();
        for resource_metrics in self.exporter.get_finished_metrics().expect("metrics") {
            for scope in resource_metrics.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() != name {
                        continue;
                    }
                    if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                        for point in sum.data_points() {
                            let attributes = point
                                .attributes()
                                .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
                                .collect();
                            points.push((attributes, point.value()));
                        }
                    }
                }
            }
        }
        points
    }

    /// Every data point of every recorded instrument: `(name, attributes, value)`; the value
    /// is the sum of a counter or up-down counter and the sample count of a histogram.
    pub fn all_points(&self) -> Vec<Point> {
        self.exporter.reset();
        self.provider.force_flush().expect("flush");
        let attributes_of = |attributes: &mut dyn Iterator<Item = &opentelemetry::KeyValue>| {
            attributes
                .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
                .collect::<BTreeMap<String, String>>()
        };
        let mut points = Vec::new();
        for resource_metrics in self.exporter.get_finished_metrics().expect("metrics") {
            for scope in resource_metrics.scope_metrics() {
                for metric in scope.metrics() {
                    let name = metric.name().to_owned();
                    match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                let value = i64::try_from(point.value()).unwrap_or(i64::MAX);
                                points.push((
                                    name.clone(),
                                    attributes_of(&mut point.attributes()),
                                    value,
                                ));
                            }
                        }
                        AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                            for point in sum.data_points() {
                                points.push((
                                    name.clone(),
                                    attributes_of(&mut point.attributes()),
                                    point.value(),
                                ));
                            }
                        }
                        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                            for point in histogram.data_points() {
                                let count = i64::try_from(point.count()).unwrap_or(i64::MAX);
                                points.push((
                                    name.clone(),
                                    attributes_of(&mut point.attributes()),
                                    count,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        points
    }

    /// The data points of `{prefix}_{name}`.
    pub fn series(&self, name: &str) -> Vec<(BTreeMap<String, String>, i64)> {
        let full = format!("{}_{name}", self.prefix);
        self.all_points()
            .into_iter()
            .filter(|(point, _, _)| *point == full)
            .map(|(_, attributes, value)| (attributes, value))
            .collect()
    }

    /// The total of `{prefix}_{name}` over every label set (0 when it was never recorded).
    pub fn total(&self, name: &str) -> i64 {
        self.series(name).into_iter().map(|(_, value)| value).sum()
    }

    /// The total of `{prefix}_{name}` for the label set containing `label = value`.
    pub fn total_where(&self, name: &str, label: &str, value: &str) -> i64 {
        self.series(name)
            .into_iter()
            .filter(|(attributes, _)| attributes.get(label).map(String::as_str) == Some(value))
            .map(|(_, count)| count)
            .sum()
    }

    /// The names (without the prefix) of every instrument that has at least one data point.
    pub fn recorded(&self) -> std::collections::BTreeSet<String> {
        let prefix = format!("{}_", self.prefix);
        self.all_points()
            .into_iter()
            .map(|(name, _, _)| name.strip_prefix(&prefix).unwrap_or(&name).to_owned())
            .collect()
    }

    /// The total of `{prefix}_audit_emit` for `result`.
    pub fn audit_emit(&self, result: &str) -> u64 {
        self.counter_points(&format!("{}_audit_emit", self.prefix))
            .into_iter()
            .filter(|(attributes, _)| attributes.get("result").map(String::as_str) == Some(result))
            .map(|(_, value)| value)
            .sum()
    }

    /// The total of `{prefix}_audit_emit` over every result.
    pub fn audit_emit_total(&self) -> u64 {
        self.counter_points(&format!("{}_audit_emit", self.prefix))
            .into_iter()
            .map(|(_, value)| value)
            .sum()
    }
}
