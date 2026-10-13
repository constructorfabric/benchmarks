#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(
    clippy::format_push_string,
    clippy::inconsistent_struct_constructor,
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::match_wildcard_for_single_variants,
    clippy::struct_excessive_bools
)]
//! Fixture of the streaming-turns end-to-end tests
//! (`cpt-cf-mini-chat-feature-streaming-turns`): a migrated `SQLite` database
//! with the real turn, quota, chat and message stores over a running outbox,
//! the real OAGW provider client over the fake gateway (scripted SSE bodies
//! with gating), the stream service and the whole REST router, plus seeding
//! helpers, readers and an SSE frame parser.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, BodyDataStream, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use futures::StreamExt;
use futures::future::BoxFuture;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::{register_chat_routes, register_stream_routes};
use crate::chat_test_env::{Reply, blob, ts};
use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult, FinalizeWon};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::reserve::ReservedColumns;
use crate::domain::service::quota::service::Clock;
use crate::domain::service::quota::test_support::{NO_SWITCHES, snapshot, with_switches};
use crate::domain::service::stream::pipeline::{StreamDeps, StreamService, StreamSettings};
use crate::domain::service::stream::ports::{
    AttachmentInfo, CreateTurnInput, FinalizeError, FinalizeWrites, HistoryMessage,
    KnowledgeRetriever, MessageBoundary, ProgressUpdate, ProviderClient, ProviderDirectory,
    RecentQuery, StoredAssistantMessage, StreamMetrics, ThreadSummaryInfo, TurnRepository,
};
use crate::domain::service::stream::test_support::RecordingStreamMetrics;
use crate::domain::service::stream::types::{TurnRecord, TurnState};
use crate::infra::db::repos::chats::SeaChatRepository;
use crate::infra::db::repos::messages::SeaMessageRepository;
use crate::infra::db::repos::turns::SeaTurnRepository;
use crate::infra::db::test_support::{Row, attachment_row, chat_row, message_row};
use crate::infra::llm::knowledge_retriever::OagwKnowledgeRetriever;
use crate::infra::llm::oagw_client::OagwProviderClient;
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::quota_test_env::{Options as QuotaOptions, QuotaEnv};
use crate::test_doubles::{
    FakeOagw, PdpReply, USER, USER_TENANT, entry, entry_json, provider_json, registry, user_ctx,
};

/// Default `url_prefix`.
pub const PREFIX: &str = "/mini-chat";
/// Alias of the fake provider upstream.
pub const ALIAS: &str = "api.openai.test";
/// Queue of the turn audit events.
pub const AUDIT_QUEUE: &str = "mini-chat.audit";
/// Alias of the knowledge-search provider upstream (`kp`, an Azure entry).
pub const KNOWLEDGE_ALIAS: &str = "kb.azure.test";
/// The vector store of the knowledge-search configuration.
pub const KNOWLEDGE_STORE: &str = "vs_kb";

/// The clock of the fixture: noon of 2026-03-10 plus 250 001 microseconds,
/// advanced by whole seconds by the tests.
///
/// `SQLite` stores a timestamp as RFC 3339 text without trailing zeros of the
/// fraction, and compares it as text: `…:00.25Z` sorts after `…:00.250001Z`.
/// The assistant message is stamped one microsecond after its question, so the
/// fixture keeps a six-digit fraction with a non-zero last digit: both
/// timestamps then have the same length and sort chronologically (on
/// `PostgreSQL` the column type orders them anyway).
pub fn base_time() -> OffsetDateTime {
    crate::domain::service::quota::test_support::noon() + time::Duration::microseconds(250_001)
}

/// A hook awaited after the (real) reserve transaction committed.
pub type AfterCreate = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// A turn store that delegates to another and can run a hook after the
/// reserve committed.
pub struct HookedTurns {
    inner: Arc<dyn TurnRepository>,
    pub after_create: Mutex<Option<AfterCreate>>,
}

#[async_trait]
impl TurnRepository for HookedTurns {
    async fn find_by_request_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        self.inner
            .find_by_request_id(scope, chat_id, request_id)
            .await
    }

    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        self.inner.find_running(scope, chat_id).await
    }

    async fn load_assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        self.inner
            .load_assistant_message(scope, chat_id, message_id)
            .await
    }

    async fn request_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        self.inner.request_attachments(scope, chat_id, ids).await
    }

    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        self.inner.ready_attachments(scope, chat_id).await
    }

    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        self.inner.vector_store_id(scope, chat_id).await
    }

    async fn snapshot_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        self.inner.snapshot_boundary(scope, chat_id).await
    }

    async fn thread_summary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryInfo>, DomainError> {
        self.inner.thread_summary(scope, chat_id).await
    }

    async fn recent_messages(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<HistoryMessage>, DomainError> {
        self.inner.recent_messages(scope, chat_id, query).await
    }

    async fn create_turn(
        &self,
        input: &CreateTurnInput<'_>,
    ) -> Result<ReservedColumns, DomainError> {
        let created = self.inner.create_turn(input).await?;
        let hook = self.after_create.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook().await;
        }
        Ok(created)
    }

    async fn finalize(
        &self,
        request: &FinalizeRequest,
        writes: &FinalizeWrites,
    ) -> Result<FinalizeResult, FinalizeError> {
        self.inner.finalize(request, writes).await
    }

    async fn turn_state(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnState>, DomainError> {
        self.inner.turn_state(tenant_id, chat_id, turn_id).await
    }

    async fn record_progress(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
        update: &ProgressUpdate,
    ) -> Result<(), DomainError> {
        self.inner
            .record_progress(tenant_id, chat_id, turn_id, update)
            .await
    }
}

/// Fixture options.
#[derive(Clone)]
pub struct Options {
    /// Answer of the fake PDP.
    pub pdp: PdpReply,
    /// File-backed WAL database with a pool (real concurrency).
    pub file_db: bool,
    /// `streaming.sse_ping_interval_seconds` of the service (also the
    /// liveness interval); below the configured minimum on purpose.
    pub ping: Duration,
    /// `streaming.sse_channel_capacity`.
    pub channel_capacity: usize,
    /// Catalog of the policy plugin.
    pub catalog: Vec<ModelCatalogEntry>,
    /// The audit enqueue of the finalization fails (its queue is not
    /// registered), after the other statements ran.
    pub audit_enqueue_fails: bool,
    /// Kill switches of the snapshot.
    pub switches: KillSwitches,
    /// The metrics sink of the stream service instead of the recording one.
    pub metrics_override: Option<Arc<dyn StreamMetrics>>,
    /// More provider entries `(id, raw entry JSON)` next to `openai`.
    pub providers: Vec<(&'static str, Value)>,
    /// Knowledge search is enabled and wired: provider `kp` (an Azure entry of
    /// kind `openai_responses` at [`KNOWLEDGE_ALIAS`]), vector store
    /// [`KNOWLEDGE_STORE`], the OAGW knowledge retriever.
    pub knowledge: bool,
    /// Handler of the attachment cleanup queue (`None`: the placeholder).
    pub attachment_cleanup: Option<Arc<dyn toolkit_db::outbox::LeasedMessageHandler>>,
    /// Handler of the chat cleanup queue (`None`: the placeholder).
    pub chat_cleanup: Option<Arc<dyn toolkit_db::outbox::LeasedMessageHandler>>,
    /// Handler of the thread-summary queue (`None`: the placeholder).
    pub thread_summary: Option<Arc<dyn toolkit_db::outbox::LeasedMessageHandler>>,
    /// `thread_summary_worker.*` of the stream settings (the trigger).
    pub thread_summary_worker: crate::config::ThreadSummaryWorkerConfig,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pdp: PdpReply::AllowScoped,
            file_db: false,
            ping: Duration::from_secs(60),
            channel_capacity: 8,
            catalog: crate::domain::service::quota::test_support::design_catalog(),
            audit_enqueue_fails: false,
            switches: NO_SWITCHES,
            metrics_override: None,
            providers: Vec::new(),
            knowledge: false,
            attachment_cleanup: None,
            chat_cleanup: None,
            thread_summary: None,
            thread_summary_worker: crate::config::ThreadSummaryWorkerConfig::default(),
        }
    }
}

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub name: String,
    pub data: Value,
    pub raw: String,
}

/// Parses complete frames of an SSE body; comment blocks are skipped.
pub fn parse_frames(text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event: ") {
                name = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data.push_str(value);
            }
        }
        let Some(name) = name else { continue };
        frames.push(Frame {
            name,
            data: serde_json::from_str(&data).unwrap_or(Value::Null),
            raw: block.to_owned(),
        });
    }
    frames
}

/// A fully read response of the send endpoint.
pub struct Streamed {
    pub reply: Reply,
    pub frames: Vec<Frame>,
}

impl Streamed {
    pub fn names(&self) -> Vec<&str> {
        self.frames.iter().map(|f| f.name.as_str()).collect()
    }

    pub fn of(&self, name: &str) -> Vec<&Frame> {
        self.frames.iter().filter(|f| f.name == name).collect()
    }

    /// The content of every `delta` of type `text`, concatenated.
    pub fn text(&self) -> String {
        self.frames
            .iter()
            .filter(|f| f.name == "delta" && f.data["type"] == "text")
            .map(|f| f.data["content"].as_str().unwrap().to_owned())
            .collect()
    }

    pub fn last(&self) -> &Frame {
        self.frames.last().expect("at least one frame")
    }
}

/// An open response read frame by frame; dropping it is the client disconnect.
pub struct OpenStream {
    pub status: StatusCode,
    pub headers: HeaderMap,
    body: BodyDataStream,
    buffer: String,
}

impl OpenStream {
    /// Wraps a response whose body is read frame by frame.
    pub fn from_response(response: axum::http::Response<Body>) -> Self {
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.into_body().into_data_stream(),
            buffer: String::new(),
        }
    }

    /// The next complete frame; `None` when the body ended.
    pub async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(frame) = parse_frames(&block).into_iter().next() {
                    return Some(frame);
                }
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.body.next())
                .await
                .expect("timed out waiting for the next SSE chunk")?;
            self.buffer
                .push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    }

    /// Reads the rest of the body as a plain reply (for a refusal).
    pub async fn into_reply(mut self) -> Reply {
        let mut text = std::mem::take(&mut self.buffer);
        while let Some(chunk) = self.body.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        Reply {
            status: self.status,
            headers: self.headers.clone(),
            json: serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        }
    }

    /// Reads every remaining frame.
    pub async fn rest(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(frame) = self.next_frame().await {
            out.push(frame);
        }
        out
    }

    /// Reads up to and including the first frame named `name`.
    pub async fn until(&mut self, name: &str) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(frame) = self.next_frame().await {
            let hit = frame.name == name;
            out.push(frame);
            if hit {
                return out;
            }
        }
        panic!("the stream ended without a `{name}` frame: {out:?}");
    }
}

/// The fixture.
pub struct StreamEnv {
    pub quota: QuotaEnv,
    pub oagw: Arc<FakeOagw>,
    pub metrics: Arc<RecordingStreamMetrics>,
    pub service: Arc<StreamService>,
    pub turns: Arc<HookedTurns>,
    pub router: Router,
    pub openapi: OpenApiRegistryImpl,
    pub chat: Uuid,
    pub options: Options,
    clock: Arc<Mutex<OffsetDateTime>>,
}

impl StreamEnv {
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    pub async fn with(options: Options) -> Self {
        let quota = QuotaEnv::with(QuotaOptions {
            file_db: options.file_db,
            pdp: options.pdp,
            catalog: options.catalog.clone(),
            now: base_time(),
            attachment_cleanup: options.attachment_cleanup.clone(),
            chat_cleanup: options.chat_cleanup.clone(),
            thread_summary: options.thread_summary.clone(),
            ..QuotaOptions::default()
        })
        .await;
        if options.switches != NO_SWITCHES {
            quota.policy.snapshots.lock().unwrap().insert(
                7,
                with_switches(snapshot(7, options.catalog.clone()), options.switches),
            );
        }
        let oagw = FakeOagw::new();
        let metrics = RecordingStreamMetrics::new();
        let clock = Arc::new(Mutex::new(base_time()));
        let shared_clock = Arc::clone(&clock);
        let clock_fn: Clock = Arc::new(move || *shared_clock.lock().unwrap());

        let audit_enqueuer = if options.audit_enqueue_fails {
            let mismatched = Arc::new(OutboxEnqueuer::new(&OutboxConfig {
                audit_queue_name: "mini-chat.not_registered".to_owned(),
                num_partitions: 4,
                ..OutboxConfig::default()
            }));
            assert!(mismatched.bind(&quota.enqueuer.outbox().expect("the outbox runs")));
            mismatched
        } else {
            Arc::clone(&quota.enqueuer)
        };
        let turn_repo: Arc<dyn TurnRepository> = Arc::new(SeaTurnRepository::new(
            Arc::clone(&quota.provider),
            Arc::clone(&quota.repo),
            audit_enqueuer,
        ));
        let turns = Arc::new(HookedTurns {
            inner: turn_repo,
            after_create: Mutex::new(None),
        });
        let mut entries = serde_json::Map::new();
        entries.insert("openai".to_owned(), provider_json(ALIAS, "openai"));
        for (id, entry) in &options.providers {
            entries.insert((*id).to_owned(), entry.clone());
        }
        if options.knowledge {
            entries.insert("kp".to_owned(), provider_json(KNOWLEDGE_ALIAS, "azure"));
        }
        let providers = Arc::new(ProviderResolver::new(&registry(&Value::Object(entries))));
        let authz = Arc::new(AuthzService::new(
            Arc::clone(&quota.pdp) as Arc<dyn AuthZResolverApi>
        ));
        let chats_repo: Arc<dyn ChatRepository> = Arc::new(SeaChatRepository::new(
            Arc::clone(&quota.provider),
            Arc::clone(&quota.enqueuer),
        ));
        let mut config = MiniChatConfig {
            thread_summary_worker: options.thread_summary_worker.clone(),
            ..MiniChatConfig::default()
        };
        if options.knowledge {
            config.knowledge_search.enabled = true;
            config.knowledge_search.provider_id = Some("kp".to_owned());
            config.knowledge_search.vector_store_id = Some(KNOWLEDGE_STORE.to_owned());
        }
        let mut settings = StreamSettings::from_config(&config);
        settings.ping_interval = options.ping;
        settings.channel_capacity = options.channel_capacity;
        let service = Arc::new(StreamService::new(StreamDeps {
            authz: Arc::clone(&authz),
            chats: Arc::clone(&chats_repo),
            policy: Arc::clone(&quota.policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota.service),
            turns: Arc::clone(&turns) as Arc<dyn TurnRepository>,
            providers: Arc::clone(&providers) as Arc<dyn ProviderDirectory>,
            client: Arc::new(OagwProviderClient::new(oagw.clone())) as Arc<dyn ProviderClient>,
            retriever: options.knowledge.then(|| {
                Arc::new(OagwKnowledgeRetriever::new(
                    oagw.clone(),
                    Arc::clone(&providers) as Arc<dyn ProviderDirectory>,
                    "kp",
                    KNOWLEDGE_STORE,
                )) as Arc<dyn KnowledgeRetriever>
            }),
            metrics: options
                .metrics_override
                .clone()
                .unwrap_or_else(|| Arc::clone(&metrics) as Arc<dyn StreamMetrics>),
            settings,
            clock: clock_fn,
        }));
        let chats = Arc::new(ChatsService::new(
            Arc::clone(&authz),
            Arc::clone(&quota.policy) as Arc<dyn PolicyCatalog>,
            Arc::new(SeaChatRepository::new(
                Arc::clone(&quota.provider),
                Arc::clone(&quota.enqueuer),
            )),
        ));
        let messages = Arc::new(MessagesService::new(
            authz,
            Arc::new(SeaMessageRepository::new(Arc::clone(&quota.provider))),
        ));
        let openapi = OpenApiRegistryImpl::new();
        let router = register_chat_routes(Router::new(), &openapi, chats, messages, PREFIX);
        let router = register_stream_routes(router, &openapi, Arc::clone(&service), PREFIX);
        let chat = seed_chat_row(&quota, 1, USER_TENANT, USER, "p").await;
        Self {
            quota,
            oagw,
            metrics,
            service,
            turns,
            router,
            openapi,
            chat,
            options,
            clock,
        }
    }

    // ---- time -----------------------------------------------------------------------------

    pub fn now(&self) -> OffsetDateTime {
        *self.clock.lock().unwrap()
    }

    /// The clock of the fixture as the `Clock` of a service.
    pub fn clock_fn(&self) -> Clock {
        let shared = Arc::clone(&self.clock);
        Arc::new(move || *shared.lock().unwrap())
    }

    pub fn advance(&self, secs: i64) {
        *self.clock.lock().unwrap() += time::Duration::seconds(secs);
    }

    // ---- requests -------------------------------------------------------------------------

    pub fn stream_uri(chat: Uuid) -> String {
        format!("{PREFIX}/v1/chats/{chat}/messages:stream")
    }

    pub fn turn_uri(chat: Uuid, request: Uuid) -> String {
        format!("{PREFIX}/v1/chats/{chat}/turns/{request}")
    }

    fn request(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<String>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let mut request = builder
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        request.extensions_mut().insert(ctx.clone());
        request
    }

    /// Posts `body` and reads the whole response (the stream must end).
    pub async fn post(&self, chat: Uuid, body: &Value) -> Reply {
        self.post_as(&user_ctx(), chat, body).await
    }

    pub async fn post_as(&self, ctx: &SecurityContext, chat: Uuid, body: &Value) -> Reply {
        self.post_raw(ctx, chat, Some("application/json"), Some(body.to_string()))
            .await
    }

    pub async fn post_raw(
        &self,
        ctx: &SecurityContext,
        chat: Uuid,
        content_type: Option<&str>,
        body: Option<String>,
    ) -> Reply {
        let mut builder = Request::builder()
            .method("POST")
            .uri(Self::stream_uri(chat));
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let mut request = builder
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = tokio::time::timeout(
            Duration::from_secs(20),
            to_bytes(response.into_body(), 64 << 20),
        )
        .await
        .expect("the response did not end")
        .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            headers,
            text,
            json,
        }
    }

    /// Posts to an explicit URI (path errors).
    pub async fn post_raw_uri(&self, uri: &str, body: &Value) -> Reply {
        let request = self.request("POST", uri, &user_ctx(), Some(body.to_string()));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
        Reply {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// Posts and parses the frames of a streamed response.
    pub async fn stream(&self, chat: Uuid, body: &Value) -> Streamed {
        let reply = self.post(chat, body).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        let frames = parse_frames(&reply.text);
        Streamed { reply, frames }
    }

    /// Posts and returns the open response, to be read frame by frame.
    pub async fn open(&self, chat: Uuid, body: &Value) -> OpenStream {
        self.open_as(&user_ctx(), chat, body).await
    }

    pub async fn open_as(&self, ctx: &SecurityContext, chat: Uuid, body: &Value) -> OpenStream {
        let request = self.request("POST", &Self::stream_uri(chat), ctx, Some(body.to_string()));
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.router.clone().oneshot(request),
        )
        .await
        .expect("the response head did not arrive")
        .unwrap();
        OpenStream {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.into_body().into_data_stream(),
            buffer: String::new(),
        }
    }

    pub async fn get(&self, uri: &str) -> Reply {
        self.get_as(&user_ctx(), uri).await
    }

    pub async fn get_as(&self, ctx: &SecurityContext, uri: &str) -> Reply {
        let request = self.request("GET", uri, ctx, None);
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
        Reply {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    pub async fn turn_status(&self, request: Uuid) -> Reply {
        self.get(&Self::turn_uri(self.chat, request)).await
    }

    // ---- the fake provider ---------------------------------------------------------------

    /// Queues a complete SSE answer.
    pub fn script(&self, events: &[(&str, Value)]) {
        self.oagw
            .push_proxy(crate::test_doubles::ProxyReply::sse(events));
    }

    /// Queues a gated answer.
    pub fn gate(&self) -> crate::test_doubles::BodyFeed {
        self.oagw.push_gated()
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// A chat of the default user on `model`.
    pub async fn seed_chat(&self, n: u128, model: &str) -> Uuid {
        seed_chat_row(&self.quota, n, USER_TENANT, USER, model).await
    }

    pub async fn seed_chat_of(&self, n: u128, tenant: Uuid, user: Uuid, model: &str) -> Uuid {
        seed_chat_row(&self.quota, n, tenant, user, model).await
    }

    /// A ready attachment of `chat`.
    pub async fn seed_attachment(
        &self,
        chat: Uuid,
        n: u128,
        edit: impl FnOnce(Row) -> Row,
    ) -> Uuid {
        let id = Uuid::from_u128(0xF100_0000 + n);
        let row = edit(
            attachment_row(id, USER_TENANT, chat)
                .set("uploaded_by_user_id", USER)
                .set("filename", format!("file{n}.pdf"))
                .set("provider_file_id", format!("file-prov{n}"))
                .set("created_at", ts(0))
                .set("updated_at", ts(0)),
        );
        self.quota.db.insert("attachments", row).await.unwrap();
        id
    }

    pub async fn seed_image(&self, chat: Uuid, n: u128) -> Uuid {
        self.seed_attachment(chat, n, |row| {
            row.set("attachment_kind", "image")
                .set("filename", format!("pic{n}.png"))
                .set("for_file_search", false)
        })
        .await
    }

    pub async fn seed_vector_store(&self, chat: Uuid, store: &str) {
        self.quota
            .db
            .insert(
                "chat_vector_stores",
                Row::default()
                    .set("id", Uuid::new_v4())
                    .set("tenant_id", USER_TENANT)
                    .set("chat_id", chat)
                    .set("vector_store_id", store)
                    .set("provider", "openai")
                    .set("file_count", 1_i32)
                    .set("created_at", ts(0)),
            )
            .await
            .unwrap();
    }

    /// A message of `chat` with a request id, `secs` after the fixture's epoch
    /// (an hour before the clock).
    pub async fn seed_message(
        &self,
        chat: Uuid,
        n: u128,
        role: &str,
        content: &str,
        secs: i64,
    ) -> Uuid {
        let id = Uuid::from_u128(0xD100_0000 + n);
        self.quota
            .db
            .insert(
                "messages",
                message_row(id, USER_TENANT, chat)
                    .set("role", role)
                    .set("content", content)
                    .set("request_id", Uuid::from_u128(0xE100_0000 + n))
                    .set("created_at", ts(secs)),
            )
            .await
            .unwrap();
        id
    }

    pub async fn seed_summary(
        &self,
        chat: Uuid,
        text: &str,
        up_to: Uuid,
        up_to_secs: i64,
        estimate: i32,
    ) {
        self.quota
            .db
            .insert(
                "thread_summaries",
                Row::default()
                    .set("id", Uuid::new_v4())
                    .set("tenant_id", USER_TENANT)
                    .set("chat_id", chat)
                    .set("summary_text", text)
                    .set("summarized_up_to_created_at", ts(up_to_secs))
                    .set("summarized_up_to_message_id", up_to)
                    .set("token_estimate", estimate)
                    .set("created_at", ts(up_to_secs))
                    .set("updated_at", ts(up_to_secs)),
            )
            .await
            .unwrap();
    }

    // ---- readers ---------------------------------------------------------------------------

    pub async fn count(&self, table: &str) -> i64 {
        self.quota.db.count(table).await
    }

    pub async fn sql(&self, sql: &str) -> String {
        self.quota.db.query_string(sql).await
    }

    /// `quote(column)` of the turn of `request`; blobs and NULL as SQL text.
    pub async fn turn_col(&self, request: Uuid, column: &str) -> String {
        self.sql(&format!(
            "SELECT quote({column}) FROM chat_turns WHERE request_id = {}",
            blob(request)
        ))
        .await
    }

    /// A text column of the turn of `request`; `None` for NULL.
    pub async fn turn_text(&self, request: Uuid, column: &str) -> Option<String> {
        let quoted = self.turn_col(request, column).await;
        (quoted != "NULL").then(|| quoted.trim_matches('\'').to_owned())
    }

    pub async fn turn_state_of(&self, request: Uuid) -> String {
        self.turn_text(request, "state")
            .await
            .expect("the turn exists")
    }

    pub async fn turn_id_of(&self, request: Uuid) -> Uuid {
        let hex = self
            .sql(&format!(
                "SELECT hex(id) FROM chat_turns WHERE request_id = {}",
                blob(request)
            ))
            .await;
        Uuid::parse_str(&hex).unwrap()
    }

    /// `(role, content, model)` of the messages of `chat` in `(created_at, id)`
    /// order.
    pub async fn messages(&self, chat: Uuid) -> Vec<(String, String, Option<String>)> {
        let joined = self
            .sql(&format!(
                "SELECT COALESCE(group_concat(role || char(31) || content || char(31) || COALESCE(model, '-'), char(30)), '') \
                 FROM (SELECT role, content, model FROM messages WHERE chat_id = {} AND deleted_at IS NULL \
                       ORDER BY created_at, id)",
                blob(chat)
            ))
            .await;
        if joined.is_empty() {
            return Vec::new();
        }
        joined
            .split('\u{1e}')
            .map(|row| {
                let mut parts = row.split('\u{1f}');
                let role = parts.next().unwrap().to_owned();
                let content = parts.next().unwrap().to_owned();
                let model = parts.next().unwrap();
                (role, content, (model != "-").then(|| model.to_owned()))
            })
            .collect()
    }

    /// A column of the assistant message of `request`.
    pub async fn assistant_col(&self, request: Uuid, column: &str) -> String {
        self.sql(&format!(
            "SELECT quote({column}) FROM messages WHERE request_id = {} AND role = 'assistant'",
            blob(request)
        ))
        .await
    }

    pub async fn usage_events(&self) -> Vec<mini_chat_sdk::UsageEvent> {
        self.quota
            .usage_rows()
            .await
            .iter()
            .map(|row| mini_chat_sdk::decode_usage_event(&row.payload).expect("a usage event"))
            .collect()
    }

    pub async fn audit_events(&self) -> Vec<mini_chat_sdk::TurnAuditEvent> {
        self.quota
            .outbox_rows(AUDIT_QUEUE)
            .await
            .iter()
            .map(|row| {
                match mini_chat_sdk::decode_audit_event(&row.payload).expect("an audit event") {
                    mini_chat_sdk::MiniChatAuditEvent::Turn(event) => event,
                    other => panic!("a turn audit event expected, got {other:?}"),
                }
            })
            .collect()
    }

    /// Every table the pipeline writes, as one string.
    pub async fn snapshot(&self) -> String {
        let mut out = String::new();
        for table in [
            "chats",
            "messages",
            "chat_turns",
            "message_attachments",
            "quota_usage",
            "thread_summaries",
        ] {
            out.push_str(&self.quota.dump(table).await);
            out.push('\n');
        }
        out.push_str(&format!(
            "outbox={}",
            self.quota.outbox_message_count().await
        ));
        out
    }

    /// Polls until the turn of `request` is no longer `running`.
    pub async fn wait_terminal(&self, request: Uuid) -> String {
        for _ in 0..1500 {
            let state = self.turn_state_of(request).await;
            if state != "running" {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the turn never left `running`");
    }

    /// Polls `check` until it is true (15 s at most).
    pub async fn wait_until(&self, what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..1500 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// The orphan watchdog wins the CAS of the running turn of `request`.
    pub async fn orphan_finalize(&self, request: Uuid) -> FinalizeResult {
        let turn = self.turn_id_of(request).await;
        let snapshot_v7 = self
            .quota
            .policy
            .snapshots
            .lock()
            .unwrap()
            .get(&7)
            .cloned()
            .unwrap();
        let req = FinalizeRequest::orphan(
            turn,
            self.now() + time::Duration::hours(1),
            SettlementSnapshot::Available(Box::new(snapshot_v7)),
            self.now(),
        );
        self.quota
            .repo
            .finalize_turn(&req)
            .await
            .expect("the orphan finalization")
    }
}

use crate::domain::service::quota::ports::QuotaRepository as _;

async fn seed_chat_row(quota: &QuotaEnv, n: u128, tenant: Uuid, user: Uuid, model: &str) -> Uuid {
    let id = crate::chat_test_env::chat_id(n);
    quota
        .db
        .insert("chats", chat_row(id, tenant, user).set("model", model))
        .await
        .unwrap();
    id
}

#[allow(unused)]
fn unused(_: &FinalizeWon, _: &MessageBoundary) {
    let _ = entry;
    let _ = entry_json;
}

// ---- provider events (OpenAI Responses wire format) -------------------------------------------------------------------

pub mod oa {
    use serde_json::{Value, json};

    pub fn delta(text: &str) -> (&'static str, Value) {
        (
            "response.output_text.delta",
            json!({ "type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": text }),
        )
    }

    pub fn completed(input: u64, output: u64) -> (&'static str, Value) {
        (
            "response.completed",
            json!({ "type": "response.completed", "response": {
                "id": "resp_9f3a1c",
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "input_tokens_details": { "cached_tokens": 3 },
                    "output_tokens_details": { "reasoning_tokens": 2 }
                }
            }}),
        )
    }

    pub fn incomplete(input: u64, output: u64, reason: &str) -> (&'static str, Value) {
        (
            "response.incomplete",
            json!({ "type": "response.incomplete", "response": {
                "id": "resp_inc",
                "incomplete_details": { "reason": reason },
                "usage": { "input_tokens": input, "output_tokens": output }
            }}),
        )
    }

    pub fn failed(message: &str) -> (&'static str, Value) {
        (
            "response.failed",
            json!({ "type": "response.failed", "response": {
                "error": { "code": "invalid_request_error", "message": message },
                "usage": { "input_tokens": 11, "output_tokens": 0 }
            }}),
        )
    }

    pub fn error_event(message: &str) -> (&'static str, Value) {
        (
            "error",
            json!({ "type": "error", "code": "server_error", "message": message }),
        )
    }

    pub fn web_start() -> (&'static str, Value) {
        (
            "response.web_search_call.searching",
            json!({ "type": "response.web_search_call.searching" }),
        )
    }

    pub fn web_done() -> (&'static str, Value) {
        (
            "response.web_search_call.completed",
            json!({ "type": "response.web_search_call.completed" }),
        )
    }

    pub fn file_start() -> (&'static str, Value) {
        (
            "response.file_search_call.searching",
            json!({ "type": "response.file_search_call.searching" }),
        )
    }

    pub fn file_done(results: usize) -> (&'static str, Value) {
        (
            "response.file_search_call.completed",
            json!({ "type": "response.file_search_call.completed", "results": vec![json!({}); results] }),
        )
    }

    pub fn code_start() -> (&'static str, Value) {
        (
            "response.code_interpreter_call.in_progress",
            json!({ "type": "response.code_interpreter_call.in_progress" }),
        )
    }

    pub fn code_done(logs: &str) -> (&'static str, Value) {
        (
            "response.output_item.done",
            json!({ "type": "response.output_item.done", "item": {
                "type": "code_interpreter_call", "id": "ci_1",
                "outputs": [ { "type": "logs", "logs": logs } ]
            }}),
        )
    }

    pub fn annotation(value: Value) -> (&'static str, Value) {
        (
            "response.output_text.annotation.added",
            json!({ "type": "response.output_text.annotation.added", "item_id": "msg_1", "content_index": 0, "annotation": value }),
        )
    }
}

/// A send body with the given content and request id.
pub fn body(content: &str, request: Uuid) -> Value {
    json!({ "content": content, "request_id": request })
}

/// `quote(blob)` as SQLite prints a 16-byte blob: upper-case hex.
pub fn qblob(id: Uuid) -> String {
    format!("X'{}'", id.simple().to_string().to_uppercase())
}

/// The `n`th request id of the tests.
pub fn rid(n: u128) -> Uuid {
    Uuid::from_u128(0xE200_0000 + n)
}
