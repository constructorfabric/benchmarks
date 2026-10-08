//! In-process application: the real services and router over an in-memory
//! `SQLite` database (or a file-backed pooled one, see
//! [`TestAppBuilder::file_db`]), a mock PDP, the static model policy plugin and the real
//! outbox pipeline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::PolicyEnforcer;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request as AxumRequest;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use axum::middleware::{self, Next};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use secrecy::SecretString;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxHandle, OutboxMessage};
use toolkit_db::{DBProvider, Db};
use toolkit_security::SecurityContext;
use tower::ServiceExt;

use super::authz::{MockPdp, PdpMode};
use super::catalog::default_catalog;
use super::fake_provider::FakeProvider;
use super::users::TestUser;
use crate::api::rest::routes::register_routes;
use crate::config::MiniChatConfig;
use crate::domain::authz::ChatAuthz;
use crate::domain::services::{ServiceDeps, Services, UploadTimings, build_services};
use crate::infra::db::{TestDbDir, file_test_db, test_db};
use crate::infra::gateways::audit::{AuditGateway, InProcessAuditGateway};
use crate::infra::gateways::model_policy::{InProcessModelPolicyGateway, ModelPolicyGateway};
use crate::infra::llm::{LlmClient, ProviderResolver, RagClient};
use crate::infra::oagw::s2s::S2sContext;
use crate::infra::outbox::{OutboxEnqueuer, OutboxHandlers, QueueKind, start_outbox};
use crate::infra::plugins::static_audit::config::StaticAuditConfig;
use crate::infra::plugins::static_audit::service::StaticAuditService;
use crate::infra::plugins::static_model_policy::config::{
    KillSwitchesConfig, StaticModelPolicyConfig,
};
use crate::infra::plugins::static_model_policy::service::StaticModelPolicyService;
use crate::infra::workers::leader::{LeaderElector, NoopElector};

/// How long [`TestApp::outbox_messages`] waits for a first delivery.
const OUTBOX_WAIT: Duration = Duration::from_secs(5);

type ConfigFn = Box<dyn FnOnce(&mut MiniChatConfig) + Send>;

/// Builder of [`TestApp`].
pub struct TestAppBuilder {
    catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
    standard: TierLimits,
    premium: TierLimits,
    config: Vec<ConfigFn>,
    pdp: PdpMode,
    upload_timings: UploadTimings,
    elector: Arc<dyn LeaderElector>,
    file_db: Option<u32>,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        let defaults = StaticModelPolicyConfig::default();
        Self {
            catalog: default_catalog(),
            kill_switches: KillSwitchesConfig::default().into(),
            standard: defaults.default_standard_limits,
            premium: defaults.default_premium_limits,
            config: Vec::new(),
            pdp: PdpMode::Allow,
            upload_timings: UploadTimings::default(),
            elector: Arc::new(NoopElector),
            file_db: None,
        }
    }
}

impl TestAppBuilder {
    /// Model catalog served by the policy plugin (default: [`default_catalog`]).
    #[must_use]
    pub fn catalog(mut self, catalog: Vec<ModelCatalogEntry>) -> Self {
        self.catalog = catalog;
        self
    }

    /// Kill switches served by the policy plugin (default: all off).
    #[must_use]
    pub fn kill_switches(mut self, kill_switches: KillSwitches) -> Self {
        self.kill_switches = kill_switches;
        self
    }

    /// Per-user limits of the `total` (standard) and `tier:premium` buckets.
    #[must_use]
    pub fn limits(mut self, standard: TierLimits, premium: TierLimits) -> Self {
        self.standard = standard;
        self.premium = premium;
        self
    }

    /// Adjust the gear configuration (applied before `apply_defaults`/`validate`).
    #[must_use]
    pub fn config(mut self, f: impl FnOnce(&mut MiniChatConfig) + Send + 'static) -> Self {
        self.config.push(Box::new(f));
        self
    }

    /// Upload / indexing waits (default: the documented values).
    #[must_use]
    pub fn upload_timings(mut self, timings: UploadTimings) -> Self {
        self.upload_timings = timings;
        self
    }

    /// Leader elector of the orphan watchdog and the upload reaper (default:
    /// [`NoopElector`]). Only the spawned workers consult it; `scan_once` does not.
    #[must_use]
    pub fn elector(mut self, elector: Arc<dyn LeaderElector>) -> Self {
        self.elector = elector;
        self
    }

    /// Run on a file-backed `SQLite` database in WAL mode with a pool of
    /// `max_conns` connections, like the real server (default: an in-memory
    /// database with one connection, where transactions never contend).
    #[must_use]
    pub fn file_db(mut self, max_conns: u32) -> Self {
        self.file_db = Some(max_conns);
        self
    }

    /// Initial PDP behaviour (default [`PdpMode::Allow`]).
    #[must_use]
    pub fn pdp(mut self, mode: PdpMode) -> Self {
        self.pdp = mode;
        self
    }

    /// Wire the application exactly like the gear does and start the outbox.
    ///
    /// # Panics
    /// On an invalid configuration or catalog, or when the outbox cannot start.
    #[allow(clippy::expect_used)]
    pub async fn build(self) -> TestApp {
        let mut cfg = MiniChatConfig::default();
        "mini-chat-test".clone_into(&mut cfg.client_credentials.client_id);
        cfg.client_credentials.client_secret = SecretString::from("mini-chat-test-secret");
        for f in self.config {
            f(&mut cfg);
        }
        cfg.apply_defaults();
        cfg.validate().expect("valid test configuration");
        let config = Arc::new(cfg);

        let (db, db_dir) = match self.file_db {
            Some(max_conns) => {
                let (db, dir) = file_test_db(max_conns).await;
                (db, Some(dir))
            }
            None => (test_db().await, None),
        };

        let k = self.kill_switches;
        let plugin = StaticModelPolicyService::from_config(&StaticModelPolicyConfig {
            model_catalog: self.catalog,
            kill_switches: KillSwitchesConfig {
                disable_premium_tier: k.disable_premium_tier,
                force_standard_tier: k.force_standard_tier,
                disable_web_search: k.disable_web_search,
                disable_file_search: k.disable_file_search,
                disable_images: k.disable_images,
                disable_code_interpreter: k.disable_code_interpreter,
            },
            default_standard_limits: self.standard,
            default_premium_limits: self.premium,
            ..StaticModelPolicyConfig::default()
        })
        .expect("valid test catalog");
        let policy: Arc<dyn ModelPolicyGateway> =
            Arc::new(InProcessModelPolicyGateway::new(Arc::new(plugin)));

        let pdp = Arc::new(MockPdp::new(self.pdp));
        let authz = Arc::new(ChatAuthz::new(PolicyEnforcer::new(
            Arc::clone(&pdp) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>
        )));
        let enqueuer = Arc::new(OutboxEnqueuer::new(config.outbox.clone()));

        let provider = FakeProvider::new();
        let s2s = Arc::new(S2sContext::new());
        s2s.set(TestUser::S2S.security_context());
        let gw = Arc::clone(&provider) as Arc<dyn oagw_sdk::ServiceGatewayClientV1>;
        let llm = Arc::new(LlmClient::new(Arc::clone(&gw), Arc::clone(&s2s)));
        let rag = Arc::new(RagClient::new(gw, Arc::clone(&s2s)));
        let stop = CancellationToken::new();

        let services = build_services(ServiceDeps {
            config: Arc::clone(&config),
            db: Arc::new(DBProvider::new(db.clone())),
            authz,
            policy: Arc::clone(&policy),
            outbox: Arc::clone(&enqueuer),
            llm,
            s2s,
            rag,
            providers: Arc::new(ProviderResolver::new(&config)),
            upload_timings: self.upload_timings,
            stop: stop.clone(),
            elector: self.elector,
        });

        let recorder = Arc::new(OutboxRecorder::default());
        let audit: Arc<dyn AuditGateway> = Arc::new(InProcessAuditGateway::new(Arc::new(
            StaticAuditService::from_config(&StaticAuditConfig::default()),
        )));
        let handlers = OutboxHandlers::with_gateways(policy, audit)
            .with_thread_summary(Arc::clone(&services.thread_summary))
            .with_cleanup(Arc::clone(&services.cleanup))
            .map(|queue, inner| {
                Arc::new(RecordingHandler {
                    queue,
                    inner,
                    recorder: Arc::clone(&recorder),
                }) as Arc<dyn LeasedMessageHandler>
            });
        let outbox = start_outbox(db.clone(), &config, handlers)
            .await
            .expect("start outbox pipeline");
        enqueuer.set_outbox(Arc::clone(outbox.outbox()));

        let router = register_routes(
            Router::new(),
            &OpenApiRegistryImpl::new(),
            services.clone(),
            &config,
        )
        .layer(middleware::from_fn(inject_security_context));

        TestApp {
            db,
            services,
            router,
            outbox,
            pdp,
            provider,
            config,
            stop,
            recorder,
            _db_dir: db_dir,
        }
    }
}

/// Inserts the `SecurityContext` of the request's [`TestUser`] extension
/// (default [`TestUser::A1`]) unless the request already carries one.
async fn inject_security_context(mut req: AxumRequest, next: Next) -> Response<Body> {
    if req.extensions().get::<SecurityContext>().is_none() {
        let user = req
            .extensions()
            .get::<TestUser>()
            .copied()
            .unwrap_or(TestUser::A1);
        req.extensions_mut().insert(user.security_context());
    }
    next.run(req).await
}

/// A running in-process application.
pub struct TestApp {
    pub db: Db,
    pub services: Services,
    /// The gear's routes plus the test `SecurityContext` layer.
    pub router: Router,
    pub outbox: OutboxHandle,
    pub pdp: Arc<MockPdp>,
    /// Fake OAGW + OpenAI-compatible provider used by the services; the S2S
    /// context is pre-set to [`TestUser::S2S`].
    pub provider: Arc<FakeProvider>,
    pub config: Arc<MiniChatConfig>,
    /// Root "gear stop" token of the background tasks (cancelled on drop).
    pub stop: CancellationToken,
    recorder: Arc<OutboxRecorder>,
    /// Directory of the file-backed database (removed on drop).
    _db_dir: Option<TestDbDir>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Status, headers and JSON body (`Null` when empty or not JSON) of a response.
#[derive(Debug)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
}

impl TestResponse {
    /// Read a whole response.
    ///
    /// # Panics
    /// When the body cannot be read.
    #[allow(clippy::expect_used)]
    pub async fn read(resp: Response<Body>) -> Self {
        let (parts, body) = resp.into_parts();
        let bytes = to_bytes(body, usize::MAX)
            .await
            .expect("read response body");
        Self {
            status: parts.status,
            headers: parts.headers,
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }
}

impl TestApp {
    #[must_use]
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Send `method path` as `user`, with an optional JSON body.
    ///
    /// # Panics
    /// When the request cannot be built.
    #[allow(clippy::expect_used)]
    pub async fn call(
        &self,
        user: TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(path);
        let body = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).expect("serialize body"))
            }
            None => Body::empty(),
        };
        let mut req = builder.body(body).expect("build request");
        req.extensions_mut().insert(user);
        TestResponse::read(self.raw(req).await).await
    }

    /// Send a raw request; its [`TestUser`] extension (default `A1`) becomes the
    /// caller's `SecurityContext`.
    ///
    /// # Panics
    /// Never: the router is infallible.
    #[allow(clippy::expect_used)]
    pub async fn raw(&self, req: Request<Body>) -> Response<Body> {
        self.router
            .clone()
            .oneshot(req)
            .await
            .expect("infallible router")
    }

    /// Payloads delivered so far to the handler of `queue`, in delivery order
    /// (each message once). Waits up to 5 s for a first delivery.
    pub async fn outbox_messages(&self, queue: QueueKind) -> Vec<Value> {
        self.outbox_messages_n(queue, 1).await
    }

    /// Like [`Self::outbox_messages`], but waits (up to 5 s) until at least `n`
    /// messages were delivered; returns what was delivered by then.
    pub async fn outbox_messages_n(&self, queue: QueueKind, n: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + OUTBOX_WAIT;
        loop {
            let msgs = self.recorder.messages(queue);
            if msgs.len() >= n || tokio::time::Instant::now() >= deadline {
                return msgs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Asserts that nothing is delivered to `queue` during `wait`.
    ///
    /// # Panics
    /// When a message was (or gets) delivered to `queue`.
    pub async fn assert_no_outbox(&self, queue: QueueKind, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let msgs = self.recorder.messages(queue);
            assert!(
                msgs.is_empty(),
                "unexpected {queue:?} outbox messages: {msgs:?}"
            );
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// `(partition_id, seq)` of a delivered message and its payload.
type Delivery = ((i64, i64), Value);

/// Delivered outbox messages per queue.
#[derive(Default)]
struct OutboxRecorder {
    delivered: Mutex<HashMap<QueueKind, Vec<Delivery>>>,
}

impl OutboxRecorder {
    fn record(&self, queue: QueueKind, msg: &OutboxMessage) {
        let key = (msg.partition_id, msg.seq);
        let payload = serde_json::from_slice(&msg.payload)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&msg.payload).into_owned()));
        let mut delivered = self
            .delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let list = delivered.entry(queue).or_default();
        if !list.iter().any(|(k, _)| *k == key) {
            list.push((key, payload));
        }
    }

    fn messages(&self, queue: QueueKind) -> Vec<Value> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&queue)
            .map(|l| l.iter().map(|(_, v)| v.clone()).collect())
            .unwrap_or_default()
    }
}

/// Records each delivery, then delegates to the real handler.
struct RecordingHandler {
    queue: QueueKind,
    inner: Arc<dyn LeasedMessageHandler>,
    recorder: Arc<OutboxRecorder>,
}

#[async_trait]
impl LeasedMessageHandler for RecordingHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.recorder.record(self.queue, msg);
        self.inner.handle(msg).await
    }
}
