#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(
    clippy::format_push_string,
    clippy::needless_pass_by_value,
    clippy::struct_excessive_bools,
    clippy::too_many_lines
)]
//! Fixture of the background-workers tests
//! (`cpt-cf-mini-chat-feature-background-workers`): the turn-mutation fixture
//! (migrated `SQLite`, running outbox, fake PDP, fake policy, fake OAGW, the whole
//! chat, stream and mutation router, seeding of whole turns) extended with the
//! provider entries and catalog of the workers, the OAGW storage client, a
//! recording worker-metrics sink, builders of the chat cleanup, thread-summary
//! and orphan-watchdog logic over the real Secure ORM stores, seeding of
//! attachments, vector stores and summaries, and readers of the rows the
//! workers write.

use std::ops::Deref;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, TurnAuditEvent, UsageEvent};
use sea_orm::ConnectionTrait as _;
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use crate::attachment_test_env::{
    ALIAS, ANTHROPIC_ALIAS, AZURE_ALIAS, att_catalog, script_happy_on,
};
use crate::chat_test_env::{BASE_UNIX, blob, ts};
use crate::config::{MiniChatConfig, ThreadSummaryWorkerConfig};
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::attachments::ports::{
    BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::attachments::test_support::TestRuntime;
use crate::domain::service::quota::service::Clock;
use crate::domain::service::quota::test_support::model;
use crate::domain::service::stream::ports::{MessageBoundary, ProviderDirectory};
use crate::domain::service::workers::chat_cleanup::{ChatCleanupDeps, ChatCleanupService};
use crate::domain::service::workers::clock::{SharedWorkerClock, TokioWorkerClock};
use crate::domain::service::workers::metrics::WorkerMetrics;
use crate::domain::service::workers::ports::{OrphanStore, SummaryModelClient, SummaryStore};
use crate::domain::service::workers::summary::{SummaryDeps, SummaryService};
use crate::domain::service::workers::summary_trigger::SummaryTask;
use crate::domain::service::workers::test_support::RecordingWorkerMetrics;
use crate::domain::service::workers::{CHAT_CLEANUP_DELIVERY_BUDGET, HandlerOutcome};
use crate::infra::db::repos::chat_cleanup::SeaChatCleanupStore;
use crate::infra::db::repos::orphans::SeaOrphanStore;
use crate::infra::db::repos::summaries::SeaSummaryStore;
use crate::infra::db::test_support::Row;
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::llm::summary_client::OagwSummaryClient;
use crate::infra::outbox::chat_cleanup::{ChatCleanupHandler, ChatCleanupPayload};
use crate::infra::outbox::thread_summary::{ThreadSummaryHandler, encode_task};
use crate::infra::storage::oagw_storage::OagwStorageClient;
use crate::infra::workers::leader::LeaderElector;
use crate::infra::workers::orphan_watchdog::{OrphanWatchdog, WatchdogDeps};
use crate::mutation_test_env::{MutationEnv, SeedTurn};
use crate::stream_test_env::Options as StreamOptions;
use crate::test_doubles::{USER, USER_TENANT, outbox_message, provider_json};

/// Queue of the thread-summary work items.
pub const SUMMARY_QUEUE: &str = "mini-chat.thread_summary";
/// Queue of the chat cleanup messages.
pub const CHAT_CLEANUP_QUEUE: &str = "mini-chat.chat_cleanup";
/// Queue of the usage events.
pub const USAGE_QUEUE: &str = "mini-chat.usage_snapshot";
/// The summary model of the fixture (provider `openai`, kind `openai_responses`).
pub const SUMMARY_MODEL: &str = "sum";
/// The wire id of [`SUMMARY_MODEL`].
pub const SUMMARY_PROVIDER_MODEL: &str = "provider-sum";

/// A handler whose target is set after the pipeline started.
#[derive(Default)]
pub struct LazyHandler(OnceLock<Arc<dyn LeasedMessageHandler>>);

impl LazyHandler {
    /// Sets the target once.
    pub fn set(&self, handler: Arc<dyn LeasedMessageHandler>) {
        assert!(self.0.set(handler).is_ok(), "the lazy handler is set once");
    }
}

#[async_trait]
impl LeasedMessageHandler for LazyHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        match self.0.get() {
            Some(inner) => inner.handle(msg).await,
            None => MessageResult::Retry,
        }
    }
}

/// The catalog of the workers: the attachment catalog plus the summary models.
pub fn worker_catalog() -> Vec<ModelCatalogEntry> {
    let mut catalog = att_catalog();
    catalog.push(
        model(SUMMARY_MODEL, ModelTier::Standard)
            .provider("openai")
            .build(),
    );
    // small windows: the input budget is 1000 - 100 = 900 tokens (1 byte = 1 token)
    catalog.push(small_model("sum-small", ""));
    catalog.push(small_model("tiny", "SECRET-SYSTEM-PROMPT"));
    catalog.push(
        model("sum-off", ModelTier::Standard)
            .provider("openai")
            .enabled(false)
            .build(),
    );
    catalog.push(
        model("sum-noprov", ModelTier::Standard)
            .provider("nowhere")
            .build(),
    );
    catalog
}

/// A standard model of the `openai` provider with a context window of 1000
/// tokens, 100 output tokens and no separate input limit: an effective input
/// budget of 900 tokens, one token per byte. `system_prompt` is the system
/// prompt of the catalog entry; the summary prompt of the entry is empty.
pub fn small_model(id: &str, system_prompt: &str) -> ModelCatalogEntry {
    let mut small = crate::test_doubles::entry_json(id, true);
    small["context_window"] = json!(1000);
    small["max_output_tokens"] = json!(100);
    small["max_input_tokens"] = json!(0);
    small["system_prompt"] = json!(system_prompt);
    small["thread_summary_prompt"] = json!("");
    small["estimation_budgets"] = json!({
        "bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0,
        "image_token_budget": 1000, "tool_surcharge_tokens": 500,
        "web_search_surcharge_tokens": 500, "code_interpreter_surcharge_tokens": 1000,
        "minimal_generation_floor": 50
    });
    small["input_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    small["output_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    small["tier"] = json!("standard");
    crate::test_doubles::entry(small)
}

fn provider_entries() -> Vec<(&'static str, Value)> {
    let mut anthropic = provider_json(ANTHROPIC_ALIAS, "openai");
    anthropic["kind"] = json!("anthropic_messages");
    anthropic["api_path"] = json!("/v1/messages");
    anthropic["rag_provider"] = json!("openai");
    vec![
        ("azure", provider_json(AZURE_ALIAS, "azure")),
        ("anthropic", anthropic),
    ]
}

/// Fixture options.
#[derive(Clone)]
pub struct WorkerOptions {
    /// `cleanup_worker.max_attempts`.
    pub max_cleanup_attempts: u32,
    /// The gear runtime is not bound (the pipeline delivers before `start`
    /// has bound the S2S context).
    pub unbound_runtime: bool,
    /// `thread_summary_worker.*`.
    pub summary: ThreadSummaryWorkerConfig,
    /// `orphan_watchdog.timeout_secs`.
    pub orphan_timeout_secs: u64,
    /// The real handlers are registered on the queues of the running pipeline
    /// (the messages are consumed); otherwise the placeholders keep them in the
    /// outbox tables.
    pub real_pipeline: bool,
    /// Kill switches of the snapshot.
    pub switches: KillSwitches,
    /// `streaming.sse_ping_interval_seconds` of the stream service.
    pub ping: Duration,
    /// A file-backed WAL database with a pool (real concurrency).
    pub file_db: bool,
    /// Also record into the real OpenTelemetry instruments (an in-memory
    /// exporter), next to the recording sink.
    pub real_metrics: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        let mut summary = ThreadSummaryWorkerConfig::default();
        SUMMARY_MODEL.clone_into(&mut summary.summary_model_id);
        Self {
            max_cleanup_attempts: 5,
            unbound_runtime: false,
            summary,
            orphan_timeout_secs: 300,
            real_pipeline: false,
            switches: crate::domain::service::quota::test_support::NO_SWITCHES,
            ping: Duration::from_secs(60),
            file_db: false,
            real_metrics: false,
        }
    }
}

/// The columns of an attachment row the cleanup tests read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttSnap {
    pub status: String,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i64,
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<String>,
    pub deleted_at: Option<String>,
    pub provider_file_id: Option<String>,
    pub secondary_file_id: Option<String>,
}

/// The stored summary of a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySnap {
    pub text: String,
    pub frontier_at: String,
    pub frontier_id: Uuid,
    pub token_estimate: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// A cloneable handle that executes raw statements (for hooks).
#[derive(Clone)]
pub struct Sql(sea_orm::DatabaseConnection);

impl Sql {
    pub async fn exec(&self, sql: &str) {
        self.0
            .execute_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql.to_owned(),
            ))
            .await
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
}

/// The real instruments over an in-memory exporter.
pub struct Otel {
    pub provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    pub exporter: opentelemetry_sdk::metrics::InMemoryMetricExporter,
    pub metrics: Arc<crate::infra::metrics::MiniChatMetrics>,
}

/// Forwards every observation to both sinks.
struct Tee(
    Arc<RecordingWorkerMetrics>,
    Arc<crate::infra::metrics::MiniChatMetrics>,
);

macro_rules! tee {
    ($($name:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {
        impl WorkerMetrics for Tee {
            $(fn $name(&self, $($arg: $ty),*) {
                self.0.$name($($arg),*);
                WorkerMetrics::$name(&*self.1, $($arg),*);
            })*
        }
    };
}

tee!(
    orphan_detected(),
    orphan_finalized(),
    orphan_stream_aborted(),
    orphan_scan_seconds(seconds: f64),
    chat_cleanup_completed(resource: crate::domain::service::workers::metrics::CleanupResource),
    chat_cleanup_failed(resource: crate::domain::service::workers::metrics::CleanupResource),
    chat_cleanup_retry(resource: crate::domain::service::workers::metrics::CleanupResource),
    chat_cleanup_store_with_failed_attachments(),
    chat_secondary_cleanup_skipped(),
    summary_execution(result: crate::domain::service::workers::metrics::SummaryResult),
    summary_cas_conflict(),
    summary_fallback(),
);

/// The fixture.
pub struct WorkerEnv {
    pub m: MutationEnv,
    pub metrics: Arc<RecordingWorkerMetrics>,
    pub storage: Arc<OagwStorageClient>,
    pub directory: Arc<ProviderResolver>,
    pub runtime: Arc<TestRuntime>,
    pub config: MiniChatConfig,
    pub options: WorkerOptions,
    pub otel: Option<Otel>,
    chat_cleanup_slot: Arc<LazyHandler>,
    summary_slot: Arc<LazyHandler>,
}

impl Deref for WorkerEnv {
    type Target = MutationEnv;

    fn deref(&self) -> &MutationEnv {
        &self.m
    }
}

/// A chat cleanup delivery of `(tenant, chat)` at `now`.
pub fn cleanup_message(
    tenant: Uuid,
    chat: Uuid,
    now: OffsetDateTime,
    attempts: i16,
) -> OutboxMessage {
    let payload = ChatCleanupPayload::new(tenant, chat, now);
    outbox_message(payload.encode().unwrap(), attempts)
}

/// `BASE_UNIX + secs` as a time.
pub fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap()
}

/// The boundary of message `id` created `secs` after the fixture epoch.
pub fn boundary(id: Uuid, secs: i64) -> MessageBoundary {
    MessageBoundary {
        created_at: at(secs),
        id,
    }
}

impl WorkerEnv {
    pub async fn new() -> Self {
        Self::with(WorkerOptions::default()).await
    }

    pub async fn with(options: WorkerOptions) -> Self {
        let chat_cleanup_slot = Arc::new(LazyHandler::default());
        let summary_slot = Arc::new(LazyHandler::default());
        let m = MutationEnv::with(StreamOptions {
            catalog: worker_catalog(),
            providers: provider_entries(),
            switches: options.switches,
            ping: options.ping,
            file_db: options.file_db,
            thread_summary_worker: options.summary.clone(),
            chat_cleanup: options
                .real_pipeline
                .then(|| Arc::clone(&chat_cleanup_slot) as Arc<dyn LeasedMessageHandler>),
            thread_summary: options
                .real_pipeline
                .then(|| Arc::clone(&summary_slot) as Arc<dyn LeasedMessageHandler>),
            ..StreamOptions::default()
        })
        .await;
        let mut config = MiniChatConfig::default();
        config.cleanup_worker.max_attempts = options.max_cleanup_attempts;
        config.thread_summary_worker = options.summary.clone();
        config.orphan_watchdog.timeout_secs = options.orphan_timeout_secs;
        let mut entries = serde_json::Map::new();
        entries.insert("openai".to_owned(), provider_json(ALIAS, "openai"));
        for (id, entry) in provider_entries() {
            entries.insert(id.to_owned(), entry);
        }
        let directory = Arc::new(ProviderResolver::new(&crate::test_doubles::registry(
            &Value::Object(entries),
        )));
        let storage = Arc::new(OagwStorageClient::new(
            m.oagw.clone() as Arc<dyn oagw_sdk::ServiceGatewayClientV1>
        ));
        let this = Self {
            m,
            metrics: RecordingWorkerMetrics::new(),
            storage,
            directory,
            runtime: if options.unbound_runtime {
                TestRuntime::unbound()
            } else {
                TestRuntime::bound()
            },
            config,
            options: options.clone(),
            otel: options.real_metrics.then(|| {
                use opentelemetry::metrics::MeterProvider as _;
                use opentelemetry_sdk::metrics::{
                    InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
                };
                let exporter = InMemoryMetricExporter::default();
                let provider = SdkMeterProvider::builder()
                    .with_reader(PeriodicReader::builder(exporter.clone()).build())
                    .build();
                let metrics = Arc::new(crate::infra::metrics::MiniChatMetrics::new(
                    &provider.meter("mini_chat"),
                    "",
                ));
                Otel {
                    provider,
                    exporter,
                    metrics,
                }
            }),
            chat_cleanup_slot,
            summary_slot,
        };
        if this.options.real_pipeline {
            this.chat_cleanup_slot
                .set(Arc::new(this.chat_cleanup_handler()));
            this.summary_slot.set(Arc::new(this.summary_handler()));
        }
        this
    }

    // ---- logic builders ------------------------------------------------------------------

    /// The metrics sink of the workers: the recording one, and the real
    /// instruments too when `real_metrics` is set.
    pub fn sink(&self) -> Arc<dyn WorkerMetrics> {
        match &self.otel {
            Some(otel) => Arc::new(Tee(Arc::clone(&self.metrics), Arc::clone(&otel.metrics))),
            None => Arc::clone(&self.metrics) as Arc<dyn WorkerMetrics>,
        }
    }

    /// The wall clock of the fixture.
    pub fn wall(&self) -> Clock {
        self.m.clock_fn()
    }

    /// The chat cleanup logic over `storage` and `timer` with the fixture's
    /// stores, directory and policy.
    pub fn chat_cleanup_service_with(
        &self,
        storage: Arc<dyn StorageClient>,
        timer: SharedWorkerClock,
    ) -> ChatCleanupService {
        self.chat_cleanup_service_budget(storage, timer, CHAT_CLEANUP_DELIVERY_BUDGET)
    }

    /// Like [`Self::chat_cleanup_service_with`] with an explicit delivery budget.
    pub fn chat_cleanup_service_budget(
        &self,
        storage: Arc<dyn StorageClient>,
        timer: SharedWorkerClock,
        delivery_budget: Duration,
    ) -> ChatCleanupService {
        ChatCleanupService::new(ChatCleanupDeps {
            store: Arc::new(SeaChatCleanupStore::new(Arc::clone(&self.m.quota.provider))),
            storage,
            directory: Arc::clone(&self.directory) as Arc<dyn StorageDirectory>,
            policy: Arc::clone(&self.m.quota.policy) as Arc<dyn PolicyCatalog>,
            providers: Arc::clone(&self.directory) as Arc<dyn ProviderDirectory>,
            metrics: self.sink(),
            runtime: Arc::clone(&self.runtime) as Arc<dyn BackgroundRuntime>,
            clock: self.wall(),
            timer,
            max_attempts: self.options.max_cleanup_attempts,
            delete_timeout: Duration::from_secs(10),
            delivery_budget,
        })
    }

    /// The chat cleanup logic over the OAGW storage client and tokio time.
    pub fn chat_cleanup_service(&self) -> ChatCleanupService {
        self.chat_cleanup_service_with(
            Arc::clone(&self.storage) as Arc<dyn StorageClient>,
            Arc::new(TokioWorkerClock::new()),
        )
    }

    /// The chat cleanup handler of the fixture.
    pub fn chat_cleanup_handler(&self) -> ChatCleanupHandler {
        ChatCleanupHandler::new(self.chat_cleanup_service())
    }

    /// Delivers the chat cleanup message of `chat` for the `attempts`-th time.
    pub async fn deliver_cleanup(&self, chat: Uuid, attempts: i16) -> MessageResult {
        self.chat_cleanup_handler()
            .handle(&cleanup_message(USER_TENANT, chat, self.now(), attempts))
            .await
    }

    /// The summary logic over `client` and `timer`.
    pub fn summary_service_with(
        &self,
        client: Arc<dyn SummaryModelClient>,
        timer: SharedWorkerClock,
        config: ThreadSummaryWorkerConfig,
    ) -> SummaryService {
        SummaryService::new(SummaryDeps {
            store: Arc::new(SeaSummaryStore::new(
                Arc::clone(&self.m.quota.provider),
                Arc::clone(&self.m.quota.enqueuer),
            )) as Arc<dyn SummaryStore>,
            policy: Arc::clone(&self.m.quota.policy) as Arc<dyn PolicyCatalog>,
            providers: Arc::clone(&self.directory) as Arc<dyn ProviderDirectory>,
            client,
            metrics: self.sink(),
            runtime: Arc::clone(&self.runtime) as Arc<dyn BackgroundRuntime>,
            clock: self.wall(),
            timer,
            config,
        })
    }

    /// The summary logic over the OAGW summary client and tokio time.
    pub fn summary_service(&self) -> SummaryService {
        self.summary_service_with(
            Arc::new(OagwSummaryClient::new(self.m.oagw.clone())),
            Arc::new(TokioWorkerClock::new()),
            self.options.summary.clone(),
        )
    }

    /// The thread-summary handler of the fixture.
    pub fn summary_handler(&self) -> ThreadSummaryHandler {
        ThreadSummaryHandler::new(self.summary_service())
    }

    /// Delivers `task` for the `attempts`-th time.
    pub async fn deliver_summary(&self, task: &SummaryTask, attempts: i16) -> MessageResult {
        self.summary_handler()
            .handle(&outbox_message(encode_task(task).unwrap(), attempts))
            .await
    }

    /// The summary logic result of `task` (without the outbox mapping).
    pub async fn handle_summary(&self, task: &SummaryTask, attempts: i16) -> HandlerOutcome {
        self.summary_service().handle(task, attempts).await
    }

    /// The orphan watchdog over the fixture's stores and the wall clock.
    pub fn watchdog(&self, elector: Arc<dyn LeaderElector>) -> OrphanWatchdog {
        self.watchdog_with(elector, self.wall())
    }

    pub fn watchdog_with(&self, elector: Arc<dyn LeaderElector>, clock: Clock) -> OrphanWatchdog {
        self.watchdog_over(self.orphan_store(), elector, clock)
    }

    /// The real orphan store of the fixture.
    pub fn orphan_store(&self) -> Arc<dyn OrphanStore> {
        let quota = &self.m.quota;
        Arc::new(SeaOrphanStore::new(
            Arc::clone(&quota.provider),
            Arc::clone(&quota.repo),
            Arc::clone(&quota.enqueuer),
        ))
    }

    /// A watchdog over `store` (a wrapper of the real one, or a fake).
    pub fn watchdog_over(
        &self,
        store: Arc<dyn OrphanStore>,
        elector: Arc<dyn LeaderElector>,
        clock: Clock,
    ) -> OrphanWatchdog {
        let quota = &self.m.quota;
        let mut orphan_config = self.config.orphan_watchdog.clone();
        orphan_config.timeout_secs = self.options.orphan_timeout_secs;
        OrphanWatchdog::new(WatchdogDeps {
            store,
            quota: Arc::clone(&quota.service),
            elector,
            metrics: self.sink(),
            config: orphan_config,
            clock,
        })
    }

    /// The summary task of `(base, target)` of `chat` with a fixed system request id.
    pub fn task(chat: Uuid, base: Option<MessageBoundary>, target: MessageBoundary) -> SummaryTask {
        SummaryTask {
            tenant_id: USER_TENANT,
            chat_id: chat,
            system_request_id: Uuid::from_u128(0x5157_0001),
            base,
            target,
        }
    }

    /// The OpenAI-compatible routes answer like a healthy provider.
    pub fn script_happy(&self) {
        script_happy_on(&self.m.oagw);
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// Deletes `chat` through `DELETE /v1/chats/{id}` as the product does: the
    /// chat is soft-deleted, its attachments are marked for cleanup and the
    /// chat-cleanup message is enqueued, all in one transaction.
    pub async fn delete_chat_via_api(&self, chat: Uuid) {
        let reply = self
            .m
            .call(
                "DELETE",
                &crate::chat_test_env::chat_uri(chat),
                &crate::test_doubles::user_ctx(),
                None,
            )
            .await;
        assert_eq!(reply.status.as_u16(), 204, "{}", reply.text);
    }

    /// The payload bytes of the `n`th chat-cleanup message in the outbox.
    pub async fn cleanup_payload(&self, n: usize) -> Vec<u8> {
        self.m.quota.outbox_rows(CHAT_CLEANUP_QUEUE).await[n]
            .payload
            .clone()
    }

    /// An image of `chat` with an uploaded Anthropic secondary copy.
    pub async fn seed_image_with_secondary(&self, chat: Uuid, n: u128, secondary: &str) -> Uuid {
        self.m
            .seed_attachment(chat, n, |row| {
                row.set("storage_backend", "openai")
                    .set("attachment_kind", "image")
                    .set("filename", format!("pic{n}.png"))
                    .set("content_type", "image/png")
                    .set("for_file_search", false)
                    .set("secondary_status", "uploaded")
                    .set("secondary_file_id", secondary)
                    .set("secondary_provider_kind", "anthropic")
            })
            .await
    }

    /// Ids of every attachment of `chat`, oldest first.
    pub async fn att_ids_of(&self, chat: Uuid) -> Vec<Uuid> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        self.m
            .quota
            .db
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT id FROM attachments WHERE chat_id = {} ORDER BY created_at, id",
                    blob(chat)
                ),
            ))
            .await
            .unwrap()
            .iter()
            .map(|r| Uuid::from_slice(&r.try_get::<Vec<u8>>("", "id").unwrap()).unwrap())
            .collect()
    }

    /// Marks an attachment owed provider cleanup (the chat soft delete does it
    /// for every attachment of the chat).
    pub async fn mark_pending(&self, id: Uuid) {
        self.exec(&format!(
            "UPDATE attachments SET cleanup_status = 'pending', cleanup_updated_at = '{}' WHERE id = {}",
            ts(1),
            blob(id)
        ))
        .await;
    }

    /// Every row and column of `attachments`, ordered, as one string.
    pub async fn attachments_dump(&self) -> String {
        self.m.quota.dump("attachments").await
    }

    /// A cloneable executor of raw statements.
    pub fn sql(&self) -> Sql {
        Sql(self.m.quota.db.raw.clone())
    }

    /// Executes a statement on the raw connection.
    pub async fn exec(&self, sql: &str) {
        self.m
            .quota
            .db
            .raw
            .execute_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql.to_owned(),
            ))
            .await
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }

    /// Soft-deletes `chat` directly (the state the cleanup handler requires).
    pub async fn soft_delete_chat(&self, chat: Uuid) {
        self.m
            .quota
            .db
            .raw
            .execute_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                format!(
                    "UPDATE chats SET deleted_at = '{}' WHERE id = {}",
                    ts(5),
                    blob(chat)
                ),
            ))
            .await
            .unwrap();
    }

    /// An attachment of `chat` owed provider cleanup: `cleanup_status`
    /// `pending`, provider file `file-prov{n}` in the `openai` backend.
    pub async fn owed(&self, chat: Uuid, n: u128) -> Uuid {
        self.owed_with(chat, n, |row| row).await
    }

    pub async fn owed_with(&self, chat: Uuid, n: u128, edit: impl FnOnce(Row) -> Row) -> Uuid {
        self.m
            .seed_attachment(chat, n, |row| {
                edit(
                    row.set("storage_backend", "openai")
                        .set("cleanup_status", "pending")
                        .set("cleanup_updated_at", ts(1))
                        .set("deleted_at", ts(5)),
                )
            })
            .await
    }

    /// A vector store row; `None` is a placeholder.
    pub async fn store_row(
        &self,
        chat: Uuid,
        vector_store_id: Option<&str>,
        provider: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = Row::default()
            .set("id", id)
            .set("tenant_id", USER_TENANT)
            .set("chat_id", chat)
            .set("provider", provider)
            .set("file_count", 0_i32)
            .set("created_at", ts(0));
        row = match vector_store_id {
            Some(store) => row.set("vector_store_id", store),
            None => row.null("vector_store_id"),
        };
        self.m
            .quota
            .db
            .insert("chat_vector_stores", row)
            .await
            .unwrap();
        id
    }

    /// Seeds `turns` completed turns `1..=turns` at `10 * n` seconds in the
    /// default chat: the user message at `10n`, the assistant message at `10n + 1`.
    pub async fn conversation(&self, turns: u128) -> Vec<crate::mutation_test_env::Seeded> {
        let mut seeded = Vec::new();
        for n in 1..=turns {
            let secs = i64::try_from(n).unwrap() * 10;
            seeded.push(self.m.seed_turn(&SeedTurn::completed(n, secs)).await);
        }
        seeded
    }

    // ---- readers ---------------------------------------------------------------------------

    pub async fn att(&self, id: Uuid) -> AttSnap {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        let row = self
            .m
            .quota
            .db
            .raw
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT status, cleanup_status, cleanup_attempts, last_cleanup_error, \
                     cleanup_updated_at, deleted_at, provider_file_id, secondary_file_id \
                     FROM attachments WHERE id = {}",
                    blob(id)
                ),
            ))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("attachment {id} does not exist"));
        let text = |c: &str| row.try_get::<Option<String>>("", c).unwrap();
        AttSnap {
            status: text("status").unwrap(),
            cleanup_status: text("cleanup_status"),
            cleanup_attempts: row.try_get::<i64>("", "cleanup_attempts").unwrap(),
            last_cleanup_error: text("last_cleanup_error"),
            cleanup_updated_at: text("cleanup_updated_at"),
            deleted_at: text("deleted_at"),
            provider_file_id: text("provider_file_id"),
            secondary_file_id: text("secondary_file_id"),
        }
    }

    /// `chat_vector_stores` rows as `(vector_store_id, provider)`.
    pub async fn stores(&self) -> Vec<(Option<String>, String)> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        self.m
            .quota
            .db
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT vector_store_id, provider FROM chat_vector_stores ORDER BY created_at, id"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.try_get::<Option<String>>("", "vector_store_id").unwrap(),
                    r.try_get::<String>("", "provider").unwrap(),
                )
            })
            .collect()
    }

    /// The thread summary of `chat`.
    pub async fn summary(&self, chat: Uuid) -> Option<SummarySnap> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        let row = self
            .m
            .quota
            .db
            .raw
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT summary_text, summarized_up_to_created_at, summarized_up_to_message_id, \
                     token_estimate, created_at, updated_at FROM thread_summaries WHERE chat_id = {}",
                    blob(chat)
                ),
            ))
            .await
            .unwrap()?;
        Some(SummarySnap {
            text: row.try_get("", "summary_text").unwrap(),
            frontier_at: row.try_get("", "summarized_up_to_created_at").unwrap(),
            frontier_id: Uuid::from_slice(
                &row.try_get::<Vec<u8>>("", "summarized_up_to_message_id")
                    .unwrap(),
            )
            .unwrap(),
            token_estimate: row.try_get("", "token_estimate").unwrap(),
            created_at: row.try_get("", "created_at").unwrap(),
            updated_at: row.try_get("", "updated_at").unwrap(),
        })
    }

    /// Ids of the messages of `chat` flagged `is_compressed`, in order.
    pub async fn compressed_ids(&self, chat: Uuid) -> Vec<Uuid> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        self.m
            .quota
            .db
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!(
                    "SELECT id FROM messages WHERE chat_id = {} AND is_compressed = 1 \
                     ORDER BY created_at, id",
                    blob(chat)
                ),
            ))
            .await
            .unwrap()
            .iter()
            .map(|r| Uuid::from_slice(&r.try_get::<Vec<u8>>("", "id").unwrap()).unwrap())
            .collect()
    }

    /// The decoded thread-summary work items in the outbox.
    pub async fn summary_messages(&self) -> Vec<Value> {
        self.m
            .quota
            .outbox_rows(SUMMARY_QUEUE)
            .await
            .iter()
            .map(crate::chat_test_env::OutboxRow::json)
            .collect()
    }

    /// The decoded chat cleanup messages in the outbox.
    pub async fn cleanup_queue(&self) -> Vec<Value> {
        self.m
            .quota
            .outbox_rows(CHAT_CLEANUP_QUEUE)
            .await
            .iter()
            .map(crate::chat_test_env::OutboxRow::json)
            .collect()
    }

    /// Usage events of the usage queue.
    pub async fn usage(&self) -> Vec<UsageEvent> {
        self.m.usage_events().await
    }

    /// Audit events of the audit queue.
    pub async fn audits(&self) -> Vec<TurnAuditEvent> {
        self.m.audit_events().await
    }

    /// Every table the workers must not touch for a chat deletion, as one
    /// string: messages, turns and chats.
    pub async fn untouched_snapshot(&self) -> String {
        let mut out = String::new();
        for table in ["chats", "messages", "chat_turns"] {
            out.push_str(&self.m.quota.dump(table).await);
            out.push('\n');
        }
        out
    }

    /// The paths of the DELETE calls to the provider, in order.
    pub fn delete_paths(&self) -> Vec<String> {
        self.m
            .oagw
            .proxied()
            .iter()
            .filter(|call| call.method == "DELETE")
            .map(|call| call.uri.split('?').next().unwrap_or_default().to_owned())
            .collect()
    }

    /// Polls an async condition (15 s at most).
    pub async fn wait_for<F, Fut>(&self, what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..1500 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }
}

/// `USER` re-exported for brevity.
pub const DEFAULT_USER: Uuid = USER;
