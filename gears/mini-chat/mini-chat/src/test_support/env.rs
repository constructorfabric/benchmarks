//! Builder and environment of the harness.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::Body;
use http::{Method, Request};
use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1,
};
use oagw_sdk::ServiceGatewayClientV1;
use parking_lot::Mutex;
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::config::ConfigProvider;
use toolkit::contracts::{DatabaseCapability, RestApiCapability, RunnableCapability};
use toolkit::{Gear, GearCtx};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::MessageResult;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use super::authn::FakeAuthn;
use super::authz::{AuthzMode, FakeAuthz};
use super::oagw::{FakeOagw, FakeProvider};
use super::recording::{Delivered, PublishFaults, RecordingAudit, RecordingPolicy};
use super::registry::FakeTypesRegistry;
use super::response::{StreamingResponse, TestResponse};
use crate::MiniChatGear;
use crate::domain::attachment::IndexingTimings;
use crate::infra::plugin_select::select_instance;
use crate::infra::plugins::static_audit::StaticAuditPlugin;
use crate::infra::plugins::static_model_policy::StaticModelPolicyPlugin;

const GEAR_NAME: &str = "mini-chat";
const POLICY_PLUGIN_NAME: &str = "static-mini-chat-model-policy-plugin";
const AUDIT_PLUGIN_NAME: &str = "static-mini-chat-audit-plugin";
const PLUGIN_VENDOR: &str = "constructorfabric";
const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
const AUDIT_QUEUE: &str = "mini-chat.audit";
/// How long `wait_for_turn_state` polls.
const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long `drain_outbox` polls: a retried publication waits for the shared outbox backoff (1 s,
/// 2 s, 4 s ... with jitter), so several transient failures in a row need more than 5 s.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// A user identity; every request is sent with a `SecurityContext` built from it
/// (`subject_id = user_id`, `subject_tenant_id = tenant_id`, `token_scopes = ["*"]`).
#[derive(Clone, Debug)]
pub struct TestUser {
    /// The user's tenant.
    pub tenant_id: Uuid,
    /// The user.
    pub user_id: Uuid,
}

impl TestUser {
    /// A user with a random tenant and a random id.
    pub fn new() -> Self {
        Self::in_tenant(Uuid::new_v4())
    }

    /// A random user in the given tenant.
    pub fn in_tenant(tenant_id: Uuid) -> Self {
        Self {
            tenant_id,
            user_id: Uuid::new_v4(),
        }
    }

    fn security_context(&self) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(self.user_id)
            .subject_tenant_id(self.tenant_id)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .expect("subject and tenant are set")
    }
}

impl Default for TestUser {
    fn default() -> Self {
        Self::new()
    }
}

struct StaticConfig(HashMap<String, Value>);

impl ConfigProvider for StaticConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&Value> {
        self.0.get(gear_name)
    }
}

/// Deep-merges `patch` over `base`: objects merge key by key, every other value is replaced.
fn deep_merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                match base_map.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

fn default_gear_config() -> Value {
    json!({
        "client_credentials": {
            "client_id": "mini-chat-test",
            "client_secret": "mini-chat-test-secret"
        },
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "host": "llm.test",
                "storage_kind": "openai",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {
                    "header": "Authorization",
                    "prefix": "Bearer ",
                    "secret_ref": "cred://openai-key"
                }
            }
        }
    })
}

fn harness_model(id: &str, tier: &str, enabled: bool, is_default: bool) -> Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": format!("Display {id}"),
        "description": format!("Description of {id}"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": "You are a helpful assistant.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": if tier == "Premium" { "3x" } else { "1x" },
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50
        },
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 2,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": tier == "Premium",
                "file_search": tier == "Premium",
                "image_generation": false,
                "code_interpreter": tier == "Premium",
                "mcp": false
            },
            "supported_endpoints": { "chat_completions": true, "responses": true }
        },
        "preference": { "is_default": is_default, "sort_order": 0 }
    })
}

fn default_policy_config() -> Value {
    json!({
        "model_catalog": [
            harness_model("gpt-premium", "Premium", true, true),
            harness_model("gpt-standard", "Standard", true, false),
            harness_model("gpt-disabled", "Standard", false, false)
        ]
    })
}

/// Builds a [`TestEnv`].
pub struct TestEnvBuilder {
    gear_config: Value,
    policy_config: Value,
    authz: AuthzMode,
    audit_plugin: bool,
    indexing_timings: Option<IndexingTimings>,
}

impl TestEnvBuilder {
    /// The harness defaults: Appendix B defaults, one `openai` provider, the default three-model
    /// catalog and an owner-scoped PDP.
    pub fn new() -> Self {
        Self {
            gear_config: default_gear_config(),
            policy_config: default_policy_config(),
            authz: AuthzMode::default(),
            audit_plugin: true,
            indexing_timings: None,
        }
    }

    /// Merges `patch` (deep, object by object) over the gear configuration. Unknown keys reach the
    /// gear's own deserializer, which rejects them.
    #[must_use]
    pub fn gear_config(mut self, patch: Value) -> Self {
        deep_merge(&mut self.gear_config, patch);
        self
    }

    /// Replaces the static model-policy plugin configuration.
    #[must_use]
    pub fn policy_config(mut self, config: Value) -> Self {
        self.policy_config = config;
        self
    }

    /// Selects how the fake PDP answers.
    #[must_use]
    pub fn authz(mut self, mode: AuthzMode) -> Self {
        self.authz = mode;
        self
    }

    /// Registers no audit plugin (neither its types-registry instance nor its client), so audit
    /// delivery finds "no audit plugin registered".
    #[must_use]
    pub fn without_audit_plugin(mut self) -> Self {
        self.audit_plugin = false;
        self
    }

    /// Overrides the fixed indexing timings of an upload (request deadline, polling waits,
    /// background rounds and limit) for this environment.
    #[must_use]
    pub fn indexing_timings(mut self, timings: IndexingTimings) -> Self {
        self.indexing_timings = Some(timings);
        self
    }

    /// Builds the environment.
    ///
    /// # Panics
    /// Panics with the error when the configuration is invalid.
    pub async fn build(self) -> TestEnv {
        match self.try_build().await {
            Ok(env) => env,
            Err(err) => panic!("test environment could not be built: {err:#}"),
        }
    }

    /// Builds the environment, or returns the init error of the gear or of a bundled plugin.
    ///
    /// # Errors
    /// The error raised by gear or plugin `init` (for example an invalid configuration), or by
    /// the database setup.
    pub async fn try_build(self) -> anyhow::Result<TestEnv> {
        let dsn = format!(
            "sqlite:file:mini_chat_test_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        );
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(1),
                min_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .context("connect the in-memory database")?;
        let gear = Arc::new(MiniChatGear::default());
        if let Some(timings) = self.indexing_timings {
            gear.use_indexing_timings(timings);
        }
        run_migrations_for_testing(&db, gear.migrations())
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let hub = Arc::new(ClientHub::default());
        hub.register::<dyn AuthZResolverApi>(Arc::new(FakeAuthz::new(self.authz)));
        hub.register::<dyn AuthNResolverClient>(Arc::new(FakeAuthn));
        hub.register::<dyn TypesRegistryClient>(Arc::new(FakeTypesRegistry::default()));
        let provider = FakeProvider::default();
        hub.register::<dyn ServiceGatewayClientV1>(Arc::new(FakeOagw::new(provider.clone())));

        let config = Arc::new(StaticConfig(HashMap::from([
            (GEAR_NAME.to_owned(), json!({ "config": self.gear_config })),
            (
                POLICY_PLUGIN_NAME.to_owned(),
                json!({ "config": self.policy_config }),
            ),
        ])));
        let cancel = CancellationToken::new();
        let ctx_for = |name: &str| {
            GearCtx::new(
                name.to_owned(),
                Uuid::new_v4(),
                Arc::clone(&config) as Arc<dyn ConfigProvider>,
                Arc::clone(&hub),
                cancel.child_token(),
            )
            .with_db(DBProvider::new(db.clone()))
        };

        StaticModelPolicyPlugin::default()
            .init(&ctx_for(POLICY_PLUGIN_NAME))
            .await
            .context("static model policy plugin init")?;
        if self.audit_plugin {
            StaticAuditPlugin::default()
                .init(&ctx_for(AUDIT_PLUGIN_NAME))
                .await
                .context("static audit plugin init")?;
        }

        let published: Delivered = Delivered::default();
        let delivered: Delivered = Delivered::default();
        let faults = Arc::new(PublishFaults::default());
        wrap_plugins(&hub, &published, &delivered, &faults, self.audit_plugin).await?;

        let gear_ctx = ctx_for(GEAR_NAME);
        gear.init(&gear_ctx).await.context("mini-chat init")?;
        let enqueued = EnqueuedMessages::default();
        let outcomes = HandlerOutcomes::default();
        if let Some(enqueuer) = gear.outbox_enqueuer() {
            let sink = Arc::clone(&enqueued);
            enqueuer.observe(Arc::new(move |queue, payload| {
                sink.lock().push((queue.to_owned(), payload.to_vec()));
            }));
            let results = Arc::clone(&outcomes);
            enqueuer.observe_outcomes(Arc::new(move |queue, result| {
                results
                    .lock()
                    .push((queue.to_owned(), outcome_name(result).to_owned()));
            }));
        }
        gear.start(cancel.child_token())
            .await
            .context("mini-chat start")?;

        let openapi = OpenApiRegistryImpl::new();
        let router = gear
            .register_rest(&gear_ctx, Router::new(), &openapi)
            .context("mini-chat REST registration")?
            .layer(axum::middleware::from_fn(
                toolkit::api::canonical_error_middleware,
            ));

        let raw_db = Database::connect(ConnectOptions::new(dsn).max_connections(1).to_owned())
            .await
            .context("connect the row-inspection connection")?;
        Ok(TestEnv {
            router,
            gear,
            provider,
            raw_db,
            enqueued,
            published,
            delivered,
            faults,
            outcomes,
            db_handle: db,
            cancel,
        })
    }
}

impl Default for TestEnvBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Messages handed to the outbox: `(queue, payload)` in enqueue order.
type EnqueuedMessages = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Handler results: `(queue, "ok" | "retry" | "reject")` in invocation order.
type HandlerOutcomes = Arc<Mutex<Vec<(String, String)>>>;

fn outcome_name(result: &MessageResult) -> &'static str {
    match result {
        MessageResult::Ok => "ok",
        MessageResult::Retry => "retry",
        MessageResult::Reject(_) => "reject",
    }
}

/// A running gear with its fakes.
pub struct TestEnv {
    router: Router,
    gear: Arc<MiniChatGear>,
    provider: FakeProvider,
    raw_db: DatabaseConnection,
    enqueued: EnqueuedMessages,
    published: Delivered,
    delivered: Delivered,
    faults: Arc<PublishFaults>,
    outcomes: HandlerOutcomes,
    db_handle: toolkit_db::Db,
    cancel: CancellationToken,
}

/// Registers recording wrappers over the bundled plugins in the hub.
async fn wrap_plugins(
    hub: &Arc<ClientHub>,
    published: &Delivered,
    delivered: &Delivered,
    faults: &Arc<PublishFaults>,
    audit_plugin: bool,
) -> anyhow::Result<()> {
    let policy_id = select_instance::<MiniChatModelPolicyPluginSpecV1>(hub, PLUGIN_VENDOR)
        .await
        .map_err(|err| anyhow::anyhow!("model policy plugin lookup: {err:?}"))?;
    let policy_scope = ClientScope::gts_id(&policy_id);
    let policy = hub
        .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&policy_scope)
        .context("the model policy plugin client is registered")?;
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        policy_scope,
        Arc::new(RecordingPolicy {
            inner: policy,
            published: Arc::clone(published),
            faults: Arc::clone(faults),
        }),
    );
    if !audit_plugin {
        return Ok(());
    }
    let audit_id = select_instance::<MiniChatAuditPluginSpecV1>(hub, PLUGIN_VENDOR)
        .await
        .map_err(|err| anyhow::anyhow!("audit plugin lookup: {err:?}"))?;
    let audit_scope = ClientScope::gts_id(&audit_id);
    let audit = hub
        .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&audit_scope)
        .context("the audit plugin client is registered")?;
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
        audit_scope,
        Arc::new(RecordingAudit {
            inner: audit,
            delivered: Arc::clone(delivered),
        }),
    );
    Ok(())
}

impl TestEnv {
    /// The default environment.
    pub async fn new() -> TestEnv {
        TestEnvBuilder::new().build().await
    }

    /// Sends a JSON request (`content-type: application/json` when there is a body).
    pub async fn request(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(path);
        let body = match body {
            Some(value) => {
                builder = builder.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let request = builder.body(body).expect("a valid request");
        self.request_raw(user, request).await
    }

    /// Sends an arbitrary request; the `SecurityContext` of `user` is injected.
    pub async fn request_raw(&self, user: &TestUser, mut request: Request<Body>) -> TestResponse {
        request.extensions_mut().insert(user.security_context());
        let response = match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("the response body is readable");
        TestResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    /// Sends a JSON request and returns as soon as the response headers are available; the body
    /// is read with [`StreamingResponse::next_event`]. Dropping the response is the client
    /// disconnect.
    pub async fn request_streaming(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> StreamingResponse {
        let mut builder = Request::builder().method(method).uri(path);
        let body = match body {
            Some(value) => {
                builder = builder.header(http::header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let mut request = builder.body(body).expect("a valid request");
        request.extensions_mut().insert(user.security_context());
        let response = match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let (parts, body) = response.into_parts();
        StreamingResponse::new(
            parts.status,
            parts.headers,
            Box::pin(body.into_data_stream()),
        )
    }

    /// Polls the database (at most 5 s) until the internal state of the turn is `state`.
    ///
    /// # Panics
    /// Panics on timeout.
    pub async fn wait_for_turn_state(&self, chat_id: Uuid, request_id: Uuid, state: &str) {
        let sql = format!(
            "SELECT state FROM chat_turns WHERE hex(chat_id) = '{}' AND hex(request_id) = '{}'",
            chat_id.simple().to_string().to_uppercase(),
            request_id.simple().to_string().to_uppercase()
        );
        let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
        loop {
            let row = self
                .raw_db
                .query_one_raw(sea_orm::Statement::from_string(
                    sea_orm::DbBackend::Sqlite,
                    sql.clone(),
                ))
                .await
                .expect("the turn state query succeeds");
            let last = row.and_then(|row| row.try_get::<String>("", "state").ok());
            if last.as_deref() == Some(state) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "turn {request_id} did not reach `{state}` in time; last state: {last:?}"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Waits (at most 5 s) until the usage and audit handlers delivered every message enqueued so
    /// far; a second call delivers nothing new.
    pub async fn drain_outbox(&self) {
        let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
        loop {
            let usage = self.outbox_payloads(USAGE_QUEUE).await.len();
            let audit = self.outbox_payloads(AUDIT_QUEUE).await.len();
            if self.published.lock().len() >= usage && self.delivered.lock().len() >= audit {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// The usage events the model-policy plugin received.
    pub fn published_usage(&self) -> Vec<Value> {
        self.published.lock().clone()
    }

    /// The audit events the audit plugin received.
    pub fn delivered_audit(&self) -> Vec<Value> {
        self.delivered.lock().clone()
    }

    /// The fake LLM and RAG provider behind the fake OAGW.
    pub fn provider(&self) -> &FakeProvider {
        &self.provider
    }

    /// A raw connection to the gear's database for direct row assertions.
    pub fn db(&self) -> DatabaseConnection {
        self.raw_db.clone()
    }

    /// Every message handed to the outbox queue `queue` so far (including processed ones), as
    /// JSON, in enqueue order.
    #[allow(
        clippy::unused_async,
        reason = "async to match the harness contract shared with the handler-driven slices"
    )]
    pub async fn outbox_payloads(&self, queue: &str) -> Vec<Value> {
        self.enqueued
            .lock()
            .iter()
            .filter(|(name, _)| name == queue)
            .filter_map(|(_, payload)| serde_json::from_slice(payload).ok())
            .collect()
    }

    /// Runs one orphan-watchdog scan with `now` as the application clock (the production scan
    /// code, independent of `orphan_watchdog.enabled` and `scan_interval_secs`).
    pub async fn run_orphan_watchdog_once(&self, now: time::OffsetDateTime) {
        if let Some(watchdog) = self.gear.orphan_watchdog() {
            watchdog.scan_once(now).await;
        }
    }

    /// Runs one upload-reaper scan with `now` as the application clock (the production scan code,
    /// independent of `upload_reaper.enabled` and `scan_interval_secs`).
    pub async fn run_upload_reaper_once(&self, now: time::OffsetDateTime) {
        if let Some(services) = self.gear.services_handle() {
            services.upload_reaper.scan_once(now).await;
        }
    }

    /// Runs one orphan-watchdog scan whose candidate read is followed by `race_sql` (a statement
    /// committed by "another connection" between the watchdog's read and its finalizing CAS), then
    /// returns the scan report as `(detected, finalized)`.
    pub async fn run_orphan_watchdog_once_racing(
        &self,
        now: time::OffsetDateTime,
        race_sql: &str,
    ) -> (usize, usize) {
        use crate::domain::orphan::{
            OrphanCandidate, OrphanRepository, OrphanWatchdog, OrphanWatchdogParts,
        };
        use crate::infra::storage::TurnRepositoryImpl;

        /// Candidate discovery that runs `sql` right after the real read.
        struct RacingCandidates {
            inner: Arc<TurnRepositoryImpl>,
            db: DatabaseConnection,
            sql: String,
        }

        #[async_trait::async_trait]
        impl OrphanRepository for RacingCandidates {
            async fn stale_turns(
                &self,
                cutoff: time::OffsetDateTime,
                limit: u64,
                attempted: &[Uuid],
            ) -> Result<Vec<OrphanCandidate>, crate::domain::error::DomainError> {
                let found = self.inner.stale_turns(cutoff, limit, attempted).await?;
                if !found.is_empty() {
                    self.db
                        .execute_raw(sea_orm::Statement::from_string(
                            sea_orm::DbBackend::Sqlite,
                            self.sql.clone(),
                        ))
                        .await
                        .expect("the racing statement must succeed");
                }
                Ok(found)
            }
        }

        let services = self
            .gear
            .services_handle()
            .expect("the gear is initialized");
        let turns = Arc::new(TurnRepositoryImpl::new(
            Arc::clone(&services.db),
            Arc::clone(&services.outbox),
            Arc::clone(&services.metrics),
        ));
        let watchdog = OrphanWatchdog::new(OrphanWatchdogParts {
            candidates: Arc::new(RacingCandidates {
                inner: Arc::clone(&turns),
                db: self.raw_db.clone(),
                sql: race_sql.to_owned(),
            }),
            turns,
            policy: Arc::clone(&services.policy),
            metrics: Arc::clone(&services.metrics),
            timeout: std::time::Duration::from_secs(services.config.orphan_watchdog.timeout_secs),
            scan_interval: std::time::Duration::from_secs(
                services.config.orphan_watchdog.scan_interval_secs,
            ),
        });
        let report = watchdog.scan_once(now).await;
        (report.detected, report.finalized)
    }

    /// Runs the repository's `fail_unstarted` CAS on `turn_id` (the plain failure of an unstarted
    /// retry/edit turn after a setup failure); `true` when the turn was moved to `failed`.
    pub async fn fail_unstarted_turn(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
        now: time::OffsetDateTime,
    ) -> bool {
        use crate::domain::mutation::MutationRepository as _;
        use crate::infra::storage::MutationRepositoryImpl;

        let services = self
            .gear
            .services_handle()
            .expect("the gear is initialized");
        let repo =
            MutationRepositoryImpl::new(Arc::clone(&services.db), Arc::clone(&services.outbox));
        repo.fail_unstarted(
            &toolkit_security::AccessScope::for_tenant(tenant_id),
            turn_id,
            error_code,
            now,
        )
        .await
        .expect("the CAS runs")
    }

    /// The next `count` calls of the model-policy plugin's `publish_usage` fail transiently.
    pub fn fail_next_usage_publishes(&self, count: u32) {
        self.faults.fail_transient(count);
    }

    /// The next `count` calls of the model-policy plugin's `publish_usage` fail permanently.
    pub fn reject_next_usage_publishes(&self, count: u32) {
        self.faults.fail_permanent(count);
    }

    /// The next calls of the model-policy plugin's `get_policy_snapshot` fail with `errors`, one
    /// error per call, in order; later calls succeed again.
    pub fn fail_next_policy_snapshots(
        &self,
        errors: impl IntoIterator<Item = mini_chat_sdk::ModelPolicyPluginError>,
    ) {
        self.faults.fail_snapshots(errors);
    }

    /// Calls that reached the recording model-policy plugin's `publish_usage`.
    pub fn usage_publish_attempts(&self) -> usize {
        self.faults.attempts()
    }

    /// Enqueues an arbitrary payload on the outbox queue `queue` through the real outbox,
    /// partitioned by `partition_key`. It is not announced to the enqueue observer.
    pub async fn enqueue_raw_outbox(&self, queue: &str, partition_key: Uuid, payload: &[u8]) {
        let enqueuer = self
            .gear
            .outbox_enqueuer()
            .expect("the gear is initialized");
        enqueuer
            .enqueue_raw(&self.db_handle, queue, partition_key, payload.to_vec())
            .await
            .expect("the raw outbox message is enqueued");
    }

    /// The outcome of every handler invocation of `queue`, in invocation order.
    ///
    /// A `reject` is reported once its message is in the dead-letter store: the call waits (at
    /// most 2 s) for the outbox to move the rejected messages there.
    pub async fn outbox_outcomes(&self, queue: &str) -> Vec<String> {
        let outcomes: Vec<String> = self
            .outcomes
            .lock()
            .iter()
            .filter(|(name, _)| name == queue)
            .map(|(_, outcome)| outcome.clone())
            .collect();
        let rejected = outcomes
            .iter()
            .filter(|outcome| *outcome == "reject")
            .count();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.dead_letter_count(queue).await < rejected
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        outcomes
    }

    async fn dead_letter_count(&self, queue: &str) -> usize {
        let sql = format!(
            "SELECT COUNT(*) AS n FROM mini_chat__outbox_dead_letters d \
             JOIN mini_chat__outbox_partitions p ON p.id = d.partition_id \
             WHERE p.queue = '{queue}'"
        );
        self.raw_db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql,
            ))
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<i64>("", "n").ok())
            .map_or(0, |count| usize::try_from(count).unwrap_or(0))
    }

    /// Makes the next `count` finalization transactions fail with a transient error (fault
    /// injection).
    pub fn fail_next_finalizations(&self, count: u32) {
        if let Some(enqueuer) = self.gear.outbox_enqueuer() {
            enqueuer.inject_enqueue_failures(count);
        }
    }

    /// The gear under test.
    pub fn gear(&self) -> &Arc<MiniChatGear> {
        &self.gear
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
