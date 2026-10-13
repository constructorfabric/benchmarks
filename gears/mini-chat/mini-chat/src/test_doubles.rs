#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(clippy::wrong_self_convention, clippy::let_underscore_must_use)]
//! Shared test doubles of the foundation slices 5 to 7: PDP, OAGW, authn
//! (S2S), model-policy and audit plugins, a plugin world over `ClientHub` and
//! types-registry, a tracing capture and payload builders.

use std::collections::{HashSet, VecDeque};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry,
    PolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError, RequesterType,
    SettlementMethod, TerminalState, TurnMutationAuditEvent, UsageEvent, UserLimits,
    encode_audit_event, encode_usage_event, usage_dedupe_key,
};
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, HttpMethod, ListQuery, PathSuffixMode, Route,
    ServiceGatewayClientV1, UpdateRouteRequest, UpdateUpstreamRequest, Upstream,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_db::outbox::OutboxMessage;
use toolkit_security::{PlatformSecurityContext, SecurityContext};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use crate::domain::authz::AuthzService;
use crate::domain::service::models::ModelsService;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::metrics::{AuditEmitMetrics, AuditEmitResult};
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::infra::test_support::FakeTypesRegistry;

/// Vendor of the gear and of the registered fake plugins.
pub const VENDOR: &str = "constructorfabric";
/// Subject of the security context the fake authn returns (the S2S identity).
pub const S2S_SUBJECT: Uuid = Uuid::from_u128(0x5252_0000_0000_0000_0000_0000_0000_0001);
/// Tenant of the S2S identity.
pub const S2S_TENANT: Uuid = Uuid::from_u128(0x5252_0000_0000_0000_0000_0000_0000_0002);
/// Tenant of the request user.
pub const USER_TENANT: Uuid = Uuid::from_u128(0x7E);
/// Request user.
pub const USER: Uuid = Uuid::from_u128(0x55);

const SUBJECT_USER_TYPE: &str = "gts.cf.core.security.subject_user.v1~";

/// Security context of a request user.
pub fn user_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(USER)
        .subject_type(SUBJECT_USER_TYPE)
        .subject_tenant_id(USER_TENANT)
        .token_scopes(vec!["ai:mini_chat".to_owned()])
        .build()
        .expect("user ctx")
}

/// Security context of the S2S identity.
pub fn s2s_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(S2S_SUBJECT)
        .subject_type(SUBJECT_USER_TYPE)
        .subject_tenant_id(S2S_TENANT)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("s2s ctx")
}

// ---- tracing capture ---------------------------------------------------------

/// One captured tracing event.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub level: Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl LogRecord {
    /// Message and every field rendered as one string.
    pub fn text(&self) -> String {
        let fields: Vec<String> = self
            .fields
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        format!("{} {}", self.message, fields.join(" "))
    }
}

struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields
                .push((field.name().to_owned(), value.to_owned()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push((field.name().to_owned(), format!("{value:?}")));
        }
    }
}

type Sink = Arc<Mutex<Vec<LogRecord>>>;

thread_local! {
    /// Sinks of the captures alive on this thread (innermost last).
    static SINKS: std::cell::RefCell<Vec<Sink>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One process-wide subscriber that forwards every event to the captures of
/// the emitting thread. A scoped per-test subscriber would race with the
/// global callsite-interest cache when tests run in parallel.
struct Router;

impl Subscriber for Router {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        SINKS.with(|sinks| {
            let sinks = sinks.borrow();
            if sinks.is_empty() {
                return;
            }
            let mut collector = FieldCollector {
                message: String::new(),
                fields: Vec::new(),
            };
            event.record(&mut collector);
            let record = LogRecord {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                message: collector.message,
                fields: collector.fields,
            };
            for sink in sinks.iter() {
                sink.lock().unwrap().push(record.clone());
            }
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

fn install_router() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(Router)
            .expect("no other global tracing subscriber in the unit tests");
    });
}

/// Captures every tracing event emitted on the current thread while alive.
/// Use it in a `current_thread` runtime (the `#[tokio::test]` default).
pub struct LogCapture {
    events: Sink,
}

impl LogCapture {
    pub fn start() -> Self {
        install_router();
        let events: Sink = Arc::new(Mutex::new(Vec::new()));
        SINKS.with(|sinks| sinks.borrow_mut().push(Arc::clone(&events)));
        Self { events }
    }

    pub fn records(&self) -> Vec<LogRecord> {
        self.events.lock().unwrap().clone()
    }

    pub fn at(&self, level: Level) -> Vec<LogRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.level == level)
            .collect()
    }

    /// Events at `level` whose rendered text contains `needle`.
    pub fn at_containing(&self, level: Level, needle: &str) -> Vec<LogRecord> {
        self.at(level)
            .into_iter()
            .filter(|record| record.text().contains(needle))
            .collect()
    }

    /// Every captured text (all levels) joined, for "never logged" assertions.
    pub fn all_text(&self) -> String {
        self.records()
            .iter()
            .map(LogRecord::text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        SINKS.with(|sinks| {
            sinks
                .borrow_mut()
                .retain(|sink| !Arc::ptr_eq(sink, &self.events));
        });
    }
}

// ---- PDP ---------------------------------------------------------------------

/// What the fake PDP answers.
#[derive(Clone, Copy)]
pub enum PdpReply {
    /// Allow without constraints (enough for decision-only checks).
    Allow,
    Deny,
    Failure,
    /// Allow with the shape of the static authz plugin: the single predicate
    /// `In(owner_tenant_id, [tenant of the subject])`, no owner predicate.
    AllowScoped,
    /// Allow with `In(owner_tenant_id, [tenant of the subject, OTHER_TENANT])`:
    /// a PDP that is wider than the request tenant.
    AllowTwoTenants,
    /// Allow with the constraints of a PDP that scopes by the chat itself:
    /// `owner_tenant_id` = tenant of the subject, `owner_id` = the subject and
    /// `id` = the resource id of the request (when the request carries one).
    /// The `id` term names the chat, so it must not constrain child tables
    /// whose own `id` is something else (reactions).
    AllowChatConstraints,
}

/// A tenant other than [`USER_TENANT`], used by isolation tests.
pub const OTHER_TENANT: Uuid = Uuid::from_u128(0x7F);
/// A user other than [`USER`].
pub const OTHER_USER: Uuid = Uuid::from_u128(0x56);

/// Security context of another user of the tenant of [`user_ctx`].
pub fn other_user_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(OTHER_USER)
        .subject_type(SUBJECT_USER_TYPE)
        .subject_tenant_id(USER_TENANT)
        .token_scopes(vec!["ai:mini_chat".to_owned()])
        .build()
        .expect("other user ctx")
}

/// Security context of a user of [`OTHER_TENANT`].
pub fn other_tenant_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(OTHER_USER)
        .subject_type(SUBJECT_USER_TYPE)
        .subject_tenant_id(OTHER_TENANT)
        .token_scopes(vec!["ai:mini_chat".to_owned()])
        .build()
        .expect("other tenant ctx")
}

fn subject_tenant(request: &EvaluationRequest) -> Uuid {
    request
        .subject
        .properties
        .get("tenant_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("the request carries the subject tenant")
}

/// Fake `AuthZResolverApi`.
pub struct FakePdp {
    reply: PdpReply,
    pub seen: Mutex<Vec<EvaluationRequest>>,
}

impl FakePdp {
    pub fn new(reply: PdpReply) -> Arc<Self> {
        Arc::new(Self {
            reply,
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<EvaluationRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant = subject_tenant(&request);
        let subject = request.subject.id;
        let resource = request.resource.id;
        self.seen.lock().unwrap().push(request);
        let scoped = |tenants: Vec<Uuid>| EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        toolkit_security::pep_properties::OWNER_TENANT_ID,
                        tenants,
                    ))],
                }],
                ..Default::default()
            },
        };
        match self.reply {
            PdpReply::Allow | PdpReply::Deny => Ok(EvaluationResponse {
                decision: matches!(self.reply, PdpReply::Allow),
                context: EvaluationResponseContext::default(),
            }),
            PdpReply::AllowScoped => Ok(scoped(vec![tenant])),
            PdpReply::AllowTwoTenants => Ok(scoped(vec![tenant, OTHER_TENANT])),
            PdpReply::AllowChatConstraints => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext {
                    constraints: vec![Constraint {
                        predicates: [
                            Some(Predicate::Eq(EqPredicate::new(
                                toolkit_security::pep_properties::OWNER_TENANT_ID,
                                tenant,
                            ))),
                            Some(Predicate::Eq(EqPredicate::new(
                                toolkit_security::pep_properties::OWNER_ID,
                                subject,
                            ))),
                            resource.map(|id| {
                                Predicate::Eq(EqPredicate::new(
                                    toolkit_security::pep_properties::RESOURCE_ID,
                                    id,
                                ))
                            }),
                        ]
                        .into_iter()
                        .flatten()
                        .collect(),
                    }],
                    ..Default::default()
                },
            }),
            PdpReply::Failure => Err(CanonicalError::service_unavailable().create()),
        }
    }
}

// ---- authn (S2S) -------------------------------------------------------------

/// What the fake `exchange_client_credentials` does.
#[derive(Clone)]
pub enum AuthnReply {
    Ok,
    Fail(String),
    Hang,
}

/// Fake `AuthNResolverClient`.
pub struct FakeAuthn {
    reply: Mutex<AuthnReply>,
    /// `(client_id, client_secret)` of every exchange.
    pub exchanges: Mutex<Vec<(String, String)>>,
}

impl FakeAuthn {
    pub fn new(reply: AuthnReply) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(reply),
            exchanges: Mutex::new(Vec::new()),
        })
    }

    pub fn exchange_count(&self) -> usize {
        self.exchanges.lock().unwrap().len()
    }
}

#[async_trait]
impl AuthNResolverClient for FakeAuthn {
    async fn authenticate(
        &self,
        _bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::Unauthorized("not used".to_owned()))
    }

    async fn exchange_client_credentials(
        &self,
        request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        self.exchanges.lock().unwrap().push((
            request.client_id.clone(),
            request.client_secret.expose_secret().to_owned(),
        ));
        let reply = self.reply.lock().unwrap().clone();
        match reply {
            AuthnReply::Ok => Ok(AuthenticationResult {
                security_context: s2s_ctx(),
            }),
            AuthnReply::Fail(message) => Err(AuthNResolverError::TokenAcquisitionFailed(message)),
            AuthnReply::Hang => {
                std::future::pending::<()>().await;
                unreachable!("the authn fake never answers")
            }
        }
    }
}

// ---- OAGW --------------------------------------------------------------------

#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
struct UpstreamScope;

#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
struct RouteScope;

/// A failure the fake OAGW injects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Canonical `failed_precondition` on `auth.config.secret_ref`.
    SecretNotReadable,
    /// Canonical `deadline_exceeded`.
    DeadlineExceeded,
    /// Canonical `already_exists` without storing anything (concurrent duplicate).
    Conflict,
    /// Canonical `internal`.
    Internal,
    /// Canonical `invalid_argument`.
    InvalidArgument,
    /// The call never answers.
    Hang,
}

impl Fault {
    fn upstream_error(self) -> Option<CanonicalError> {
        Some(match self {
            Self::SecretNotReadable => UpstreamScope::failed_precondition()
                .with_precondition_violation(
                    "auth.config.secret_ref",
                    "secret_ref is not accessible to this tenant",
                    "STATE",
                )
                .create(),
            Self::DeadlineExceeded => {
                UpstreamScope::deadline_exceeded("upstream call timed out").create()
            }
            Self::Conflict => UpstreamScope::already_exists("alias already exists")
                .with_resource("alias")
                .create(),
            Self::Internal => CanonicalError::internal("boom").create(),
            Self::InvalidArgument => UpstreamScope::invalid_argument()
                .with_field_violation("server.endpoints", "bad endpoint", "INVALID")
                .create(),
            Self::Hang => return None,
        })
    }

    fn route_error(self) -> Option<CanonicalError> {
        Some(match self {
            Self::SecretNotReadable => CanonicalError::internal("secret").create(),
            Self::DeadlineExceeded => {
                RouteScope::deadline_exceeded("route call timed out").create()
            }
            Self::Conflict => RouteScope::already_exists("route overlap")
                .with_resource("route")
                .create(),
            Self::Internal => CanonicalError::internal("boom").create(),
            Self::InvalidArgument => RouteScope::invalid_argument()
                .with_field_violation("match.http.path", "bad path", "INVALID")
                .create(),
            Self::Hang => return None,
        })
    }
}

/// One recorded OAGW call.
#[derive(Debug, Clone, PartialEq)]
pub enum OagwCall {
    CreateUpstream {
        alias: String,
        host: String,
        port: u16,
        https: bool,
        plugin_type: Option<String>,
        auth_config: Vec<(String, String)>,
    },
    ListUpstreams,
    ListRoutes {
        upstream_id: Uuid,
    },
    CreateRoute {
        upstream_id: Uuid,
        method: HttpMethod,
        path: String,
        query_allowlist: Vec<String>,
        suffix: PathSuffixMode,
    },
}

#[derive(Default)]
struct OagwState {
    proxy_queue: VecDeque<ProxyReply>,
    proxied: Vec<RecordedProxy>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    calls: Vec<(Uuid, OagwCall)>,
    upstream_faults: Vec<(String, Fault)>,
    route_faults: Vec<(String, Fault)>,
    list_routes_fault: Option<Fault>,
    rename: Vec<(String, String)>,
    rules: Vec<ScriptRule>,
}

/// A scripted answer chosen by method and URI instead of by call order.
type RuleHandler = Arc<dyn Fn(&RecordedProxy) -> ProxyReply + Send + Sync>;

struct ScriptRule {
    method: String,
    matcher: UriMatcher,
    handler: RuleHandler,
}

/// How a rule matches the URI of a call.
enum UriMatcher {
    /// The path and query contain the text.
    Contains(String),
    /// The path (query ignored) equals the pattern; `*` stands for one segment.
    Path(String),
}

impl UriMatcher {
    fn matches(&self, uri: &str) -> bool {
        match self {
            Self::Contains(text) => uri.contains(text.as_str()),
            Self::Path(pattern) => {
                let path = uri.split('?').next().unwrap_or_default();
                let wanted: Vec<&str> = pattern.split('/').collect();
                let actual: Vec<&str> = path.split('/').collect();
                wanted.len() == actual.len()
                    && wanted.iter().zip(&actual).all(|(w, a)| *w == "*" || w == a)
            }
        }
    }
}

/// A rendezvous a test uses to hold one `proxy_request` call in flight: the call
/// signals its arrival, waits for [`Gate::release`], then answers.
#[derive(Default)]
pub struct Gate {
    arrived: AtomicUsize,
    arrived_notify: tokio::sync::Notify,
    released: AtomicBool,
    release_notify: tokio::sync::Notify,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Resolves once a call is held at this gate; panics when none arrives in
    /// 5 s of wall-clock time (a test of a call that never happens must fail,
    /// not hang).
    pub async fn arrived(&self) {
        let wait = async {
            loop {
                let notified = self.arrived_notify.notified();
                if self.arrived.load(Ordering::SeqCst) > 0 {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("no call reached the gate within 5 s");
    }

    /// Number of calls that reached the gate.
    pub fn arrivals(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    /// Lets every held (and later) call through.
    pub fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release_notify.notify_waiters();
    }

    async fn pass(&self) {
        self.arrived.fetch_add(1, Ordering::SeqCst);
        self.arrived_notify.notify_waiters();
        loop {
            let notified = self.release_notify.notified();
            if self.released.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

// ---- scripted proxy ----------------------------------------------------------------------

/// The state shared by a [`BodyFeed`] and the response body built from it.
#[derive(Default)]
pub struct FeedState {
    dropped: AtomicBool,
    notify: tokio::sync::Notify,
    pulled: AtomicUsize,
}

/// The test side of a streaming response body: chunks are released one at a
/// time, so a test controls exactly when the gear can read the next event.
/// Dropping the response body on the gear side (the upstream connection being
/// closed) is observable through [`BodyFeed::dropped`].
pub struct BodyFeed {
    tx: tokio::sync::mpsc::UnboundedSender<Result<bytes::Bytes, oagw_sdk::body::BoxError>>,
    state: Arc<FeedState>,
}

impl BodyFeed {
    /// Releases a raw chunk.
    pub fn send(&self, chunk: impl Into<bytes::Bytes>) {
        let _ = self.tx.send(Ok(chunk.into()));
    }

    /// Releases one SSE frame `event: <name>\ndata: <json>\n\n`.
    pub fn send_event(&self, name: &str, data: &Value) {
        self.send(sse_frame(name, data));
    }

    /// Releases a frame without an `event:` line (the name is the `type` of
    /// the data).
    pub fn send_data(&self, data: &Value) {
        self.send(format!("data: {data}\n\n"));
    }

    /// Fails the body read.
    pub fn fail(&self, error: oagw_sdk::body::BoxError) {
        let _ = self.tx.send(Err(error));
    }

    /// Ends the body (EOF).
    pub fn end(self) {
        drop(self.tx);
    }

    /// The gear dropped the response body.
    pub fn is_dropped(&self) -> bool {
        self.state.dropped.load(Ordering::SeqCst)
    }

    /// Number of chunks the gear has read so far.
    pub fn pulled(&self) -> usize {
        self.state.pulled.load(Ordering::SeqCst)
    }

    /// Resolves when the gear drops the response body.
    pub async fn dropped(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.is_dropped() {
                return;
            }
            notified.await;
        }
    }
}

struct FeedStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, oagw_sdk::body::BoxError>>,
    state: Arc<FeedState>,
}

impl futures::Stream for FeedStream {
    type Item = Result<bytes::Bytes, oagw_sdk::body::BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        if matches!(polled, Poll::Ready(Some(_))) {
            self.state.pulled.fetch_add(1, Ordering::SeqCst);
        }
        polled
    }
}

impl Drop for FeedStream {
    fn drop(&mut self) {
        self.state.dropped.store(true, Ordering::SeqCst);
        self.state.notify.notify_waiters();
    }
}

/// One SSE frame.
pub fn sse_frame(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// A complete SSE body from `(event name, data)` pairs.
pub fn sse_body(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| sse_frame(name, data))
        .collect()
}

/// The body of a scripted response.
pub enum ReplyBody {
    /// A complete body, then EOF.
    Bytes(Vec<u8>),
    /// A body fed by the test.
    Feed(
        tokio::sync::mpsc::UnboundedReceiver<Result<bytes::Bytes, oagw_sdk::body::BoxError>>,
        Arc<FeedState>,
    ),
}

/// The scripted answer of one `proxy_request` call.
pub enum ProxyReply {
    /// A response.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: ReplyBody,
        source: Option<oagw_sdk::api::ErrorSource>,
    },
    /// The call returns this canonical error.
    Error(Fault),
    /// The call never answers (the pending call future is observable through
    /// [`FakeOagw::pending_calls_dropped`]).
    Pending,
    /// The call answers with the inner reply after the delay (tokio time).
    Delayed(std::time::Duration, Box<ProxyReply>),
    /// The call is held at the gate, then answers with the inner reply.
    Held(Arc<Gate>, Box<ProxyReply>),
}

impl ProxyReply {
    /// 200 `text/event-stream` with a complete body.
    pub fn sse(events: &[(&str, Value)]) -> Self {
        Self::sse_text(sse_body(events))
    }

    /// 200 `text/event-stream` with `text` as the complete body.
    pub fn sse_text(text: String) -> Self {
        Self::Response {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
            body: ReplyBody::Bytes(text.into_bytes()),
            source: None,
        }
    }

    /// 200 `text/event-stream` fed by the returned [`BodyFeed`].
    pub fn gated() -> (Self, BodyFeed) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(FeedState::default());
        (
            Self::Response {
                status: 200,
                headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                body: ReplyBody::Feed(rx, Arc::clone(&state)),
                source: None,
            },
            BodyFeed { tx, state },
        )
    }

    /// The same reply after `delay` of tokio time.
    #[must_use]
    pub fn after(self, delay: std::time::Duration) -> Self {
        Self::Delayed(delay, Box::new(self))
    }

    /// The same reply once `gate` is released.
    #[must_use]
    pub fn held_by(self, gate: &Arc<Gate>) -> Self {
        Self::Held(Arc::clone(gate), Box::new(self))
    }

    /// A buffered response with `status`, JSON `body` and no event stream.
    pub fn json(status: u16, body: &Value) -> Self {
        Self::Response {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: ReplyBody::Bytes(body.to_string().into_bytes()),
            source: None,
        }
    }

    /// The same reply with one more header.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Self::Response { headers, .. } = &mut self {
            headers.push((name.to_owned(), value.to_owned()));
        }
        self
    }

    /// The same reply marked as produced by the gateway (`ErrorSource`).
    #[must_use]
    pub fn from_gateway(mut self) -> Self {
        if let Self::Response { source, .. } = &mut self {
            *source = Some(oagw_sdk::api::ErrorSource::Gateway);
        }
        self
    }

    /// The same reply marked as produced by the provider (`ErrorSource`).
    #[must_use]
    pub fn from_upstream(mut self) -> Self {
        if let Self::Response { source, .. } = &mut self {
            *source = Some(oagw_sdk::api::ErrorSource::Upstream);
        }
        self
    }
}

/// One recorded `proxy_request` call.
#[derive(Debug, Clone)]
pub struct RecordedProxy {
    pub subject: Uuid,
    pub tenant: Uuid,
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedProxy {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the request body is JSON")
    }

    /// A header value (names compare case-insensitively).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sets a flag when a pending `proxy_request` future is dropped.
struct PendingGuard(Arc<AtomicUsize>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Fake `ServiceGatewayClientV1` holding upstreams and routes in memory.
///
/// Behaviour mirrors the real gateway where provisioning depends on it:
/// creating an upstream with an existing alias is `already_exists`, creating a
/// route that overlaps an existing one (same upstream, method and path) is
/// `already_exists`. A reused upstream is found through `list_upstreams`.
#[derive(Default)]
pub struct FakeOagw {
    state: Mutex<OagwState>,
    pending_dropped: Arc<AtomicUsize>,
}

impl FakeOagw {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues the answer of the next `proxy_request` call.
    pub fn push_proxy(&self, reply: ProxyReply) {
        self.state.lock().unwrap().proxy_queue.push_back(reply);
    }

    /// Answers every call whose method is `method` and whose path and query
    /// contain `uri_contains` with `handler`, before the queue is consulted.
    /// A newer rule wins over an older one.
    pub fn on(
        &self,
        method: &str,
        uri_contains: &str,
        handler: impl Fn(&RecordedProxy) -> ProxyReply + Send + Sync + 'static,
    ) {
        self.state.lock().unwrap().rules.insert(
            0,
            ScriptRule {
                method: method.to_owned(),
                matcher: UriMatcher::Contains(uri_contains.to_owned()),
                handler: Arc::new(handler),
            },
        );
    }

    /// Like [`Self::on`], matching the whole path (the query is ignored); `*`
    /// stands for exactly one path segment.
    pub fn on_path(
        &self,
        method: &str,
        pattern: &str,
        handler: impl Fn(&RecordedProxy) -> ProxyReply + Send + Sync + 'static,
    ) {
        self.state.lock().unwrap().rules.insert(
            0,
            ScriptRule {
                method: method.to_owned(),
                matcher: UriMatcher::Path(pattern.to_owned()),
                handler: Arc::new(handler),
            },
        );
    }

    /// Every call matching the path pattern answers `status` with `body`.
    pub fn on_path_json(&self, method: &str, pattern: &str, status: u16, body: &Value) {
        let body = body.clone();
        self.on_path(method, pattern, move |_| ProxyReply::json(status, &body));
    }

    /// The calls matching the path pattern answer `replies` in order; the last
    /// one repeats.
    pub fn on_path_sequence(&self, method: &str, pattern: &str, replies: Vec<(u16, Value)>) {
        let next = AtomicUsize::new(0);
        self.on_path(method, pattern, move |_| {
            let at = next.fetch_add(1, Ordering::SeqCst).min(replies.len() - 1);
            ProxyReply::json(replies[at].0, &replies[at].1)
        });
    }

    /// Every matching call answers `status` with the JSON `body`.
    pub fn on_json(&self, method: &str, uri_contains: &str, status: u16, body: &Value) {
        let body = body.clone();
        self.on(method, uri_contains, move |_| {
            ProxyReply::json(status, &body)
        });
    }

    /// The matching calls answer `replies` in order; the last one repeats.
    pub fn on_sequence(&self, method: &str, uri_contains: &str, replies: Vec<(u16, Value)>) {
        let next = AtomicUsize::new(0);
        self.on(method, uri_contains, move |_| {
            let at = next.fetch_add(1, Ordering::SeqCst).min(replies.len() - 1);
            ProxyReply::json(replies[at].0, &replies[at].1)
        });
    }

    /// The recorded calls of `method` whose path and query contain `uri_contains`.
    pub fn requests_to(&self, method: &str, uri_contains: &str) -> Vec<RecordedProxy> {
        self.proxied()
            .into_iter()
            .filter(|call| call.method == method && call.uri.contains(uri_contains))
            .collect()
    }

    /// Queues a gated streaming answer and returns its feed.
    pub fn push_gated(&self) -> BodyFeed {
        let (reply, feed) = ProxyReply::gated();
        self.push_proxy(reply);
        feed
    }

    /// The `proxy_request` calls received so far.
    pub fn proxied(&self) -> Vec<RecordedProxy> {
        self.state.lock().unwrap().proxied.clone()
    }

    /// Number of `proxy_request` calls received so far.
    pub fn proxy_call_count(&self) -> usize {
        self.state.lock().unwrap().proxied.len()
    }

    /// Number of pending (never answering) `proxy_request` futures that were
    /// dropped by the caller.
    pub fn pending_calls_dropped(&self) -> usize {
        self.pending_dropped.load(Ordering::SeqCst)
    }

    /// Stores an upstream with `alias` and returns its id.
    pub fn seed_upstream(&self, alias: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.state.lock().unwrap().upstreams.push(Upstream {
            id,
            tenant_id: S2S_TENANT,
            alias: alias.to_owned(),
            server: oagw_sdk::Server { endpoints: vec![] },
            protocol: oagw_sdk::HTTP_PROTOCOL_ID.to_owned(),
            enabled: true,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
        });
        id
    }

    /// Stores a route of `upstream_id`.
    pub fn seed_route(&self, upstream_id: Uuid, method: HttpMethod, path: &str) {
        self.state.lock().unwrap().routes.push(Route {
            id: Uuid::new_v4(),
            tenant_id: S2S_TENANT,
            upstream_id,
            match_rules: oagw_sdk::MatchRules {
                http: Some(oagw_sdk::HttpMatch {
                    methods: vec![method],
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: 0,
            enabled: true,
        });
    }

    /// `create_upstream` for `alias` fails with `fault`.
    pub fn fail_upstream(&self, alias: &str, fault: Fault) {
        self.state
            .lock()
            .unwrap()
            .upstream_faults
            .push((alias.to_owned(), fault));
    }

    /// `create_route` for the route path `path` fails with `fault`.
    pub fn fail_route(&self, path: &str, fault: Fault) {
        self.state
            .lock()
            .unwrap()
            .route_faults
            .push((path.to_owned(), fault));
    }

    /// `list_routes` fails with `fault`.
    pub fn fail_list_routes(&self, fault: Fault) {
        self.state.lock().unwrap().list_routes_fault = Some(fault);
    }

    /// Removes every injected fault.
    pub fn clear_faults(&self) {
        let mut state = self.state.lock().unwrap();
        state.upstream_faults.clear();
        state.route_faults.clear();
        state.list_routes_fault = None;
    }

    /// The upstream created for `alias` is stored (and returned) under `returned`.
    pub fn rename_alias(&self, alias: &str, returned: &str) {
        self.state
            .lock()
            .unwrap()
            .rename
            .push((alias.to_owned(), returned.to_owned()));
    }

    /// Every call in order.
    pub fn calls(&self) -> Vec<OagwCall> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|(_, call)| call.clone())
            .collect()
    }

    /// Subjects of the security contexts the calls used.
    pub fn call_subjects(&self) -> HashSet<Uuid> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|(subject, _)| *subject)
            .collect()
    }

    pub fn upstream_creates(&self) -> Vec<OagwCall> {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, OagwCall::CreateUpstream { .. }))
            .collect()
    }

    pub fn route_creates(&self) -> Vec<OagwCall> {
        self.calls()
            .into_iter()
            .filter(|call| matches!(call, OagwCall::CreateRoute { .. }))
            .collect()
    }

    /// `(method, path)` of every route create call.
    pub fn created_routes(&self) -> Vec<(HttpMethod, String)> {
        self.route_creates()
            .into_iter()
            .filter_map(|call| match call {
                OagwCall::CreateRoute { method, path, .. } => Some((method, path)),
                _ => None,
            })
            .collect()
    }

    /// Routes stored for the upstream of `alias` as `(method, path)`.
    pub fn stored_routes(&self, alias: &str) -> Vec<(HttpMethod, String)> {
        let state = self.state.lock().unwrap();
        let Some(upstream) = state.upstreams.iter().find(|u| u.alias == alias) else {
            return Vec::new();
        };
        state
            .routes
            .iter()
            .filter(|route| route.upstream_id == upstream.id)
            .filter_map(|route| {
                route
                    .match_rules
                    .http
                    .as_ref()
                    .map(|http| (http.methods[0], http.path.clone()))
            })
            .collect()
    }

    pub fn upstream_count(&self) -> usize {
        self.state.lock().unwrap().upstreams.len()
    }

    fn record(&self, ctx: &SecurityContext, call: OagwCall) {
        self.state
            .lock()
            .unwrap()
            .calls
            .push((ctx.subject_id(), call));
    }
}

fn page<T: Clone>(items: &[T], query: &ListQuery) -> Vec<T> {
    items
        .iter()
        .skip(query.skip as usize)
        .take(query.top as usize)
        .cloned()
        .collect()
}

/// Mirrors OAGW's alias rules on upstream creation
/// (`enforce_alias_create_with` / `enforce_alias_create_derived`): a hostname
/// endpoint derives `host` (standard port: 80 http, 443 https) or `host:port`
/// and rejects any other user alias; an IP literal requires an explicit alias.
/// The stored alias is lower-cased without trailing dots.
fn oagw_alias(
    endpoint: &oagw_sdk::Endpoint,
    requested: Option<&str>,
) -> Result<String, CanonicalError> {
    let invalid = |message: String| {
        UpstreamScope::invalid_argument()
            .with_field_violation("alias", message, "INVALID")
            .create()
    };
    let normalize = |alias: &str| alias.to_ascii_lowercase().trim_end_matches('.').to_owned();
    let host = endpoint
        .host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&endpoint.host)
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned();
    if host.parse::<std::net::IpAddr>().is_ok() {
        let Some(alias) = requested else {
            return Err(invalid(
                "explicit alias is required for IP-based or heterogeneous-host endpoints"
                    .to_owned(),
            ));
        };
        return Ok(normalize(alias));
    }
    let standard = match endpoint.scheme {
        oagw_sdk::Scheme::Http => 80,
        _ => 443,
    };
    let derived = if endpoint.port == standard {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    };
    if let Some(alias) = requested
        && normalize(alias) != derived
    {
        return Err(invalid(format!(
            "alias is auto-derived for hostname-based endpoints; remove the 'alias' field (derived: '{derived}')"
        )));
    }
    Ok(derived)
}

fn unsupported(what: &str) -> CanonicalError {
    CanonicalError::internal(format!("fake oagw: {what} is not supported")).create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        let endpoint = req.server().endpoints.first().cloned().expect("endpoint");
        let alias = match oagw_alias(&endpoint, req.alias()) {
            Ok(alias) => alias,
            Err(error) => {
                self.record(
                    &ctx,
                    OagwCall::CreateUpstream {
                        alias: req.alias().unwrap_or_default().to_owned(),
                        host: endpoint.host.clone(),
                        port: endpoint.port,
                        https: matches!(endpoint.scheme, oagw_sdk::Scheme::Https),
                        plugin_type: None,
                        auth_config: Vec::new(),
                    },
                );
                return Err(error);
            }
        };
        let (plugin_type, mut auth_config) = req.auth().map_or((None, Vec::new()), |auth| {
            (
                Some(auth.plugin_type.clone()),
                auth.config
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>(),
            )
        });
        auth_config.sort();
        self.record(
            &ctx,
            OagwCall::CreateUpstream {
                alias: alias.clone(),
                host: endpoint.host.clone(),
                port: endpoint.port,
                https: matches!(endpoint.scheme, oagw_sdk::Scheme::Https),
                plugin_type,
                auth_config,
            },
        );
        let fault = self
            .state
            .lock()
            .unwrap()
            .upstream_faults
            .iter()
            .find(|(a, _)| *a == alias)
            .map(|(_, fault)| *fault);
        if let Some(fault) = fault {
            let Some(error) = fault.upstream_error() else {
                std::future::pending::<()>().await;
                unreachable!("hang")
            };
            return Err(error);
        }
        let mut state = self.state.lock().unwrap();
        if state
            .upstreams
            .iter()
            .any(|u| u.alias.to_ascii_lowercase().trim_end_matches('.') == alias)
        {
            return Err(Fault::Conflict.upstream_error().unwrap());
        }
        let returned = state
            .rename
            .iter()
            .find(|(from, _)| *from == alias)
            .map_or_else(|| alias.clone(), |(_, to)| to.clone());
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: S2S_TENANT,
            alias: returned,
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: true,
            auth: req.auth().cloned(),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
        };
        state.upstreams.push(upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("get_upstream"))
    }

    async fn list_upstreams(
        &self,
        ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        self.record(&ctx, OagwCall::ListUpstreams);
        let state = self.state.lock().unwrap();
        Ok(page(&state.upstreams, query))
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("update_upstream"))
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        Err(unsupported("delete_upstream"))
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        let http = req.match_rules().http.clone().expect("http match");
        let method = http.methods[0];
        self.record(
            &ctx,
            OagwCall::CreateRoute {
                upstream_id: req.upstream_id(),
                method,
                path: http.path.clone(),
                query_allowlist: http.query_allowlist.clone(),
                suffix: http.path_suffix_mode,
            },
        );
        let fault = self
            .state
            .lock()
            .unwrap()
            .route_faults
            .iter()
            .find(|(path, _)| *path == http.path)
            .map(|(_, fault)| *fault);
        if let Some(fault) = fault {
            let Some(error) = fault.route_error() else {
                std::future::pending::<()>().await;
                unreachable!("hang")
            };
            return Err(error);
        }
        let mut state = self.state.lock().unwrap();
        let overlaps = state.routes.iter().any(|route| {
            route.upstream_id == req.upstream_id()
                && route.match_rules.http.as_ref().is_some_and(|existing| {
                    existing.path == http.path && existing.methods.contains(&method)
                })
        });
        if overlaps {
            return Err(Fault::Conflict.route_error().unwrap());
        }
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: S2S_TENANT,
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            priority: req.priority(),
            enabled: true,
        };
        state.routes.push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        Err(unsupported("get_route"))
    }

    async fn list_routes(
        &self,
        ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        let upstream_id = upstream_id.expect("provisioning lists the routes of one upstream");
        self.record(&ctx, OagwCall::ListRoutes { upstream_id });
        let fault = self.state.lock().unwrap().list_routes_fault;
        if let Some(fault) = fault {
            let Some(error) = fault.route_error() else {
                std::future::pending::<()>().await;
                unreachable!("hang")
            };
            return Err(error);
        }
        let state = self.state.lock().unwrap();
        let routes: Vec<Route> = state
            .routes
            .iter()
            .filter(|route| route.upstream_id == upstream_id)
            .cloned()
            .collect();
        Ok(page(&routes, query))
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unsupported("update_route"))
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Err(unsupported("delete_route"))
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unsupported("resolve_proxy_target"))
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<oagw_sdk::Body>,
    ) -> Result<http::Response<oagw_sdk::Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body
            .into_bytes()
            .await
            .expect("the request body is buffered");
        let reply = {
            let mut state = self.state.lock().unwrap();
            let recorded = RecordedProxy {
                subject: ctx.subject_id(),
                tenant: ctx.subject_tenant_id(),
                method: parts.method.to_string(),
                uri: parts
                    .uri
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_string(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect(),
                body: body.to_vec(),
            };
            let handler = state
                .rules
                .iter()
                .find(|rule| rule.method == recorded.method && rule.matcher.matches(&recorded.uri))
                .map(|rule| Arc::clone(&rule.handler));
            state.proxied.push(recorded.clone());
            match handler {
                Some(handler) => {
                    drop(state);
                    Some(handler(&recorded))
                }
                None => state.proxy_queue.pop_front(),
            }
        };
        let Some(mut reply) = reply else {
            return Err(unsupported("proxy_request without a scripted reply"));
        };
        loop {
            match reply {
                ProxyReply::Delayed(delay, inner) => {
                    tokio::time::sleep(delay).await;
                    reply = *inner;
                }
                ProxyReply::Held(gate, inner) => {
                    gate.pass().await;
                    reply = *inner;
                }
                settled => {
                    reply = settled;
                    break;
                }
            }
        }
        match reply {
            ProxyReply::Error(fault) => Err(fault
                .upstream_error()
                .unwrap_or_else(|| unsupported("a hanging fault"))),
            ProxyReply::Pending => {
                let _guard = PendingGuard(Arc::clone(&self.pending_dropped));
                std::future::pending::<()>().await;
                unreachable!("a pending call never answers")
            }
            ProxyReply::Delayed(..) | ProxyReply::Held(..) => {
                unreachable!("delays and gates are settled before the match")
            }
            ProxyReply::Response {
                status,
                headers,
                body,
                source,
            } => {
                let mut builder = http::Response::builder().status(status);
                for (name, value) in &headers {
                    builder = builder.header(name, value);
                }
                let body = match body {
                    ReplyBody::Bytes(bytes) => {
                        oagw_sdk::Body::Stream(Box::pin(futures::stream::once(async move {
                            Ok(bytes::Bytes::from(bytes))
                        })))
                    }
                    ReplyBody::Feed(rx, state) => {
                        oagw_sdk::Body::Stream(Box::pin(FeedStream { rx, state }))
                    }
                };
                let mut response = builder.body(body).expect("a valid scripted response");
                if let Some(source) = source {
                    response.extensions_mut().insert(source);
                }
                Ok(response)
            }
        }
    }
}

// ---- catalog and plugins -----------------------------------------------------

/// Catalog entry JSON with every required field; `id` doubles as the model name.
pub fn entry_json(id: &str, enabled: bool) -> Value {
    json!({
        "id": id,
        "provider_model_id": format!("provider-{id}"),
        "display_name": format!("Model {id}"),
        "description": format!("About {id}"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "icon": "icon.svg",
        "tier": "premium",
        "enabled": enabled,
        "system_prompt": "SECRET-SYSTEM-PROMPT",
        "thread_summary_prompt": "SECRET-SUMMARY-PROMPT",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 2_500_000,
        "output_tokens_credit_multiplier_micro": 7_500_000,
        "multiplier_display": "2x",
        "max_num_results": 5,
        "preference": { "is_default": true, "sort_order": 3 },
        "general_config": {
            "type": "model.general.v1",
            "available_from": "2026-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": true, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": false, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        }
    })
}

/// Decodes a catalog entry.
pub fn entry(value: Value) -> ModelCatalogEntry {
    serde_json::from_value(value).expect("catalog entry")
}

/// How a read call of the fake policy plugin behaves.
#[derive(Clone)]
pub enum Behavior {
    Normal,
    Fail(PolicyPluginError),
    Hang,
}

/// How `publish_usage` of the fake policy plugin behaves.
#[derive(Clone)]
pub enum PublishBehavior {
    Ok,
    Transient,
    Permanent,
    Hang,
}

/// Fake model-policy plugin serving a fixed catalog at policy version 7.
pub struct FakePolicyPlugin {
    pub catalog: Mutex<Vec<ModelCatalogEntry>>,
    pub version: Mutex<Behavior>,
    pub snapshot: Mutex<Behavior>,
    /// Limits served by `get_user_limits`; `None` answers with an internal
    /// error ("not used").
    pub limits: Mutex<Option<UserLimits>>,
    pub limits_behavior: Mutex<Behavior>,
    pub publish: Mutex<PublishBehavior>,
    /// `(user_id, policy_version)` of every snapshot request.
    pub snapshot_requests: Mutex<Vec<(Uuid, u64)>>,
    pub published: Mutex<Vec<UsageEvent>>,
}

/// Policy version the fake plugin serves.
pub const POLICY_VERSION: u64 = 7;

impl FakePolicyPlugin {
    pub fn with_catalog(catalog: Vec<ModelCatalogEntry>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(catalog),
            version: Mutex::new(Behavior::Normal),
            snapshot: Mutex::new(Behavior::Normal),
            limits: Mutex::new(None),
            limits_behavior: Mutex::new(Behavior::Normal),
            publish: Mutex::new(PublishBehavior::Ok),
            snapshot_requests: Mutex::new(Vec::new()),
            published: Mutex::new(Vec::new()),
        })
    }

    pub fn empty() -> Arc<Self> {
        Self::with_catalog(Vec::new())
    }

    pub fn set_version(&self, behavior: Behavior) {
        *self.version.lock().unwrap() = behavior;
    }

    pub fn set_snapshot(&self, behavior: Behavior) {
        *self.snapshot.lock().unwrap() = behavior;
    }

    pub fn set_limits(&self, limits: Option<UserLimits>) {
        *self.limits.lock().unwrap() = limits;
    }

    pub fn set_limits_behavior(&self, behavior: Behavior) {
        *self.limits_behavior.lock().unwrap() = behavior;
    }

    pub fn set_publish(&self, behavior: PublishBehavior) {
        *self.publish.lock().unwrap() = behavior;
    }

    pub fn published_events(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicyPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        let behavior = self.version.lock().unwrap().clone();
        match behavior {
            Behavior::Normal => Ok(PolicyVersionInfo {
                policy_version: POLICY_VERSION,
                generated_at: OffsetDateTime::UNIX_EPOCH,
            }),
            Behavior::Fail(error) => Err(error),
            Behavior::Hang => std::future::pending().await,
        }
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        self.snapshot_requests
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        let behavior = self.snapshot.lock().unwrap().clone();
        match behavior {
            Behavior::Normal if policy_version == POLICY_VERSION => Ok(PolicySnapshot {
                policy_version,
                model_catalog: self.catalog.lock().unwrap().clone(),
                kill_switches: serde_json::from_value(json!({
                    "disable_premium_tier": false,
                    "force_standard_tier": false,
                    "disable_web_search": false,
                    "disable_file_search": false,
                    "disable_images": false,
                    "disable_code_interpreter": false
                }))
                .expect("kill switches"),
            }),
            Behavior::Normal => Err(PolicyPluginError::NotFound(format!(
                "version {policy_version}"
            ))),
            Behavior::Fail(error) => Err(error),
            Behavior::Hang => std::future::pending().await,
        }
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError> {
        let behavior = self.limits_behavior.lock().unwrap().clone();
        match behavior {
            Behavior::Normal => self
                .limits
                .lock()
                .unwrap()
                .ok_or_else(|| PolicyPluginError::Internal("not used".to_owned())),
            Behavior::Fail(error) => Err(error),
            Behavior::Hang => std::future::pending().await,
        }
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        let behavior = self.publish.lock().unwrap().clone();
        match behavior {
            PublishBehavior::Ok => {
                self.published.lock().unwrap().push(event);
                Ok(())
            }
            PublishBehavior::Transient => Err(PublishError::Transient("try later".to_owned())),
            PublishBehavior::Permanent => Err(PublishError::Permanent("bad event".to_owned())),
            PublishBehavior::Hang => std::future::pending().await,
        }
    }
}

/// How `emit` of the fake audit plugin behaves.
#[derive(Clone)]
pub enum EmitBehavior {
    Ok,
    Transient,
    PluginTimeout,
    Permanent,
    Hang,
}

/// Fake audit plugin.
pub struct FakeAuditPlugin {
    pub behavior: Mutex<EmitBehavior>,
    pub emitted: Mutex<Vec<MiniChatAuditEvent>>,
}

impl FakeAuditPlugin {
    pub fn new(behavior: EmitBehavior) -> Arc<Self> {
        Arc::new(Self {
            behavior: Mutex::new(behavior),
            emitted: Mutex::new(Vec::new()),
        })
    }

    pub fn emitted_events(&self) -> Vec<MiniChatAuditEvent> {
        self.emitted.lock().unwrap().clone()
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAuditPlugin {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        let behavior = self.behavior.lock().unwrap().clone();
        match behavior {
            EmitBehavior::Ok => {
                self.emitted.lock().unwrap().push(event);
                Ok(())
            }
            EmitBehavior::Transient => Err(AuditPluginError::Transient("later".to_owned())),
            EmitBehavior::PluginTimeout => Err(AuditPluginError::PluginTimeout),
            EmitBehavior::Permanent => Err(AuditPluginError::Permanent("never".to_owned())),
            EmitBehavior::Hang => std::future::pending().await,
        }
    }
}

/// `ClientHub` plus an in-memory types-registry, with plugin registration helpers.
pub struct PluginWorld {
    pub hub: Arc<ClientHub>,
    pub registry: Arc<FakeTypesRegistry>,
}

impl PluginWorld {
    pub fn new() -> Self {
        let registry = FakeTypesRegistry::new();
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(
            Arc::clone(&registry) as Arc<dyn TypesRegistryClient>
        );
        Self { hub, registry }
    }

    /// Registers the policy plugin instance and its `ClientHub` client.
    pub fn register_policy(&self, plugin: Arc<dyn MiniChatModelPolicyPluginClientV1>) {
        let id = self.add_policy_instance();
        self.hub
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&id),
                plugin,
            );
    }

    /// Registers only the registry instance (no `ClientHub` client).
    pub fn add_policy_instance(&self) -> String {
        let id = format!(
            "{}cf.core._.fake_policy.v1",
            MiniChatModelPolicyPluginSpecV1::gts_type_id()
        );
        self.registry
            .add_instance(&id, json!({ "id": id, "vendor": VENDOR, "priority": 100 }));
        id
    }

    /// Registers the audit plugin instance and its `ClientHub` client.
    pub fn register_audit(&self, plugin: Arc<dyn MiniChatAuditPluginClientV1>) {
        let id = self.add_audit_instance();
        self.hub
            .register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), plugin);
    }

    /// Registers only the registry instance (no `ClientHub` client).
    pub fn add_audit_instance(&self) -> String {
        let id = format!(
            "{}cf.core._.fake_audit.v1",
            MiniChatAuditPluginSpecV1::gts_type_id()
        );
        self.registry
            .add_instance(&id, json!({ "id": id, "vendor": VENDOR, "priority": 100 }));
        id
    }

    pub fn policy_gateway(&self) -> ModelPolicyGateway {
        ModelPolicyGateway::new(&self.hub, VENDOR)
    }

    pub fn audit_gateway(&self) -> AuditGateway {
        AuditGateway::new(&self.hub, VENDOR)
    }
}

/// A models service over the fake PDP and the plugin world.
pub fn models_service(pdp: &Arc<FakePdp>, gateway: ModelPolicyGateway) -> Arc<ModelsService> {
    Arc::new(ModelsService::new(
        Arc::new(AuthzService::new(
            Arc::clone(pdp) as Arc<dyn AuthZResolverApi>
        )),
        Arc::new(gateway),
    ))
}

// ---- metrics port ------------------------------------------------------------

/// Records the audit outcomes.
#[derive(Default)]
pub struct RecordingAuditMetrics {
    pub results: Mutex<Vec<AuditEmitResult>>,
}

impl RecordingAuditMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn recorded(&self) -> Vec<AuditEmitResult> {
        self.results.lock().unwrap().clone()
    }
}

impl AuditEmitMetrics for RecordingAuditMetrics {
    fn record_audit_emit(&self, result: AuditEmitResult) {
        self.results.lock().unwrap().push(result);
    }
}

// ---- payloads ----------------------------------------------------------------

/// A valid user-turn usage event.
pub fn usage_event() -> UsageEvent {
    let tenant = USER_TENANT;
    let turn = Uuid::from_u128(0x3333);
    let request = Uuid::from_u128(0x4444);
    UsageEvent {
        tenant_id: tenant,
        user_id: Some(USER),
        chat_id: Uuid::from_u128(0x2222),
        turn_id: Some(turn),
        request_id: request,
        effective_model: "gpt-5.2".to_owned(),
        selected_model: "gpt-5.2".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: mini_chat_sdk::BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 1_000,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::User,
        dedupe_key: usage_dedupe_key(tenant, turn, request),
        system_task_type: None,
    }
}

pub fn usage_payload() -> Vec<u8> {
    encode_usage_event(&usage_event()).expect("encode usage")
}

/// A valid audit event.
pub fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
        tenant_id: USER_TENANT,
        actor_user_id: USER,
        chat_id: Uuid::from_u128(0xC1),
        request_id: Uuid::from_u128(0x91),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    })
}

pub fn audit_payload() -> Vec<u8> {
    encode_audit_event(&audit_event()).expect("encode audit")
}

/// An outbox message delivered for the `attempts`-th time (0 on the first).
pub fn outbox_message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

// ---- provider registry ---------------------------------------------------------

/// Plugin type of the API-key auth used by the fixtures.
pub const APIKEY_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Raw provider entry JSON with API-key auth; `storage_kind` is `openai` or `azure`.
pub fn provider_json(host: &str, storage_kind: &str) -> Value {
    let mut entry = json!({
        "kind": "openai_responses",
        "host": host,
        "storage_kind": storage_kind,
        "auth_plugin_type": APIKEY_PLUGIN,
        "auth_config": {
            "header": "Authorization",
            "prefix": "Bearer ",
            "secret_ref": "cred://openai-key"
        }
    });
    if storage_kind == "azure" {
        entry["api_version"] = json!("2025-01-01-preview");
    }
    entry
}

/// Validates a raw `providers` JSON object (keys in configuration order).
pub fn registry(providers: &Value) -> crate::config::ProviderRegistry {
    let map: crate::config::ProviderMap =
        serde_json::from_value(providers.clone()).expect("provider map");
    crate::config::validate_providers(Some(&map)).expect("valid providers")
}

/// Like [`registry`] but keeps the given order of the entries (a JSON object
/// built with `json!` is sorted by key).
pub fn registry_ordered(entries: &[(&str, Value)]) -> crate::config::ProviderRegistry {
    let body: Vec<String> = entries
        .iter()
        .map(|(id, entry)| {
            format!(
                "{}:{}",
                serde_json::to_string(id).unwrap(),
                serde_json::to_string(entry).unwrap()
            )
        })
        .collect();
    let map: crate::config::ProviderMap =
        serde_json::from_str(&format!("{{{}}}", body.join(","))).expect("provider map");
    crate::config::validate_providers(Some(&map)).expect("valid providers")
}

/// Client credentials of the fixtures.
pub fn credentials() -> crate::config::ClientCredentials {
    crate::config::ClientCredentials {
        client_id: "mini-chat-client".to_owned(),
        client_secret: crate::config::SecretValue::new("S3CRET-SENTINEL"),
    }
}
