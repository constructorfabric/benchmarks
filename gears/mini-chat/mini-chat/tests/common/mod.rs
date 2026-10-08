//! In-process test harness: the domain service on a temporary SQLite
//! database with a fake OAGW (scripted provider), a fake model policy and
//! the static `AuthZ` PDP.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc
)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, EvaluationRequest, EvaluationResponse, PolicyEnforcer};
use bytes::Bytes;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::authz::Authorizer;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::ports::PolicyProvider;
use mini_chat::domain::service::Service;
use mini_chat::domain::service::cleanup::{
    AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, UsageHandler,
};
use mini_chat::domain::service::stream::{StreamEvent, StreamStart};
use mini_chat::domain::service::summary::ThreadSummaryHandler;
use mini_chat::infra::llm::client::LlmClient;
use mini_chat::infra::llm::gateway::{ProxyClient, S2sContext};
use mini_chat::infra::llm::provider::ProviderRegistry;
use mini_chat::infra::llm::storage::StorageClient;
use mini_chat::infra::metrics::Metrics;
use mini_chat::infra::outbox::{self, Handlers, OutboxEnqueuer};
use mini_chat::infra::storage::migrations::all_migrations;
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits, UsageEvent,
    UserLimits,
};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::models::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::outbox::OutboxHandle;
use toolkit_security::{PlatformSecurityContext, SecurityContext};
use uuid::Uuid;

pub const TENANT_A: Uuid = Uuid::from_u128(0xa);
pub const TENANT_B: Uuid = Uuid::from_u128(0xb);
pub const USER_A1: Uuid = Uuid::from_u128(0xa1);
pub const USER_A2: Uuid = Uuid::from_u128(0xa2);
pub const USER_B1: Uuid = Uuid::from_u128(0xb1);

pub fn ctx(tenant: Uuid, user: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

/// Static PDP adapter.
pub struct StaticPdp(static_authz_plugin::domain::service::Service);

#[async_trait]
impl AuthZResolverApi for StaticPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(self.0.evaluate(&req))
    }
}

/// One scripted provider answer.
#[derive(Clone)]
pub enum Reply {
    /// SSE events (JSON data objects).
    Events(Vec<Value>),
    /// HTTP error with a JSON body.
    Status(u16, Value),
    /// Send one event then never finish.
    Hang,
    /// A non-streaming JSON body.
    Json(Value),
}

/// A recorded proxy request.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub uri: String,
    pub body: Bytes,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Fake OAGW: answers proxy requests like an OpenAI-compatible provider.
#[derive(Default)]
pub struct FakeGateway {
    pub script: Mutex<VecDeque<Reply>>,
    pub requests: Mutex<Vec<Recorded>>,
    pub index_status: Mutex<String>,
}

impl FakeGateway {
    pub fn push(&self, r: Reply) {
        self.script.lock().push_back(r);
    }

    pub fn requests_to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests
            .lock()
            .iter()
            .filter(|r| {
                r.uri
                    .split('?')
                    .next()
                    .unwrap_or_default()
                    .ends_with(suffix)
            })
            .cloned()
            .collect()
    }
}

pub fn text_events(text: &str, input: i64, output: i64) -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_text.delta", "item_id": "m", "delta": text}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": input, "output_tokens": output}}}),
    ]
}

fn json_response(status: u16, v: &Value) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::Bytes(Bytes::from(v.to_string())))
        .unwrap()
}

fn sse_response(events: Vec<Value>, hang: bool) -> http::Response<Body> {
    let chunks: Vec<Result<Bytes, oagw_sdk::body::BoxError>> = events
        .into_iter()
        .map(|e| Ok(Bytes::from(format!("data: {e}\n\n"))))
        .collect();
    let stream = futures::stream::iter(chunks);
    let body: oagw_sdk::body::BodyStream = if hang {
        Box::pin(futures::StreamExt::chain(
            stream,
            futures::stream::pending(),
        ))
    } else {
        Box::pin(stream)
    };
    http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::Stream(body))
        .unwrap()
}

fn unused<T>() -> Result<T, CanonicalError> {
    Err(CanonicalError::internal("not used in tests").create())
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeGateway {
    async fn create_upstream(
        &self,
        _: SecurityContext,
        _: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unused()
    }
    async fn get_upstream(&self, _: SecurityContext, _: Uuid) -> Result<Upstream, CanonicalError> {
        unused()
    }
    async fn list_upstreams(
        &self,
        _: SecurityContext,
        _: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        unused()
    }
    async fn update_upstream(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unused()
    }
    async fn delete_upstream(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        unused()
    }
    async fn create_route(
        &self,
        _: SecurityContext,
        _: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unused()
    }
    async fn get_route(&self, _: SecurityContext, _: Uuid) -> Result<Route, CanonicalError> {
        unused()
    }
    async fn list_routes(
        &self,
        _: SecurityContext,
        _: Option<Uuid>,
        _: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        unused()
    }
    async fn update_route(
        &self,
        _: SecurityContext,
        _: Uuid,
        _: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unused()
    }
    async fn delete_route(&self, _: SecurityContext, _: Uuid) -> Result<(), CanonicalError> {
        unused()
    }
    async fn resolve_proxy_target(
        &self,
        _: SecurityContext,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unused()
    }

    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let body = body.into_bytes().await.unwrap_or_default();
        let method = parts.method.to_string();
        let uri = parts.uri.to_string();
        self.requests.lock().push(Recorded {
            method: method.clone(),
            uri: uri.clone(),
            body: body.clone(),
        });
        let path = uri.split('?').next().unwrap_or_default().to_owned();
        if path.ends_with("/responses") {
            let req_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = self.script.lock().pop_front();
            return Ok(match reply {
                Some(Reply::Status(s, v)) => json_response(s, &v),
                Some(Reply::Json(v)) => json_response(200, &v),
                Some(Reply::Hang) => sse_response(
                    vec![json!({"type": "response.created", "response": {"id": "r"}})],
                    true,
                ),
                Some(Reply::Events(ev)) => sse_response(ev, false),
                None if req_json["stream"] == json!(false) => json_response(
                    200,
                    &json!({"output": [{"type": "message", "content": [{"type": "output_text",
                        "text": "<analysis>x</analysis><summary>The summary.</summary>"}]}],
                        "usage": {"input_tokens": 10, "output_tokens": 7}}),
                ),
                None => sse_response(text_events("Hello", 11, 7), false),
            });
        }
        let id = Uuid::new_v4().simple().to_string();
        Ok(match method.as_str() {
            "DELETE" => json_response(200, &json!({"deleted": true})),
            "GET" => json_response(200, &json!({"status": self.index_status.lock().clone()})),
            _ if path.ends_with("/files") && path.contains("/vector_stores/") => {
                json_response(200, &json!({"status": self.index_status.lock().clone()}))
            }
            _ if path.ends_with("/files") => {
                json_response(200, &json!({"id": format!("file-{id}")}))
            }
            _ if path.ends_with("/vector_stores") => {
                json_response(200, &json!({"id": format!("vs_{id}")}))
            }
            _ => json_response(404, &json!({"error": {"message": "not found"}})),
        })
    }
}

/// Fake model policy.
pub struct FakePolicy {
    pub snapshot: Mutex<PolicySnapshot>,
    pub limits: Mutex<(TierLimits, TierLimits)>,
    pub published: Mutex<Vec<UsageEvent>>,
}

#[async_trait]
impl PolicyProvider for FakePolicy {
    async fn current_snapshot(&self, _: Uuid) -> Result<PolicySnapshot, DomainError> {
        Ok(self.snapshot.lock().clone())
    }
    async fn snapshot(&self, _: Uuid, _: u64) -> Result<PolicySnapshot, DomainError> {
        Ok(self.snapshot.lock().clone())
    }
    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let (standard, premium) = *self.limits.lock();
        Ok(UserLimits {
            user_id,
            policy_version: version,
            standard,
            premium,
        })
    }
    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        self.published.lock().push(event);
        Ok(())
    }
}

pub fn model(id: &str, tier: &str, enabled: bool, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": format!("You are {id}."),
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "general_config": {"tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": default, "sort_order": 0}
    }))
    .unwrap()
}

/// A running service with its fakes.
pub struct Harness {
    pub url: String,
    pub svc: Arc<Service>,
    pub gw: Arc<FakeGateway>,
    pub policy: Arc<FakePolicy>,
    pub outbox: Option<OutboxHandle>,
    _dir: tempfile::TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_config(|_| {}).await
    }

    pub async fn with_config(tweak: impl FnOnce(&mut MiniChatConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mini_chat.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = toolkit_db::connect_db(
            &format!("{url}&busy_timeout=5000&journal_mode=WAL"),
            toolkit_db::ConnectOpts::default(),
        )
        .await
        .unwrap();
        toolkit_db::migration_runner::run_migrations_for_testing(&db, all_migrations())
            .await
            .unwrap();
        let mut cfg: MiniChatConfig = serde_json::from_value(json!({
            "client_credentials": {"client_id": "mc", "client_secret": "s"},
            "providers": {"openai": {"kind": "openai_responses", "host": "127.0.0.1", "port": 9,
                                     "use_http": true, "storage_kind": "openai"}},
            "orphan_watchdog": {"enabled": false},
            "upload_reaper": {"enabled": false},
        }))
        .unwrap();
        tweak(&mut cfg);
        cfg.validate().unwrap();
        let cfg = Arc::new(cfg);
        let gw = Arc::new(FakeGateway::default());
        "completed".clone_into(&mut gw.index_status.lock());
        let policy = Arc::new(FakePolicy {
            snapshot: Mutex::new(PolicySnapshot {
                policy_version: 1,
                model_catalog: vec![
                    model("prem", "premium", true, true),
                    model("std", "standard", true, false),
                    model("gpt-4.1-mini", "standard", true, false),
                    model("off", "standard", false, false),
                ],
                kill_switches: KillSwitches::default(),
            }),
            limits: Mutex::new((
                TierLimits {
                    limit_daily_credits_micro: 100_000_000,
                    limit_monthly_credits_micro: 1_000_000_000,
                },
                TierLimits {
                    limit_daily_credits_micro: 50_000_000,
                    limit_monthly_credits_micro: 500_000_000,
                },
            )),
            published: Mutex::new(Vec::new()),
        });
        let s2s = Arc::new(S2sContext::default());
        s2s.set(ctx(TENANT_A, USER_A1));
        let gateway: Arc<dyn ServiceGatewayClientV1> = gw.clone();
        let proxy = ProxyClient::new(gateway, s2s);
        let pdp: Arc<dyn AuthZResolverApi> = Arc::new(StaticPdp(
            static_authz_plugin::domain::service::Service::new(),
        ));
        let svc = Arc::new(Service {
            cfg: Arc::clone(&cfg),
            db: db.clone(),
            authz: Authorizer::new(PolicyEnforcer::new(pdp)),
            policy: policy.clone(),
            audit: None,
            providers: Arc::new(ProviderRegistry::new(&cfg.providers)),
            llm: LlmClient::new(proxy.clone()),
            storage: StorageClient::new(proxy),
            outbox: Arc::new(OutboxEnqueuer::new(&cfg.outbox)),
            upload_slots: Arc::new(Semaphore::new(4)),
            shutdown: CancellationToken::new(),
            metrics: Arc::new(Metrics::global("mini_chat_test")),
        });
        let handle = outbox::start(
            db,
            &svc.outbox,
            Handlers {
                usage: Arc::new(UsageHandler(Arc::clone(&svc))),
                attachment_cleanup: Arc::new(AttachmentCleanupHandler(Arc::clone(&svc))),
                chat_cleanup: Arc::new(ChatCleanupHandler(Arc::clone(&svc))),
                thread_summary: Arc::new(ThreadSummaryHandler(Arc::clone(&svc))),
                audit: Arc::new(AuditHandler(Arc::clone(&svc))),
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        Self {
            url,
            svc,
            gw,
            policy,
            outbox: Some(handle),
            _dir: dir,
        }
    }

    pub async fn stop(mut self) {
        if let Some(h) = self.outbox.take() {
            h.stop().await;
        }
    }

    /// Collect all events of a stream start.
    pub async fn collect(start: StreamStart) -> Vec<StreamEvent> {
        match start {
            StreamStart::Replay(ev) => ev,
            StreamStart::Live(mut live) => {
                let _guard = live.cancel.clone().drop_guard();
                let mut out = Vec::new();
                while let Some(ev) = live.events.recv().await {
                    let terminal = ev.is_terminal();
                    out.push(ev);
                    if terminal {
                        break;
                    }
                }
                out
            }
        }
    }

    /// Raw SQL statement.
    pub async fn exec(&self, sql: &str) {
        use sea_orm::ConnectionTrait;
        let conn = sea_orm::Database::connect(&self.url).await.unwrap();
        conn.execute_unprepared(sql).await.unwrap();
    }

    /// Raw SQL query (rows as JSON objects).
    pub async fn rows(&self, sql: &str) -> Vec<Value> {
        use sea_orm::{ConnectionTrait, Statement};
        let conn = sea_orm::Database::connect(&self.url).await.unwrap();
        let res = conn
            .query_all_raw(Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql.to_owned(),
            ))
            .await
            .unwrap();
        res.into_iter()
            .map(|r| {
                let mut obj = serde_json::Map::new();
                for col in r.column_names() {
                    let v: Value = r
                        .try_get::<Option<i64>>("", &col)
                        .map(|v| v.map_or(Value::Null, Value::from))
                        .or_else(|_| {
                            r.try_get::<Option<String>>("", &col)
                                .map(|v| v.map_or(Value::Null, Value::from))
                        })
                        .or_else(|_| {
                            r.try_get::<Option<Vec<u8>>>("", &col)
                                .map(|v| v.map_or(Value::Null, |b| Value::from(hex(&b))))
                        })
                        .unwrap_or(Value::Null);
                    obj.insert(col, v);
                }
                Value::Object(obj)
            })
            .collect()
    }

    pub async fn wait_published(&self, n: usize) -> Vec<UsageEvent> {
        for _ in 0..200 {
            let p = self.policy.published.lock().clone();
            if p.len() >= n {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.policy.published.lock().clone()
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .concat()
}

pub fn uhex(u: Uuid) -> String {
    hex(u.as_bytes())
}
