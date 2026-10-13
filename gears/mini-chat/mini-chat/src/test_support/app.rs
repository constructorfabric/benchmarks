//! `TestApp`: the gear's real service graph and router over in-memory fakes.
//!
//! [`TestApp::builder`] assembles the services with [`crate::wiring::build_services`], exactly like
//! the gear does, but with:
//! - a fresh migrated private temp-file `SQLite` database in WAL mode ([`test_db`]),
//! - a [`FakePdp`] (default [`PdpMode::TenantConstraint`]),
//! - a [`FakeGateway`] as the OAGW client (unscripted: every proxy call is a gateway 404 until a
//!   test installs responders on [`TestApp::gateway`]),
//! - `DirectPolicyGateway` over a [`RecordingPolicy`] serving the builder's catalog (default
//!   [`test_catalog`]), kill switches (default all off) and limits (default
//!   [`STANDARD_LIMITS`] / [`PREMIUM_LIMITS`]) as policy version 1,
//! - `DirectAuditGateway` over a [`RecordingAudit`],
//! - [`test_config`] (config defaults with a single `openai` provider pointing at
//!   `http://127.0.0.1:9`) unless [`TestAppBuilder::config`] replaces it.
//!
//! The S2S context is set to the fixed [`s2s_security_context`], as `serve` would after the
//! client-credentials exchange.
//!
//! The outbox pipeline runs for real (idle interval [`OUTBOX_IDLE_INTERVAL`]) with the default
//! handlers, each wrapped in a [`RecordingHandler`]: [`TestApp::outbox_payloads`] returns what a
//! queue received. [`TestAppBuilder::outbox_handler`] replaces the handler of one queue. The
//! pipeline stops when the `TestApp` is dropped (or on [`TestApp::shutdown`]).
//!
//! [`TestApp::stream`] sends a request answered with SSE and collects its events
//! ([`SseFrame`]); [`TestApp::open_stream`] returns an [`SseReader`] that yields the events as
//! they arrive (with receipt instants) and disconnects the client when dropped.
//!
//! Requests go through the router returned by `api::routes::register_routes` wrapped with
//! `toolkit::api::canonical_error_middleware`, as behind the api-gateway; the caller's
//! `SecurityContext` is inserted as a request extension (what the gateway auth layer does).
//!
//! ```ignore
//! let app = TestApp::builder().pdp(PdpMode::Deny).build().await;
//! let res = app.call("GET", "/mini-chat/v1/models", &ctx(tenant, user), None).await;
//! assert_eq!(res.status, 403);
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Request, Response, StatusCode};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde_json::Value;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::client_hub::ClientHub;
use toolkit_db::outbox::{LeasedMessageHandler, OutboxHandle};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use super::authn::s2s_security_context;
use super::catalog::test_catalog;
use super::db::{TestDb, test_db};
use super::gateway::FakeGateway;
use super::outbox::{RecordedPayloads, RecordingHandler};
use super::pdp::{FakePdp, PdpMode};
pub use super::plugins::{RecordingAudit, RecordingPolicy};
use crate::api::state::AppServices;
use crate::config::MiniChatConfig;
use crate::config::providers::{ProviderEntry, ProviderKind, StorageKind};
use crate::domain::attachment::IndexingTimings;
use crate::infra::gateways::audit::DirectAuditGateway;
use crate::infra::gateways::policy::DirectPolicyGateway;
use crate::infra::outbox::{LoggingAckHandler, OutboxHandlers, QueueKind};
use crate::metrics::Metrics;
use crate::wiring::{ServiceDeps, build_services, default_outbox_handlers, start_outbox_pipeline};

/// Idle polling interval of the outbox workers in tests.
pub const OUTBOX_IDLE_INTERVAL: Duration = Duration::from_millis(50);

/// Default standard-tier limits of the test policy.
pub const STANDARD_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 100_000_000,
    limit_monthly_credits_micro: 1_000_000_000,
};

/// Default premium-tier limits of the test policy.
pub const PREMIUM_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 50_000_000,
    limit_monthly_credits_micro: 500_000_000,
};

/// All kill switches off.
pub const NO_KILL_SWITCHES: KillSwitches = KillSwitches {
    disable_premium_tier: false,
    force_standard_tier: false,
    disable_web_search: false,
    disable_file_search: false,
    disable_images: false,
    disable_code_interpreter: false,
};

/// `SecurityContext` of user `user` in tenant `tenant`.
pub fn ctx(tenant: Uuid, user: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .build()
        .expect("test security context")
}

/// The `openai` provider entry used by the test catalog: `openai_responses` at
/// `http://127.0.0.1:9` with `openai` storage and no auth plugin.
pub fn test_provider() -> ProviderEntry {
    ProviderEntry {
        kind: ProviderKind::OpenaiResponses,
        host: "127.0.0.1".to_owned(),
        port: Some(9),
        use_http: true,
        upstream_alias: None,
        api_path: "/v1/responses".to_owned(),
        auth_plugin_type: None,
        auth_config: None,
        storage_kind: Some(StorageKind::Openai),
        storage_backend: None,
        api_version: None,
        rag_provider: None,
        tenant_overrides: BTreeMap::new(),
    }
}

/// OAGW alias of [`anthropic_provider`].
pub const ANTHROPIC_ALIAS: &str = "anthropic.test";
/// OAGW alias of [`azure_provider`].
pub const AZURE_ALIAS: &str = "azure.test";
/// `api_version` of [`azure_provider`].
pub const AZURE_API_VERSION: &str = "2025-03-01-preview";

/// An `anthropic_messages` entry at alias [`ANTHROPIC_ALIAS`] (`/v1/messages`) whose files and
/// vector stores go to the `openai` entry (`rag_provider`).
pub fn anthropic_provider() -> ProviderEntry {
    ProviderEntry {
        kind: ProviderKind::AnthropicMessages,
        host: ANTHROPIC_ALIAS.to_owned(),
        port: None,
        use_http: false,
        api_path: "/v1/messages".to_owned(),
        storage_kind: None,
        rag_provider: Some("openai".to_owned()),
        ..test_provider()
    }
}

/// An Azure `openai_responses` entry at alias [`AZURE_ALIAS`] with `azure` storage and
/// [`AZURE_API_VERSION`] (the knowledge search provider).
pub fn azure_provider() -> ProviderEntry {
    ProviderEntry {
        host: AZURE_ALIAS.to_owned(),
        port: None,
        use_http: false,
        api_path: "/openai/v1/responses".to_owned(),
        storage_kind: Some(StorageKind::Azure),
        api_version: Some(AZURE_API_VERSION.to_owned()),
        ..test_provider()
    }
}

/// [`test_config`] plus the `anthropic` entry ([`anthropic_provider`]).
pub fn anthropic_config() -> MiniChatConfig {
    let mut cfg = test_config();
    cfg.providers
        .insert("anthropic".to_owned(), anthropic_provider());
    cfg
}

/// Config defaults with `providers = { openai: test_provider() }`.
pub fn test_config() -> MiniChatConfig {
    MiniChatConfig {
        providers: BTreeMap::from([("openai".to_owned(), test_provider())]),
        ..MiniChatConfig::default()
    }
}

/// Response of [`TestApp::call`].
#[derive(Debug)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// The body parsed as JSON; `Null` for an empty body, a JSON string for a non-JSON body.
    pub json: Value,
}

/// Builder of [`TestApp`]; every setting has a default (see the module docs).
pub struct TestAppBuilder {
    config: MiniChatConfig,
    catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
    standard: TierLimits,
    premium: TierLimits,
    pdp: PdpMode,
    outbox_handlers: Vec<(QueueKind, Arc<dyn LeasedMessageHandler>)>,
    indexing_timings: IndexingTimings,
    metrics: Option<Arc<Metrics>>,
}

impl TestAppBuilder {
    /// Replaces the whole configuration (start from [`test_config`] to keep the provider).
    #[must_use]
    pub fn config(mut self, config: MiniChatConfig) -> Self {
        self.config = config;
        self
    }

    /// Model catalog served by the policy plugin, in order.
    #[must_use]
    pub fn catalog(mut self, catalog: Vec<ModelCatalogEntry>) -> Self {
        self.catalog = catalog;
        self
    }

    #[must_use]
    pub fn kill_switches(mut self, kill_switches: KillSwitches) -> Self {
        self.kill_switches = kill_switches;
        self
    }

    /// Standard and premium tier limits returned for every user.
    #[must_use]
    pub fn limits(mut self, standard: TierLimits, premium: TierLimits) -> Self {
        self.standard = standard;
        self.premium = premium;
        self
    }

    #[must_use]
    pub fn pdp(mut self, mode: PdpMode) -> Self {
        self.pdp = mode;
        self
    }

    /// Waits and deadlines of document indexing (default: the DESIGN values).
    #[must_use]
    pub fn indexing_timings(mut self, timings: IndexingTimings) -> Self {
        self.indexing_timings = timings;
        self
    }

    /// Records the services' metrics into `metrics` (e.g. a
    /// [`MetricsProbe`](super::metrics::MetricsProbe)'s instruments).
    #[must_use]
    pub fn metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Replaces the handler of one outbox queue (the payloads it receives are still recorded).
    #[must_use]
    pub fn outbox_handler(
        mut self,
        kind: QueueKind,
        handler: Arc<dyn LeasedMessageHandler>,
    ) -> Self {
        self.outbox_handlers.push((kind, handler));
        self
    }

    /// Makes both cleanup queues acknowledge without processing, for tests that assert on the
    /// state an enqueue leaves behind (`cleanup_status = pending`, the events delivered) and must
    /// not race the real cleanup handlers.
    #[must_use]
    pub fn quiet_cleanup(self) -> Self {
        self.outbox_handler(QueueKind::AttachmentCleanup, Arc::new(LoggingAckHandler))
            .outbox_handler(QueueKind::ChatCleanup, Arc::new(LoggingAckHandler))
    }

    /// Builds the services, starts the outbox pipeline and builds the router.
    pub async fn build(self) -> TestApp {
        let cfg = Arc::new(self.config);
        let test_db = test_db().await;
        let db = test_db.db();
        let gateway = Arc::new(FakeGateway::new());
        let pdp = Arc::new(FakePdp::new(self.pdp));
        let usage = Arc::new(RecordingPolicy::new(
            self.catalog,
            self.kill_switches,
            self.standard,
            self.premium,
        ));
        let audit = Arc::new(RecordingAudit::new());

        let services = build_services(ServiceDeps {
            cfg: Arc::clone(&cfg),
            db: db.clone(),
            hub: Arc::new(ClientHub::new()),
            authz_client: pdp.clone(),
            gateway: gateway.clone(),
            policy: Arc::new(DirectPolicyGateway(usage.clone())),
            audit: Arc::new(DirectAuditGateway(audit.clone())),
            indexing_timings: self.indexing_timings,
            metrics: self.metrics,
        })
        .expect("build services");
        // What `serve` does after the client-credentials exchange.
        services.s2s.set(s2s_security_context());

        let recorded = Arc::new(RecordedPayloads::default());
        let handlers = self
            .outbox_handlers
            .into_iter()
            .fold(default_outbox_handlers(&services), |h, (kind, handler)| {
                h.with(kind, handler)
            });
        let handlers = wrap_recording(handlers, &cfg, &recorded);
        let outbox_handle =
            start_outbox_pipeline(&services, db.clone(), handlers, Some(OUTBOX_IDLE_INTERVAL))
                .await
                .expect("start outbox pipeline");

        let router = crate::api::routes::register_routes(
            Router::new(),
            &OpenApiRegistryImpl::new(),
            Arc::clone(&services),
            &cfg.url_prefix,
        )
        .layer(axum::middleware::from_fn(
            toolkit::api::canonical_error_middleware,
        ));

        TestApp {
            router,
            services,
            db,
            gateway,
            audit,
            usage,
            pdp,
            recorded,
            outbox_handle: std::sync::Mutex::new(Some(outbox_handle)),
            _test_db: test_db,
        }
    }
}

/// Wraps every handler in a [`RecordingHandler`] keyed by its configured queue name.
fn wrap_recording(
    handlers: OutboxHandlers,
    cfg: &MiniChatConfig,
    recorded: &Arc<RecordedPayloads>,
) -> OutboxHandlers {
    handlers.map(|kind, inner| {
        Arc::new(RecordingHandler::new(
            kind.queue_name(&cfg.outbox),
            inner,
            Arc::clone(recorded),
        ))
    })
}

/// The gear under test; see the module docs.
pub struct TestApp {
    pub router: Router,
    pub services: Arc<AppServices>,
    /// The same database the services use.
    #[allow(dead_code)] // seeded / inspected directly by the chat and stream tests (Task 8+)
    pub db: toolkit_db::Db,
    #[allow(dead_code)] // scripted by the provider and storage tests (Task 10+)
    pub gateway: Arc<FakeGateway>,
    /// Audit plugin behind the audit gateway.
    #[allow(dead_code)] // asserted by the finalization and mutation tests (Task 15+)
    pub audit: Arc<RecordingAudit>,
    /// Model policy plugin behind the policy gateway (catalog, limits, published usage).
    pub usage: Arc<RecordingPolicy>,
    pub pdp: Arc<FakePdp>,
    recorded: Arc<RecordedPayloads>,
    outbox_handle: std::sync::Mutex<Option<OutboxHandle>>,
    /// Owns the database file; declared last so it is removed after everything above is dropped.
    _test_db: TestDb,
}

impl TestApp {
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder {
            config: test_config(),
            catalog: test_catalog(),
            kill_switches: NO_KILL_SWITCHES,
            standard: STANDARD_LIMITS,
            premium: PREMIUM_LIMITS,
            pdp: PdpMode::TenantConstraint,
            outbox_handlers: Vec::new(),
            indexing_timings: IndexingTimings::default(),
            metrics: None,
        }
    }

    /// Payloads delivered to the outbox queue named `queue`, decoded from JSON, in delivery order
    /// (a redelivery appears again).
    pub fn outbox_payloads(&self, queue: &str) -> Vec<Value> {
        self.recorded.payloads(queue)
    }

    /// Stops the outbox pipeline and waits for its workers. Dropping the `TestApp` cancels them
    /// without waiting.
    #[allow(dead_code)] // for tests that restart or inspect the database after the pipeline stops
    pub async fn shutdown(&self) {
        let handle = self
            .outbox_handle
            .lock()
            .expect("outbox handle lock")
            .take();
        if let Some(handle) = handle {
            handle.stop().await;
        }
    }

    /// Sends `method uri` as `ctx`, with `body` as JSON (`content-type: application/json`), and
    /// buffers the response.
    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<Value>,
    ) -> TestResponse {
        let resp = self.call_raw(json_request(method, uri, ctx, body)).await;
        buffer_response(resp).await
    }

    /// Sends a request expected to answer with an SSE stream and returns an incremental reader
    /// over its events; any status other than 200 is returned buffered as `Err`.
    #[allow(clippy::result_large_err)] // test helper; the rejection is inspected in place
    pub async fn open_stream(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Value,
    ) -> Result<SseReader, TestResponse> {
        let resp = self
            .call_raw(json_request(method, uri, ctx, Some(body)))
            .await;
        if resp.status() != StatusCode::OK {
            return Err(buffer_response(resp).await);
        }
        Ok(SseReader::new(resp))
    }

    /// Like [`Self::open_stream`], then reads every event until the body ends (panics after
    /// [`STREAM_READ_TIMEOUT`]).
    #[allow(clippy::result_large_err)] // test helper; the rejection is inspected in place
    pub async fn stream(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Value,
    ) -> Result<Vec<SseFrame>, TestResponse> {
        Ok(self
            .open_stream(method, uri, ctx, body)
            .await?
            .collect()
            .await)
    }

    /// Sends a prepared request (insert the `SecurityContext` extension yourself) and buffers the
    /// response like [`Self::call`].
    pub async fn call_buffered(&self, req: Request<Body>) -> TestResponse {
        buffer_response(self.call_raw(req).await).await
    }

    /// Sends a prepared request (insert the `SecurityContext` extension yourself).
    pub async fn call_raw(&self, req: Request<Body>) -> Response<Body> {
        self.router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible")
    }

    /// Polls `condition` (10 ms, doubling up to 100 ms) until it is true; panics naming `desc`
    /// after 5 s of **real** time. See [`Self::wait_until_within`].
    pub async fn wait_until<F, Fut>(desc: &str, condition: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        Self::wait_until_within(desc, Duration::from_secs(5), condition).await;
    }

    /// Like [`Self::wait_until`] with a custom timeout.
    ///
    /// The deadline is wall-clock (`std::time::Instant`); the poll interval uses tokio time, so it
    /// is instant with a paused clock. Paused-clock tests must not rely on timers while waiting on
    /// database progress: sqlx runs on its own OS thread, so the runtime looks idle and
    /// auto-advances the clock, firing any tokio timer (leases, heartbeats, timeouts) early.
    pub async fn wait_until_within<F, Fut>(desc: &str, timeout: Duration, mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + timeout;
        let mut delay = Duration::from_millis(10);
        loop {
            if condition().await {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting until {desc}"
            );
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(100));
        }
    }
}

/// Longest time [`TestApp::stream`] / [`SseReader::collect`] wait for the end of a stream.
pub const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// `method uri` as `ctx` with an optional JSON body.
fn json_request(
    method: &str,
    uri: &str,
    ctx: &SecurityContext,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(json) => {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).expect("serialize request body"))
        }
        None => Body::empty(),
    };
    let mut req = builder.body(body).expect("request");
    req.extensions_mut().insert(ctx.clone());
    req
}

/// Reads the whole body; JSON when it parses, else a JSON string, `Null` when empty.
async fn buffer_response(resp: Response<Body>) -> TestResponse {
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    TestResponse {
        status,
        headers,
        json,
    }
}

/// One SSE event: its name and its JSON `data`.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub event: String,
    pub data: Value,
}

/// Incremental reader over an SSE response body. Dropping it drops the body (a client
/// disconnect).
pub struct SseReader {
    pub headers: HeaderMap,
    body: Body,
    buf: String,
}

impl SseReader {
    fn new(resp: Response<Body>) -> Self {
        let headers = resp.headers().clone();
        Self {
            headers,
            body: resp.into_body(),
            buf: String::new(),
        }
    }

    /// The next event and the instant it was received; `None` once the body ended. Comment
    /// lines (keep-alives) are skipped.
    pub async fn next_frame(&mut self) -> Option<(SseFrame, std::time::Instant)> {
        use http_body_util::BodyExt as _;
        loop {
            while let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                if let Some(frame) = parse_block(&block) {
                    return Some((frame, std::time::Instant::now()));
                }
            }
            let frame = self.body.frame().await?.expect("read SSE body");
            if let Ok(data) = frame.into_data() {
                self.buf
                    .push_str(std::str::from_utf8(&data).expect("UTF-8 SSE body"));
            }
        }
    }

    /// Every remaining event until the body ends; panics after [`STREAM_READ_TIMEOUT`].
    pub async fn collect(mut self) -> Vec<SseFrame> {
        tokio::time::timeout(STREAM_READ_TIMEOUT, async {
            let mut frames = Vec::new();
            while let Some((frame, _)) = self.next_frame().await {
                frames.push(frame);
            }
            frames
        })
        .await
        .expect("SSE stream did not end in time")
    }
}

/// Parses one `event:` / `data:` block; `None` for a comment-only block.
fn parse_block(block: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if let Some(name) = line.strip_prefix("event:") {
            event = Some(name.trim().to_owned());
        } else if let Some(chunk) = line.strip_prefix("data:") {
            data.push(chunk.strip_prefix(' ').unwrap_or(chunk));
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    let data = data.join("\n");
    Some(SseFrame {
        event: event.unwrap_or_else(|| "message".to_owned()),
        data: serde_json::from_str(&data).unwrap_or(Value::String(data)),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;

    use super::*;
    use crate::test_support::catalog::standard_model;

    #[tokio::test]
    async fn builder_defaults_and_overrides() {
        let app = TestApp::builder().build().await;
        let user = Uuid::new_v4();
        let limits = app.usage.get_user_limits(user, 1).await.unwrap();
        assert_eq!(
            (limits.standard, limits.premium),
            (STANDARD_LIMITS, PREMIUM_LIMITS)
        );
        let snapshot = app.usage.get_policy_snapshot(user, 1).await.unwrap();
        assert_eq!(snapshot.model_catalog, test_catalog());
        assert_eq!(snapshot.kill_switches, NO_KILL_SWITCHES);
        let provider = &app.services.cfg.providers["openai"];
        assert_eq!(app.services.cfg.providers.len(), 1);
        assert_eq!(
            (provider.host.as_str(), provider.port, provider.use_http),
            ("127.0.0.1", Some(9), true)
        );
        assert_eq!(provider.kind, ProviderKind::OpenaiResponses);
        assert_eq!(provider.storage_kind, Some(StorageKind::Openai));

        let small = TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 2,
        };
        let switches = KillSwitches {
            disable_web_search: true,
            ..NO_KILL_SWITCHES
        };
        let mut config = test_config();
        config.url_prefix = "/chat".to_owned();
        let app = TestApp::builder()
            .catalog(vec![standard_model("only")])
            .kill_switches(switches)
            .limits(small, small)
            .config(config)
            .build()
            .await;
        let limits = app.usage.get_user_limits(user, 1).await.unwrap();
        assert_eq!((limits.standard, limits.premium), (small, small));
        let snapshot = app.usage.get_policy_snapshot(user, 1).await.unwrap();
        assert_eq!(snapshot.model_catalog.len(), 1);
        assert!(snapshot.kill_switches.disable_web_search);
        let res = app
            .call("GET", "/chat/v1/models", &ctx(user, user), None)
            .await;
        assert_eq!(res.status, StatusCode::OK, "routes follow url_prefix");
    }

    #[tokio::test]
    async fn wait_until_returns_once_condition_holds() {
        let polls = Arc::new(AtomicUsize::new(0));
        TestApp::wait_until("third poll", || {
            let polls = Arc::clone(&polls);
            async move { polls.fetch_add(1, Ordering::SeqCst) >= 2 }
        })
        .await;
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "timed out after 50ms waiting until never")]
    async fn wait_until_panics_after_timeout() {
        TestApp::wait_until_within("never", Duration::from_millis(50), || async { false }).await;
    }

    /// With a paused clock the runtime auto-advances while the condition depends on work outside
    /// tokio time (here an OS thread, in practice sqlx's worker): the deadline must be real time.
    #[tokio::test(start_paused = true)]
    async fn wait_until_deadline_is_real_time() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let setter = Arc::clone(&flag);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::SeqCst);
        });
        TestApp::wait_until("the OS thread set the flag", || {
            let flag = Arc::clone(&flag);
            async move { flag.load(Ordering::SeqCst) }
        })
        .await;
        worker.join().unwrap();
    }
}
