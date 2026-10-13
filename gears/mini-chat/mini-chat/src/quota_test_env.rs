#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
//! Fixture of the quota persistence and REST tests
//! (`cpt-cf-mini-chat-feature-quota-billing`): a migrated `SQLite` database
//! (shared in memory, or a file in WAL mode with a pool like the server), the
//! real `SeaQuotaRepository` over a running outbox whose usage handler keeps
//! the messages (so tests can read them), the quota service over a fake policy
//! catalog, a fake PDP and a recording metrics sink, the status router, seeding
//! helpers and raw-SQL readers.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, UserLimits};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::Value;
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxHandle, OutboxMessage};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_quota_routes;
use crate::chat_test_env::{OutboxRow, Reply, blob, chat_id, msg_id, request_id, ts};
use crate::config::{OutboxConfig, ThreadSummaryWorkerConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::quota::finalize::{
    FinalizeRequest, FinalizeVariant, TerminalClassification, TerminalColumns,
};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::ports::{QuotaMetrics, QuotaRepository};
use crate::domain::service::quota::service::{QuotaDeps, QuotaService, QuotaServiceConfig};
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::quota::test_support::{
    FakePolicy, RecordingMetrics, default_config, design_catalog, design_limits, fixed_clock, noon,
    noon_keys, settlement_snapshot, snapshot,
};
use crate::infra::db::repos::quota::SeaQuotaRepository;
use crate::infra::db::test_support::{Row, TestDb, chat_row, message_row, turn_row};
use crate::infra::outbox::queues::{OutboxEnqueuer, OutboxHandlers, start_outbox};
use crate::infra::outbox::usage::UsageHandler;
use crate::test_doubles::{FakePdp, FakePolicyPlugin, PdpReply, PluginWorld, USER, USER_TENANT};

/// Default `url_prefix`.
pub const PREFIX: &str = "/mini-chat";
/// Default usage queue.
pub const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
/// `period_start` of the daily rows of [`noon`].
pub const DAILY_START: &str = "2026-03-10";
/// `period_start` of the monthly rows of [`noon`].
pub const MONTHLY_START: &str = "2026-03-01";
/// The text of [`noon`] as stored by the application.
pub const NOON_TEXT: &str = "2026-03-10T12:00:00Z";

/// A usage handler that never acknowledges: the messages stay in the outbox
/// tables where the tests read them.
struct HoldHandler;

#[async_trait]
impl LeasedMessageHandler for HoldHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Retry
    }
}

/// Fixture options.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    /// File-backed WAL database with a pool of several connections.
    pub file_db: bool,
    /// Answer of the fake PDP.
    pub pdp: PdpReply,
    /// Start the outbox pipeline and bind the enqueuer.
    pub outbox: bool,
    /// `outbox.queue_name`; `None` keeps the default.
    pub usage_queue: Option<&'static str>,
    /// Quota service configuration.
    pub config: QuotaServiceConfig,
    /// Deliver the usage messages to the fake model-policy plugin through the
    /// real usage handler (they are then consumed instead of kept).
    pub deliver_usage: bool,
    /// The clock of the service.
    pub now: OffsetDateTime,
    /// Catalog served by the fake policy catalog (policy version 7).
    pub catalog: Vec<ModelCatalogEntry>,
    /// Limits served by the fake policy catalog.
    pub limits: UserLimits,
    /// Handler of the attachment cleanup queue; `None` keeps the placeholder
    /// (the messages stay readable in the outbox tables).
    pub attachment_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
    /// Handler of the chat cleanup queue; `None` keeps the placeholder.
    pub chat_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
    /// Handler of the thread-summary queue; `None` keeps the placeholder.
    pub thread_summary: Option<Arc<dyn LeasedMessageHandler>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            file_db: false,
            pdp: PdpReply::AllowScoped,
            outbox: true,
            usage_queue: None,
            config: default_config(),
            deliver_usage: false,
            now: noon(),
            catalog: design_catalog(),
            limits: design_limits(7),
            attachment_cleanup: None,
            chat_cleanup: None,
            thread_summary: None,
        }
    }
}

impl Options {
    /// Defaults on a file-backed WAL database.
    pub fn file() -> Self {
        Self {
            file_db: true,
            ..Self::default()
        }
    }
}

/// The fixture.
pub struct QuotaEnv {
    pub db: TestDb,
    pub provider: Arc<DBProvider<DomainError>>,
    pub enqueuer: Arc<OutboxEnqueuer>,
    pub repo: Arc<SeaQuotaRepository>,
    pub policy: Arc<FakePolicy>,
    pub pdp: Arc<FakePdp>,
    pub metrics: Arc<RecordingMetrics>,
    pub service: Arc<QuotaService>,
    pub options: Options,
    /// The plugin that receives the usage events when `deliver_usage` is set.
    pub plugin: Option<Arc<FakePolicyPlugin>>,
    outbox: Option<OutboxHandle>,
}

impl QuotaEnv {
    /// Defaults on the shared in-memory database.
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    /// Defaults on a file-backed WAL database.
    pub async fn file() -> Self {
        Self::with(Options::file()).await
    }

    /// Builds the fixture.
    pub async fn with(options: Options) -> Self {
        let db = if options.file_db {
            TestDb::migrated_file().await
        } else {
            TestDb::migrated().await
        };
        let provider = Arc::new(DBProvider::<DomainError>::new(db.db.clone()));
        let mut outbox_config = OutboxConfig {
            num_partitions: 4,
            ..OutboxConfig::default()
        };
        if let Some(queue) = options.usage_queue {
            queue.clone_into(&mut outbox_config.queue_name);
        }
        let enqueuer = Arc::new(OutboxEnqueuer::new(&outbox_config));
        let (usage_handler, plugin): (
            Arc<dyn LeasedMessageHandler>,
            Option<Arc<FakePolicyPlugin>>,
        ) = if options.deliver_usage {
            let world = PluginWorld::new();
            let plugin = FakePolicyPlugin::with_catalog(Vec::new());
            world
                .register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
            (
                Arc::new(UsageHandler::new(Arc::new(world.policy_gateway()))),
                Some(plugin),
            )
        } else {
            (Arc::new(HoldHandler), None)
        };
        let outbox = if options.outbox {
            let handle = start_outbox(
                &db.db,
                &outbox_config,
                &ThreadSummaryWorkerConfig::default(),
                OutboxHandlers {
                    usage: usage_handler,
                    audit: Arc::new(HoldHandler),
                    attachment_cleanup: options.attachment_cleanup.clone(),
                    chat_cleanup: options.chat_cleanup.clone(),
                    thread_summary: options.thread_summary.clone(),
                },
            )
            .await
            .expect("the outbox pipeline starts");
            assert!(enqueuer.bind(handle.outbox()));
            Some(handle)
        } else {
            None
        };
        let repo = Arc::new(SeaQuotaRepository::new(
            Arc::clone(&provider),
            Arc::clone(&enqueuer),
            options.config.quota.overshoot_tolerance_factor,
        ));
        let policy = FakePolicy::serving(snapshot(7, options.catalog.clone()), options.limits);
        let pdp = FakePdp::new(options.pdp);
        let metrics = RecordingMetrics::new();
        let service = Arc::new(
            QuotaService::new(QuotaDeps {
                authz: Arc::new(AuthzService::new(
                    Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>
                )),
                policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
                repo: Arc::clone(&repo) as Arc<dyn QuotaRepository>,
                metrics: Arc::clone(&metrics) as Arc<dyn QuotaMetrics>,
                config: options.config.clone(),
            })
            .with_clock(fixed_clock(options.now)),
        );
        Self {
            db,
            provider,
            enqueuer,
            repo,
            policy,
            pdp,
            metrics,
            service,
            options,
            plugin,
            outbox,
        }
    }

    /// The quota status router under `prefix` with its `OpenAPI` registry.
    pub fn router(&self, prefix: &str) -> (Router, OpenApiRegistryImpl) {
        let openapi = OpenApiRegistryImpl::new();
        let router =
            register_quota_routes(Router::new(), &openapi, Arc::clone(&self.service), prefix);
        (router, openapi)
    }

    /// `GET uri` as `ctx` through the status router under [`PREFIX`].
    pub async fn get_as(&self, ctx: &SecurityContext, uri: &str) -> Reply {
        let (router, _openapi) = self.router(PREFIX);
        send(&router, "GET", uri, ctx).await
    }

    /// `GET uri` as the default user.
    pub async fn get(&self, uri: &str) -> Reply {
        self.get_as(&crate::test_doubles::user_ctx(), uri).await
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// Inserts chat `n` of the default user and tenant.
    pub async fn seed_chat(&self, n: u128) -> Uuid {
        self.seed_chat_of(n, USER_TENANT, USER).await
    }

    /// Inserts chat `n` of `(tenant, user)`.
    pub async fn seed_chat_of(&self, n: u128, tenant: Uuid, user: Uuid) -> Uuid {
        let id = chat_id(n);
        self.db
            .insert("chats", chat_row(id, tenant, user))
            .await
            .expect("seed chat");
        id
    }

    /// Inserts a turn; returns its id.
    pub async fn seed_turn(&self, seed: &TurnSeed) -> Uuid {
        let id = seed.id();
        let mut row = turn_row(id, seed.tenant, seed.chat, request_id(seed.n), seed.state)
            .set("started_at", seed.started_at.clone())
            .set("web_search_completed_count", seed.web_done)
            .set("code_interpreter_completed_count", seed.ci_done)
            .set("file_search_completed_count", seed.fs_done);
        row = match seed.requester {
            Some(user) => row.set("requester_user_id", user),
            None => row.null("requester_user_id"),
        };
        row = match &seed.last_progress_at {
            Some(at) => row.set("last_progress_at", at.clone()),
            None => row.null("last_progress_at"),
        };
        if let Some(reserve) = &seed.reserve {
            row = row
                .set("reserve_tokens", reserve.reserve_tokens)
                .set("max_output_tokens_applied", reserve.max_output_tokens)
                .set("reserved_credits_micro", reserve.reserved_credits_micro)
                .set("policy_version_applied", reserve.policy_version)
                .set("effective_model", reserve.effective_model.clone())
                .set("minimal_generation_floor_applied", reserve.floor);
        }
        if seed.deleted {
            row = row.set("deleted_at", crate::infra::db::test_support::TS);
        }
        self.db.insert("chat_turns", row).await.expect("seed turn");
        id
    }

    /// Inserts a quota row.
    pub async fn seed_bucket(&self, seed: &BucketSeed) {
        let row = Row::default()
            .set("id", Uuid::new_v4())
            .set("tenant_id", seed.tenant)
            .set("user_id", seed.user)
            .set("period_type", seed.period)
            .set("period_start", seed.start)
            .set("bucket", seed.bucket)
            .set("spent_credits_micro", seed.spent)
            .set("reserved_credits_micro", seed.reserved)
            .set("calls", seed.calls)
            .set("input_tokens", seed.input_tokens)
            .set("output_tokens", seed.output_tokens)
            .set("web_search_calls", seed.web)
            .set("code_interpreter_calls", seed.ci)
            .set("updated_at", crate::infra::db::test_support::TS);
        self.db
            .insert("quota_usage", row)
            .await
            .expect("seed bucket");
    }

    /// The four rows of DESIGN 5.10 for the default user at [`noon`]:
    /// `tier:premium` spent `20_000_000` daily and `200_000_000` monthly, `total`
    /// spent `25_000_000` daily and `240_000_000` monthly, plus `reserved` booked
    /// into `total` (and into `tier:premium` when `premium`).
    pub async fn seed_design_world(&self, reserved: i64, premium: bool) {
        self.seed_bucket(&BucketSeed::total("daily", 25_000_000, reserved))
            .await;
        self.seed_bucket(&BucketSeed::total("monthly", 240_000_000, reserved))
            .await;
        self.seed_bucket(&BucketSeed::premium(
            "daily",
            20_000_000,
            if premium { reserved } else { 0 },
        ))
        .await;
        self.seed_bucket(&BucketSeed::premium(
            "monthly",
            200_000_000,
            if premium { reserved } else { 0 },
        ))
        .await;
    }

    /// Inserts a message of chat `chat`; `created` is seconds after the base
    /// time of [`ts`].
    pub async fn seed_message(&self, chat: Uuid, seed: &MessageSeed) -> Uuid {
        let id = msg_id(seed.n);
        let mut row = message_row(id, USER_TENANT, chat)
            .set("role", seed.role)
            .set("created_at", ts(seed.created))
            .set("input_tokens", seed.input_tokens)
            .set("output_tokens", seed.output_tokens);
        if let Some(explicit) = seed.explicit_id {
            row = row.set("id", explicit);
        }
        self.db.insert("messages", row).await.expect("seed message");
        if seed.deleted {
            self.db
                .soft_delete("messages", seed.explicit_id.unwrap_or(id))
                .await;
        }
        seed.explicit_id.unwrap_or(id)
    }

    // ---- readers -----------------------------------------------------------------------------

    /// The row of the default user at [`noon`] in `period` and `bucket`.
    pub async fn bucket(&self, period: &str, bucket: &str) -> Option<Bucket> {
        let start = if period == "daily" {
            DAILY_START
        } else {
            MONTHLY_START
        };
        self.bucket_of(USER_TENANT, USER, period, start, bucket)
            .await
    }

    /// The `total` row of the default user at [`noon`].
    pub async fn total(&self, period: &str) -> Option<Bucket> {
        self.bucket(period, "total").await
    }

    /// The `tier:premium` row of the default user at [`noon`].
    pub async fn premium(&self, period: &str) -> Option<Bucket> {
        self.bucket(period, "tier:premium").await
    }

    /// A quota row by its full key.
    pub async fn bucket_of(
        &self,
        tenant: Uuid,
        user: Uuid,
        period: &str,
        start: &str,
        bucket: &str,
    ) -> Option<Bucket> {
        let sql = format!(
            "SELECT spent_credits_micro, reserved_credits_micro, calls, input_tokens, \
             output_tokens, file_search_calls, web_search_calls, code_interpreter_calls, \
             rag_retrieval_calls, image_inputs, image_upload_bytes, updated_at \
             FROM quota_usage WHERE tenant_id = {} AND user_id = {} AND period_type = '{period}' \
             AND period_start = '{start}' AND bucket = '{bucket}'",
            blob(tenant),
            blob(user)
        );
        let row = self
            .db
            .raw
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql))
            .await
            .expect("read bucket")?;
        let int = |index: usize| row.try_get_by_index::<i64>(index).expect("integer column");
        Some(Bucket {
            spent: int(0),
            reserved: int(1),
            calls: int(2),
            input_tokens: int(3),
            output_tokens: int(4),
            file_search_calls: int(5),
            web: int(6),
            ci: int(7),
            rag_retrieval_calls: int(8),
            image_inputs: int(9),
            image_upload_bytes: int(10),
            updated_at: row.try_get_by_index::<String>(11).expect("updated_at"),
        })
    }

    /// Number of `quota_usage` rows.
    pub async fn bucket_count(&self) -> i64 {
        self.db.count("quota_usage").await
    }

    /// Every `quota_usage` row and column as one string.
    pub async fn quota_dump(&self) -> String {
        self.dump("quota_usage").await
    }

    /// Every row and column of `table`, ordered, as one string.
    pub async fn dump(&self, table: &str) -> String {
        let names = self
            .db
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
            ))
            .await
            .expect("table info");
        let columns: Vec<String> = names
            .iter()
            .map(|row| format!("quote({})", row.try_get::<String>("", "name").unwrap()))
            .collect();
        let expr = columns.join("||'|'||");
        self.db
            .query_string(&format!(
                "SELECT COALESCE(group_concat(r, char(10)), '') \
                 FROM (SELECT {expr} AS r FROM {table} ORDER BY r)"
            ))
            .await
    }

    /// One column of a turn as `quote()`d SQL text (`NULL` for SQL NULL).
    pub async fn turn_col(&self, turn: Uuid, column: &str) -> String {
        self.db
            .query_string(&format!(
                "SELECT quote({column}) FROM chat_turns WHERE id = {}",
                blob(turn)
            ))
            .await
    }

    /// A text column of a turn; `None` for NULL.
    pub async fn turn_text(&self, turn: Uuid, column: &str) -> Option<String> {
        let quoted = self.turn_col(turn, column).await;
        (quoted != "NULL").then(|| quoted.trim_matches('\'').to_owned())
    }

    /// `chat_turns.state`.
    pub async fn turn_state(&self, turn: Uuid) -> String {
        self.turn_text(turn, "state").await.expect("state is set")
    }

    /// Rows of the usage queue (default or configured name).
    pub async fn usage_rows(&self) -> Vec<OutboxRow> {
        let queue = self
            .options
            .usage_queue
            .map_or_else(|| USAGE_QUEUE.to_owned(), str::to_owned);
        self.outbox_rows(&queue).await
    }

    /// Rows of `queue` of the outbox.
    pub async fn outbox_rows(&self, queue: &str) -> Vec<OutboxRow> {
        let sql = format!(
            "SELECT DISTINCT b.id AS body_id, p.queue AS queue, p.partition AS part, b.payload AS payload \
             FROM mini_chat__outbox_body b \
             JOIN (SELECT partition_id, body_id FROM mini_chat__outbox_incoming \
                   UNION ALL SELECT partition_id, body_id FROM mini_chat__outbox_outgoing) m \
               ON m.body_id = b.id \
             JOIN mini_chat__outbox_partitions p ON p.id = m.partition_id \
             WHERE p.queue = '{queue}' ORDER BY b.id"
        );
        let mut last_error = None;
        for _ in 0..20 {
            match self
                .db
                .raw
                .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql.clone()))
                .await
            {
                Ok(rows) => {
                    return rows
                        .iter()
                        .map(|row| OutboxRow {
                            queue: row.try_get("", "queue").unwrap(),
                            partition: row.try_get("", "part").unwrap(),
                            payload: row.try_get("", "payload").unwrap(),
                        })
                        .collect();
                }
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        panic!("cannot read the outbox: {last_error:?}");
    }

    /// Number of messages of every outbox queue together.
    pub async fn outbox_message_count(&self) -> i64 {
        self.db.count("mini_chat__outbox_body").await
    }
}

/// Sends a request as `ctx` and reads the reply.
pub async fn send(router: &Router, method: &str, uri: &str, ctx: &SecurityContext) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ctx.clone());
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        text,
        json,
    }
}

// ---- seeds -------------------------------------------------------------------------------------

/// The reserve columns of a seeded turn.
#[derive(Clone)]
pub struct ReserveSeed {
    pub reserve_tokens: i64,
    pub max_output_tokens: i32,
    pub reserved_credits_micro: i64,
    pub floor: i32,
    pub policy_version: i64,
    pub effective_model: String,
}

/// A seeded `chat_turns` row.
#[derive(Clone)]
pub struct TurnSeed {
    pub n: u128,
    pub chat: Uuid,
    pub tenant: Uuid,
    pub requester: Option<Uuid>,
    pub state: &'static str,
    pub reserve: Option<ReserveSeed>,
    pub started_at: String,
    pub last_progress_at: Option<String>,
    pub deleted: bool,
    pub web_done: i32,
    pub ci_done: i32,
    pub fs_done: i32,
}

impl TurnSeed {
    /// A running standard turn of the DESIGN 5.10 numbers: `reserve_tokens`
    /// 1500, 500 output tokens, reserved credits `1_500_000`, floor 50, policy
    /// version 7, effective model `s`; started at 11:30 and last progress at
    /// 11:59 of the day of [`noon`].
    pub fn standard(n: u128, chat: Uuid) -> Self {
        Self {
            n,
            chat,
            tenant: USER_TENANT,
            requester: Some(USER),
            state: "running",
            reserve: Some(ReserveSeed {
                reserve_tokens: 1500,
                max_output_tokens: 500,
                reserved_credits_micro: 1_500_000,
                floor: 50,
                policy_version: 7,
                effective_model: "s".to_owned(),
            }),
            started_at: "2026-03-10T11:30:00Z".to_owned(),
            last_progress_at: Some("2026-03-10T11:59:00Z".to_owned()),
            deleted: false,
            web_done: 0,
            ci_done: 0,
            fs_done: 0,
        }
    }

    /// A running premium turn (`p`, reserved `3_750_000`).
    pub fn premium(n: u128, chat: Uuid) -> Self {
        let mut seed = Self::standard(n, chat);
        let reserve = seed.reserve.as_mut().unwrap();
        reserve.reserved_credits_micro = 3_750_000;
        reserve.effective_model = "p".to_owned();
        seed
    }

    /// The turn id.
    pub fn id(&self) -> Uuid {
        Uuid::from_u128(0xA000_0000 + self.n)
    }

    /// The request id of the turn.
    pub fn request(&self) -> Uuid {
        request_id(self.n)
    }

    pub fn with_reserve(mut self, reserve_tokens: i64, max_output: i32, credits: i64) -> Self {
        let reserve = self.reserve.as_mut().unwrap();
        reserve.reserve_tokens = reserve_tokens;
        reserve.max_output_tokens = max_output;
        reserve.reserved_credits_micro = credits;
        self
    }

    pub fn without_reserve(mut self) -> Self {
        self.reserve = None;
        self
    }

    pub fn without_requester(mut self) -> Self {
        self.requester = None;
        self
    }

    pub fn state(mut self, state: &'static str) -> Self {
        self.state = state;
        self
    }

    pub fn started(mut self, at: &str, last_progress: Option<&str>) -> Self {
        at.clone_into(&mut self.started_at);
        self.last_progress_at = last_progress.map(str::to_owned);
        self
    }

    pub fn applied_version(mut self, version: i64) -> Self {
        self.reserve.as_mut().unwrap().policy_version = version;
        self
    }

    pub fn tools_done(mut self, web: i32, ci: i32, fs: i32) -> Self {
        self.web_done = web;
        self.ci_done = ci;
        self.fs_done = fs;
        self
    }
}

/// A seeded `quota_usage` row (the default user at [`noon`] unless changed).
#[derive(Clone)]
pub struct BucketSeed {
    pub tenant: Uuid,
    pub user: Uuid,
    pub period: &'static str,
    pub start: &'static str,
    pub bucket: &'static str,
    pub spent: i64,
    pub reserved: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web: i32,
    pub ci: i32,
}

impl BucketSeed {
    fn new(period: &'static str, bucket: &'static str, spent: i64, reserved: i64) -> Self {
        Self {
            tenant: USER_TENANT,
            user: USER,
            period,
            start: if period == "daily" {
                DAILY_START
            } else {
                MONTHLY_START
            },
            bucket,
            spent,
            reserved,
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            web: 0,
            ci: 0,
        }
    }

    pub fn total(period: &'static str, spent: i64, reserved: i64) -> Self {
        Self::new(period, "total", spent, reserved)
    }

    pub fn premium(period: &'static str, spent: i64, reserved: i64) -> Self {
        Self::new(period, "tier:premium", spent, reserved)
    }

    pub fn of(mut self, tenant: Uuid, user: Uuid) -> Self {
        self.tenant = tenant;
        self.user = user;
        self
    }

    pub fn at(mut self, start: &'static str) -> Self {
        self.start = start;
        self
    }

    pub fn counters(mut self, calls: i32, input_tokens: i64, output_tokens: i64) -> Self {
        self.calls = calls;
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self
    }

    pub fn tools(mut self, web: i32, ci: i32) -> Self {
        self.web = web;
        self.ci = ci;
        self
    }
}

/// A seeded message.
#[derive(Clone)]
pub struct MessageSeed {
    pub n: u128,
    pub role: &'static str,
    pub created: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub deleted: bool,
    pub explicit_id: Option<Uuid>,
}

impl MessageSeed {
    pub fn assistant(n: u128, created: i64, input_tokens: i64, output_tokens: i64) -> Self {
        Self {
            n,
            role: "assistant",
            created,
            input_tokens,
            output_tokens,
            deleted: false,
            explicit_id: None,
        }
    }

    pub fn role(mut self, role: &'static str) -> Self {
        self.role = role;
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted = true;
        self
    }

    pub fn id(mut self, id: Uuid) -> Self {
        self.explicit_id = Some(id);
        self
    }
}

/// A row of `quota_usage` as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub spent: i64,
    pub reserved: i64,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub file_search_calls: i64,
    pub web: i64,
    pub ci: i64,
    pub rag_retrieval_calls: i64,
    pub image_inputs: i64,
    pub image_upload_bytes: i64,
    pub updated_at: String,
}

// ---- requests ----------------------------------------------------------------------------------

/// Builder of a stream-variant finalization request for a turn of
/// [`TurnSeed`]; defaults to a completed turn without usage, selected model
/// `p`, the period keys of [`noon`], tool counts 0 and the DESIGN 5.10
/// snapshot (version 7).
#[derive(Clone)]
pub struct StreamReq {
    pub turn: Uuid,
    pub tenant: Uuid,
    pub classification: TerminalClassification,
    pub columns: TerminalColumns,
    pub usage: Option<mini_chat_sdk::UsageTokens>,
    pub tools: ToolCallCounts,
    pub snapshot: SettlementSnapshot,
    pub now: OffsetDateTime,
}

impl StreamReq {
    /// A completed turn with an assistant message id and a provider response.
    pub fn completed(turn: Uuid) -> Self {
        Self {
            turn,
            tenant: USER_TENANT,
            classification: TerminalClassification::Completed,
            columns: TerminalColumns {
                state: mini_chat_sdk::TerminalState::Completed,
                error_code: None,
                error_detail: None,
                assistant_message_id: Some(Uuid::from_u128(0xB000_0001)),
                provider_response_id: Some("resp_1".to_owned()),
            },
            usage: None,
            tools: ToolCallCounts::default(),
            snapshot: settlement_snapshot(7, design_catalog()),
            now: noon(),
        }
    }

    /// A failed turn with `error_code`.
    pub fn failed(turn: Uuid, error_code: &str) -> Self {
        let mut req = Self::completed(turn);
        req.classification = TerminalClassification::Failed {
            error_code: error_code.to_owned(),
        };
        req.columns = TerminalColumns {
            state: mini_chat_sdk::TerminalState::Failed,
            error_code: Some(error_code.to_owned()),
            error_detail: Some("detail".to_owned()),
            assistant_message_id: None,
            provider_response_id: None,
        };
        req
    }

    /// A client disconnect.
    pub fn cancelled(turn: Uuid) -> Self {
        let mut req = Self::completed(turn);
        req.classification = TerminalClassification::Cancelled;
        req.columns = TerminalColumns {
            state: mini_chat_sdk::TerminalState::Cancelled,
            error_code: None,
            error_detail: None,
            assistant_message_id: None,
            provider_response_id: None,
        };
        req
    }

    /// The tenant the request is scoped to (defaults to [`USER_TENANT`]).
    pub fn tenant(mut self, tenant: Uuid) -> Self {
        self.tenant = tenant;
        self
    }

    pub fn usage(mut self, input: u64, output: u64) -> Self {
        self.usage = Some(mini_chat_sdk::UsageTokens {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        });
        self
    }

    pub fn tools(mut self, web: u32, ci: u32, fs: u32) -> Self {
        self.tools = ToolCallCounts {
            web_search: web,
            code_interpreter: ci,
            file_search: fs,
        };
        self
    }

    pub fn snapshot(mut self, snapshot: SettlementSnapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    pub fn build(&self) -> FinalizeRequest {
        FinalizeRequest {
            turn_id: self.turn,
            variant: FinalizeVariant::Stream {
                tenant_id: self.tenant,
                period_keys: noon_keys(),
                selected_model: "p".to_owned(),
            },
            classification: self.classification.clone(),
            columns: self.columns.clone(),
            usage: self.usage,
            tool_calls: self.tools,
            snapshot: self.snapshot.clone(),
            now: self.now,
        }
    }
}

/// The orphan request of `turn` with the stale cutoff 11:55 of the day of
/// [`noon`] and the DESIGN 5.10 snapshot.
pub fn orphan_request(turn: Uuid) -> FinalizeRequest {
    FinalizeRequest::orphan(
        turn,
        time::Date::from_calendar_date(2026, time::Month::March, 10)
            .unwrap()
            .with_hms(11, 55, 0)
            .unwrap()
            .assume_utc(),
        settlement_snapshot(7, design_catalog()),
        noon(),
    )
}
