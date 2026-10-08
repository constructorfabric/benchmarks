//! REST app harness: the gear's router (`register_routes`) over a per-test
//! `SQLite` file, a fake model policy, a fake audit port and a fake PDP.
//! Requests are sent with an injected `SecurityContext` (the api-gateway's
//! job in production).
//!
//! The outbox pipeline runs for the whole test. By default its handlers are
//! no-op `RetryHandler`s, so enqueued rows stay queued and can be read with
//! [`TestApp::outbox_payloads`]; [`TestAppBuilder::real_handlers`] starts it
//! with the gear's real handlers instead (`start_pipeline`).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit::api::{OpenApiRegistry, OperationSpec};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::PolicyEnforcer;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::clock::FixedClock;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::services::{AppDeps, AppServices, IndexingTimings};
use mini_chat::infra::metrics::MiniChatMetrics;
use mini_chat::infra::outbox::enqueuer::OutboxEnqueuer;
use mini_chat::infra::outbox::{HandlerDeps, register_queues, start_pipeline};
use mini_chat::infra::s2s::S2sContextProvider;
use mini_chat::test_support::FakeOagw;
use sea_orm::DatabaseConnection;
use toolkit_db::outbox::{Outbox, OutboxHandle};

use super::fakes::{
    FakeAudit, FakePdp, FakePolicy, default_limits, premium_model, standard_model,
    standard_no_vision,
};
use super::{TestDb, test_db_with_raw};

/// Fixed start time of the harness clock.
pub const CLOCK_START: i64 = 1_760_000_000;

/// Subject of the fixed S2S security context the services use for OAGW calls.
pub const S2S_SUBJECT_ID: Uuid = Uuid::from_u128(0x5250_0000_0000_4000_8000_0000_0000_0001);
/// Tenant of the fixed S2S security context.
pub const S2S_TENANT_ID: Uuid = Uuid::from_u128(0x5250_0000_0000_4000_8000_0000_0000_0002);

type ConfigFn = Box<dyn FnOnce(&mut MiniChatConfig)>;

pub struct TestApp {
    pub router: Router,
    pub db: Arc<DBProvider<DomainError>>,
    pub policy: Arc<FakePolicy>,
    /// Audit port of the real handlers ([`TestAppBuilder::real_handlers`]).
    pub audit: Arc<FakeAudit>,
    pub pdp: Arc<FakePdp>,
    pub clock: Arc<FixedClock>,
    pub config: Arc<MiniChatConfig>,
    pub services: Arc<AppServices>,
    /// Outbox enqueuer of the services (installed on the started pipeline).
    pub outbox: Arc<OutboxEnqueuer>,
    /// Raw connection to the same database (raw SQL in tests only).
    pub raw: DatabaseConnection,
    /// OAGW used by the services (provider calls), with a fixed S2S context.
    pub oagw: Arc<FakeOagw>,
    _outbox_handle: OutboxHandle,
    openapi: Arc<RecordingOpenApi>,
    /// Keeps the database files alive (deleted on drop, after the fields above).
    _files: TestDb,
}

pub struct TestAppBuilder {
    catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
    limits: (TierLimits, TierLimits),
    config_fns: Vec<ConfigFn>,
    sse_ping_interval: Option<std::time::Duration>,
    indexing: IndexingTimings,
    real_handlers: bool,
    metrics: Option<Arc<MiniChatMetrics>>,
}

/// Millisecond indexing waits for tests (deadline 1 s, rounds of 100 ms,
/// background limit 1.5 s). The deadline leaves room for the first DB writes
/// of a test, which can queue behind the outbox workers' startup burst.
pub fn fast_timings() -> IndexingTimings {
    use std::time::Duration;
    let ms = Duration::from_millis;
    IndexingTimings {
        deadline: ms(1000),
        poll_initial: ms(5),
        poll_max: ms(20),
        bg_round: ms(100),
        bg_poll_max: ms(20),
        bg_total: ms(1500),
        ready_retry_delays: [ms(5), ms(10), ms(20)],
        vector_store_poll_initial: ms(5),
    }
}

/// Minimal valid gear configuration (defaults everywhere).
pub fn test_config() -> MiniChatConfig {
    let mut cfg: MiniChatConfig = serde_json::from_value(serde_json::json!({
        "client_credentials": {"client_id": "mini-chat-test", "client_secret": "secret"}
    }))
    .expect("test config");
    cfg.validate().expect("valid test config");
    cfg
}

impl TestApp {
    /// Defaults: catalog `[premium_model("p1"), standard_model("s1"),
    /// standard_no_vision("s-novision")]`, no kill switches, generous limits.
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder {
            catalog: vec![
                premium_model("p1"),
                standard_model("s1"),
                standard_no_vision("s-novision"),
            ],
            kill_switches: KillSwitches::default(),
            limits: default_limits(),
            config_fns: Vec::new(),
            sse_ping_interval: None,
            indexing: fast_timings(),
            real_handlers: false,
            metrics: None,
        }
    }

    /// Client acting as `user_id` in `tenant_id`.
    pub fn as_user(&self, user_id: Uuid, tenant_id: Uuid) -> UserClient<'_> {
        let ctx = SecurityContext::builder()
            .subject_id(user_id)
            .subject_tenant_id(tenant_id)
            .build()
            .expect("security context");
        UserClient { app: self, ctx }
    }

    /// JSON payloads of every outbox message still queued on `queue`
    /// (with the default no-op handlers: every message enqueued so far).
    pub async fn outbox_payloads(&self, queue: &str) -> Vec<Value> {
        use sea_orm::{ConnectionTrait, Statement};
        let sql = "SELECT p.queue, CAST(b.payload AS TEXT) FROM ( \
                     SELECT partition_id, body_id FROM toolkit_outbox_incoming \
                     UNION ALL SELECT partition_id, body_id FROM toolkit_outbox_outgoing) m \
                   JOIN toolkit_outbox_partitions p ON p.id = m.partition_id \
                   JOIN toolkit_outbox_body b ON b.id = m.body_id";
        self.raw
            .query_all_raw(Statement::from_string(self.raw.get_database_backend(), sql))
            .await
            .expect("outbox query")
            .iter()
            .filter_map(|r| {
                let q: String = r.try_get_by_index(0).expect("queue");
                let payload: String = r.try_get_by_index(1).expect("payload");
                (q == queue).then(|| serde_json::from_str(&payload).expect("JSON payload"))
            })
            .collect()
    }

    /// Operations registered by `register_routes` (`OpenAPI` specs).
    pub fn operations(&self) -> Vec<OperationSpec> {
        self.openapi.specs.lock().unwrap().clone()
    }
}

impl TestAppBuilder {
    pub fn catalog(mut self, catalog: Vec<ModelCatalogEntry>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn kill_switches(mut self, ks: KillSwitches) -> Self {
        self.kill_switches = ks;
        self
    }

    /// Per-tier limits `(standard, premium)`.
    pub fn limits(mut self, standard: TierLimits, premium: TierLimits) -> Self {
        self.limits = (standard, premium);
        self
    }

    /// Override the SSE ping interval (config allows 5..=60 s only).
    pub fn sse_ping_interval(mut self, interval: std::time::Duration) -> Self {
        self.sse_ping_interval = Some(interval);
        self
    }

    /// Override the indexing waits (default [`fast_timings`]).
    pub fn indexing_timings(mut self, t: IndexingTimings) -> Self {
        self.indexing = t;
        self
    }

    /// Start the outbox pipeline with the gear's real handlers
    /// (`start_pipeline`: usage → [`TestApp::policy`], audit →
    /// [`TestApp::audit`], cleanup → the services' `CleanupService`, thread
    /// summary → the services' `ThreadSummaryService`) instead of the no-op
    /// `RetryHandler`s.
    pub fn real_handlers(mut self) -> Self {
        self.real_handlers = true;
        self
    }

    /// Record on `metrics` instead of no-op instruments (see
    /// [`super::MetricsRecorder`]).
    pub fn metrics(mut self, metrics: Arc<MiniChatMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Adjust the gear configuration (applied after defaults, then validated).
    pub fn config(mut self, f: impl FnOnce(&mut MiniChatConfig) + 'static) -> Self {
        self.config_fns.push(Box::new(f));
        self
    }

    pub async fn build(self) -> TestApp {
        let mut cfg = test_config();
        for f in self.config_fns {
            f(&mut cfg);
        }
        cfg.validate().expect("valid test config");
        let config = Arc::new(cfg);

        let (files, raw) = test_db_with_raw().await;
        let db = files.provider();
        let outbox = Arc::new(OutboxEnqueuer::new(config.outbox.clone()));
        let audit = Arc::new(FakeAudit::default());
        let policy = Arc::new(FakePolicy::new(
            self.catalog,
            self.kill_switches,
            self.limits,
        ));
        let pdp = Arc::new(FakePdp::default());
        let clock = Arc::new(FixedClock::new(
            OffsetDateTime::from_unix_timestamp(CLOCK_START).expect("clock start"),
        ));

        let oagw = Arc::new(FakeOagw::new());
        let s2s_ctx = SecurityContext::builder()
            .subject_id(S2S_SUBJECT_ID)
            .subject_tenant_id(S2S_TENANT_ID)
            .build()
            .expect("s2s security context");

        let mut services = AppServices::new(AppDeps {
            config: Arc::clone(&config),
            db: Arc::clone(&db),
            clock: clock.clone(),
            policy: policy.clone(),
            enforcer: PolicyEnforcer::new(pdp.clone()),
            outbox: outbox.clone(),
            oagw: oagw.clone(),
            s2s: Arc::new(S2sContextProvider::fixed(s2s_ctx)),
            indexing: self.indexing,
            shutdown: tokio_util::sync::CancellationToken::new(),
            metrics: self
                .metrics
                .unwrap_or_else(|| Arc::new(MiniChatMetrics::noop())),
        });
        if let Some(interval) = self.sse_ping_interval {
            services.sse_ping_interval = interval;
        }
        let services = Arc::new(services);

        // Like the gear's `start`: the pipeline starts after the services
        // and is then installed on their enqueuer.
        let outbox_handle = if self.real_handlers {
            let deps = HandlerDeps {
                policy: policy.clone(),
                audit: audit.clone(),
                cleanup: Arc::clone(&services.cleanup),
                thread_summary: services.summaries.clone(),
                metrics: Arc::clone(&services.metrics),
            };
            start_pipeline(db.db(), &config, deps).await
        } else {
            register_queues(Outbox::builder(db.db()), &config.outbox)
                .start()
                .await
                .map_err(anyhow::Error::from)
        }
        .expect("outbox pipeline starts");
        outbox
            .set_outbox(Arc::clone(outbox_handle.outbox()))
            .expect("outbox installed once");

        let openapi = Arc::new(RecordingOpenApi::default());
        let router = mini_chat::api::rest::routes::register_routes(
            Router::new(),
            openapi.as_ref(),
            Arc::clone(&services),
        );

        TestApp {
            router,
            db,
            policy,
            audit,
            pdp,
            clock,
            config,
            services,
            outbox,
            raw,
            oagw,
            _outbox_handle: outbox_handle,
            openapi,
            _files: files,
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI registry that records specs
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingOpenApi {
    inner: OpenApiRegistryImpl,
    specs: Mutex<Vec<OperationSpec>>,
}

impl OpenApiRegistry for RecordingOpenApi {
    fn register_operation(&self, spec: &OperationSpec) {
        self.specs.lock().unwrap().push(spec.clone());
        self.inner.register_operation(spec);
    }

    fn ensure_schema_raw(
        &self,
        name: &str,
        schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        self.inner.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Client + response
// ---------------------------------------------------------------------------

pub struct UserClient<'a> {
    app: &'a TestApp,
    pub ctx: SecurityContext,
}

/// One multipart part.
pub struct MultipartPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

impl MultipartPart {
    pub fn file(name: &str, filename: &str, content_type: &str, data: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.to_owned(),
            filename: Some(filename.to_owned()),
            content_type: Some(content_type.to_owned()),
            data: data.into(),
        }
    }
}

const BOUNDARY: &str = "mini-chat-test-boundary";

impl UserClient<'_> {
    /// Send a prepared request (the `SecurityContext` is injected).
    pub async fn send(&self, mut req: Request<Body>) -> TestResponse {
        req.extensions_mut().insert(self.ctx.clone());
        let resp = self
            .app
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        TestResponse {
            status,
            headers,
            body,
        }
    }

    fn request(method: Method, path: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(path)
    }

    /// POST JSON and return the response with its body still streaming.
    pub async fn open_stream(&self, path: &str, body: &Value) -> LiveResponse {
        let mut req = Self::request(Method::POST, path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(self.ctx.clone());
        let resp = self
            .app
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        LiveResponse {
            status: resp.status(),
            headers: resp.headers().clone(),
            body: Box::pin(resp.into_body().into_data_stream()),
            buf: String::new(),
        }
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.send(
            Self::request(Method::GET, path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    pub async fn delete(&self, path: &str) -> TestResponse {
        self.send(
            Self::request(Method::DELETE, path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn json(&self, method: Method, path: &str, body: &Value) -> TestResponse {
        self.send(
            Self::request(method, path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    pub async fn post_json(&self, path: &str, body: &Value) -> TestResponse {
        self.json(Method::POST, path, body).await
    }

    pub async fn patch_json(&self, path: &str, body: &Value) -> TestResponse {
        self.json(Method::PATCH, path, body).await
    }

    pub async fn put_json(&self, path: &str, body: &Value) -> TestResponse {
        self.json(Method::PUT, path, body).await
    }

    pub async fn post_multipart(&self, path: &str, parts: Vec<MultipartPart>) -> TestResponse {
        let mut body = Vec::new();
        for p in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            let filename = p
                .filename
                .as_ref()
                .map(|f| format!("; filename=\"{f}\""))
                .unwrap_or_default();
            let disposition = format!(
                "Content-Disposition: form-data; name=\"{}\"{filename}",
                p.name
            );
            body.extend_from_slice(disposition.as_bytes());
            body.extend_from_slice(b"\r\n");
            if let Some(ct) = &p.content_type {
                body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(&p.data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        self.send(
            Self::request(Method::POST, path)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }
}

/// A response whose body is read incrementally (SSE frame by frame).
pub struct LiveResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    body: futures::stream::BoxStream<'static, Result<Bytes, axum::Error>>,
    buf: String,
}

/// Default wait for one SSE event.
pub const EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl LiveResponse {
    /// Next SSE event (comment-only frames skipped); `None` at end of body.
    /// Panics when no event arrives within [`EVENT_TIMEOUT`].
    pub async fn next_event(&mut self) -> Option<(String, Value)> {
        self.next_event_within(EVENT_TIMEOUT)
            .await
            .expect("timed out waiting for an SSE event")
    }

    /// Next SSE event within `timeout` (`Err` on timeout).
    pub async fn next_event_within(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<(String, Value)>, tokio::time::error::Elapsed> {
        use futures::StreamExt;
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(idx) = self.buf.find("\n\n") {
                    let frame: String = self.buf.drain(..idx + 2).collect();
                    if let Some(ev) = parse_sse_frame(&frame) {
                        return Some(ev);
                    }
                    continue;
                }
                match self.body.next().await {
                    Some(Ok(bytes)) => self
                        .buf
                        .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n")),
                    Some(Err(e)) => panic!("body read failed: {e}"),
                    None => return None,
                }
            }
        })
        .await
    }

    /// All remaining events until the body ends.
    pub async fn rest(mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Some(ev) = self.next_event().await {
            out.push(ev);
        }
        out
    }

    /// Whole body as JSON (non-SSE responses).
    pub async fn json(self) -> Value {
        use futures::StreamExt;
        let chunks: Vec<Bytes> = self.body.map(|c| c.expect("body chunk")).collect().await;
        serde_json::from_slice(&chunks.concat()).expect("JSON body")
    }
}

/// One SSE frame as `(event, data)` (`None` for comment-only frames).
fn parse_sse_frame(frame: &str) -> Option<(String, Value)> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if event.is_none() && data.is_empty() {
        return None;
    }
    let raw = data.join("\n");
    let value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
    Some((event.unwrap_or_else(|| "message".to_owned()), value))
}

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl TestResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    }

    /// SSE frames as `(event, data)`; frames without `event:` are `"message"`,
    /// comment-only frames (pings) are skipped, non-JSON data becomes a string.
    pub fn sse_events(&self) -> Vec<(String, Value)> {
        let text = self.text().replace("\r\n", "\n");
        text.split("\n\n").filter_map(parse_sse_frame).collect()
    }
}
