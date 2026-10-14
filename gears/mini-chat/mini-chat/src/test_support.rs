//! Shared test infrastructure: identities, mock PDPs, SQLite harness, catalog builders and
//! stubs of the platform clients. Compiled for tests only.
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverClient, AuthNResolverError, AuthenticationResult, ClientCredentialsRequest,
};
use authz_resolver_sdk::constraints::{
    Constraint, EqPredicate, InTenantSubtreePredicate, Predicate,
};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigrationTrait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit::context::GearCtx;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, Db, connect_db};
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use types_registry_sdk::testing::MockTypesRegistryClient;
use types_registry_sdk::{
    GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery, TypesRegistryClient,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;

pub mod attach_fakes;
pub mod attachments;
pub mod chats;
pub mod cleanup_fakes;
pub mod maintenance;
pub mod manual_time;
pub mod mutations;
pub mod provider;
pub mod quota;
pub mod s2;
pub mod send;
pub mod storage_mock;
pub mod streaming;
pub mod tool_fakes;

// ── Identities: two tenants, two users in tenant A ───────────────────────────

pub const TENANT_A: Uuid = Uuid::from_u128(0xA000_0000_0000_0000_0000_0000_0000_00A1);
pub const TENANT_B: Uuid = Uuid::from_u128(0xB000_0000_0000_0000_0000_0000_0000_00B1);
pub const USER_A1: Uuid = Uuid::from_u128(0x1111_0000_0000_0000_0000_0000_0000_0001);
pub const USER_A2: Uuid = Uuid::from_u128(0x1111_0000_0000_0000_0000_0000_0000_0002);
pub const USER_B1: Uuid = Uuid::from_u128(0x2222_0000_0000_0000_0000_0000_0000_0001);

pub const SUBJECT_USER_TYPE: &str = "gts.cf.core.security.subject_user.v1~";

/// A security context without a subject type or token scopes.
pub fn ctx(subject_id: Uuid, tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("test ctx")
}

/// A security context with token scopes and an optional subject type.
pub fn ctx_with(
    subject_id: Uuid,
    tenant_id: Uuid,
    subject_type: Option<&str>,
    scopes: &[&str],
) -> SecurityContext {
    let mut builder = SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .token_scopes(scopes.iter().map(|s| (*s).to_owned()).collect());
    if let Some(subject_type) = subject_type {
        builder = builder.subject_type(subject_type);
    }
    builder.build().expect("test ctx")
}

// ── Mock PDP ─────────────────────────────────────────────────────────────────

/// How the mock PDP answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pdp {
    /// `decision: true` with one constraint `owner_tenant_id = subject tenant` when that property
    /// is advertised (what the static PDP does: it never returns an owner predicate).
    TenantOnly,
    /// `decision: true` with `owner_tenant_id` and `owner_id` equality predicates.
    TenantAndOwner,
    /// `decision: false`.
    Deny,
    /// `decision: true` and an empty constraint list.
    NoConstraints,
    /// `decision: true` with a predicate on a property the PEP does not support.
    UnknownProperty,
    /// `decision: true` with a predicate whose capability was never advertised.
    UnadvertisedPredicate,
    /// Two constraints: one on an unknown property, one valid tenant predicate.
    OneBadOneGood,
    /// The evaluation call fails (unreachable PDP).
    Unreachable,
    /// The evaluation call never answers (drives the PEP deadline).
    Hang,
}

/// A mock PDP that records every evaluation request.
pub struct RecordingPdp {
    behavior: Pdp,
    requests: Mutex<Vec<EvaluationRequest>>,
}

impl RecordingPdp {
    pub fn new(behavior: Pdp) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            requests: Mutex::new(Vec::new()),
        })
    }

    /// The requests received so far.
    pub fn requests(&self) -> Vec<EvaluationRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The only request received.
    pub fn only_request(&self) -> EvaluationRequest {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected exactly one PDP call");
        requests.into_iter().next().unwrap()
    }
}

fn tenant_of(request: &EvaluationRequest) -> Uuid {
    request
        .subject
        .properties
        .get("tenant_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .expect("subject.properties[tenant_id]")
}

fn permit(constraints: Vec<Constraint>) -> EvaluationResponse {
    EvaluationResponse {
        decision: true,
        context: EvaluationResponseContext {
            constraints,
            ..Default::default()
        },
    }
}

#[async_trait]
impl AuthZResolverApi for RecordingPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.requests.lock().unwrap().push(request.clone());
        let tenant = tenant_of(&request);
        let eq = |property: &str, value: Uuid| Predicate::Eq(EqPredicate::new(property, value));
        // like the static PDP, only emit predicates on properties the PEP advertised
        let supported = |property: &str| {
            request
                .context
                .supported_properties
                .iter()
                .any(|p| p == property)
        };
        let constraint_of = |predicates: Vec<Predicate>| {
            if predicates.is_empty() {
                vec![]
            } else {
                vec![Constraint { predicates }]
            }
        };
        match self.behavior {
            Pdp::TenantOnly => Ok(permit(constraint_of(
                supported(pep_properties::OWNER_TENANT_ID)
                    .then(|| eq(pep_properties::OWNER_TENANT_ID, tenant))
                    .into_iter()
                    .collect(),
            ))),
            Pdp::TenantAndOwner => Ok(permit(constraint_of(
                [
                    supported(pep_properties::OWNER_TENANT_ID)
                        .then(|| eq(pep_properties::OWNER_TENANT_ID, tenant)),
                    supported(pep_properties::OWNER_ID)
                        .then(|| eq(pep_properties::OWNER_ID, request.subject.id)),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ))),
            Pdp::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            Pdp::NoConstraints => Ok(permit(vec![])),
            Pdp::UnknownProperty => Ok(permit(vec![Constraint {
                predicates: vec![eq("bogus_property", tenant)],
            }])),
            Pdp::UnadvertisedPredicate => Ok(permit(vec![Constraint {
                predicates: vec![Predicate::InTenantSubtree(InTenantSubtreePredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    tenant,
                ))],
            }])),
            Pdp::OneBadOneGood => Ok(permit(vec![
                Constraint {
                    predicates: vec![eq("bogus_property", tenant)],
                },
                Constraint {
                    predicates: vec![eq(pep_properties::OWNER_TENANT_ID, tenant)],
                },
            ])),
            Pdp::Unreachable => Err(CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("pdp unreachable")
                .create()),
            Pdp::Hang => {
                std::future::pending::<()>().await;
                unreachable!("pending never completes")
            }
        }
    }
}

/// An enforcer over a recording mock PDP.
pub fn enforcer(behavior: Pdp) -> (PolicyEnforcer, Arc<RecordingPdp>) {
    let pdp = RecordingPdp::new(behavior);
    (PolicyEnforcer::new(pdp.clone()), pdp)
}

// ── SQLite harness ───────────────────────────────────────────────────────────

/// An isolated in-memory SQLite database: the toolkit `Db`, a provider over it, and a raw
/// connection to the same database for direct SQL assertions (foreign keys on).
pub struct TestDb {
    pub db: Db,
    pub provider: Arc<DBProvider<DomainError>>,
    pub raw: DatabaseConnection,
}

/// Connect a fresh database and run `migrations` through the toolkit migration runner.
pub async fn sqlite_db(migrations: Vec<Box<dyn MigrationTrait>>) -> TestDb {
    let dsn = format!("sqlite:file:mc_{}?mode=memory&cache=shared", Uuid::new_v4());
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect sqlite");
    run_migrations_for_testing(&db, migrations)
        .await
        .expect("run migrations");
    let raw = Database::connect(&dsn).await.expect("connect raw sqlite");
    raw.execute_raw(Statement::from_string(
        raw.get_database_backend(),
        "PRAGMA foreign_keys = ON;",
    ))
    .await
    .expect("enable foreign keys");
    let provider = Arc::new(DBProvider::<DomainError>::new(db.clone()));
    TestDb { db, provider, raw }
}

/// Run one statement on the raw connection.
pub async fn raw_exec(raw: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    raw.execute_raw(Statement::from_string(raw.get_database_backend(), sql))
        .await
        .map(|_| ())
}

/// The first column of the first row as `i64`.
pub async fn raw_i64(raw: &DatabaseConnection, sql: &str) -> i64 {
    let row = raw
        .query_one_raw(Statement::from_string(raw.get_database_backend(), sql))
        .await
        .expect("query")
        .expect("one row");
    row.try_get_by_index::<i64>(0).expect("i64 column")
}

// ── Catalog builders ─────────────────────────────────────────────────────────

/// The JSON of a catalog entry with the required keys only.
pub fn entry_json(id: &str, tier: &str, enabled: bool) -> Value {
    json!({
        "id": id,
        "provider_model_id": format!("provider-{id}"),
        "display_name": format!("Model {id}"),
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "tier": tier,
        "enabled": enabled,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 2_500_000,
        "output_tokens_credit_multiplier_micro": 10_000_000,
        "max_num_results": 5,
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

/// A typed catalog entry; `patch` merges extra keys into the JSON first.
pub fn entry(id: &str, tier: ModelTier, enabled: bool, patch: &Value) -> ModelCatalogEntry {
    let tier = match tier {
        ModelTier::Standard => "standard",
        ModelTier::Premium => "premium",
    };
    let mut value = entry_json(id, tier, enabled);
    if let Some(extra) = patch.as_object() {
        for (key, v) in extra {
            value[key] = v.clone();
        }
    }
    serde_json::from_value(value).expect("valid catalog entry")
}

/// A snapshot of version 1 with the given entries and no kill switches.
pub fn snapshot(entries: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: entries,
        kill_switches: KillSwitches::default(),
    }
}

// ── Fake model-policy gateway ────────────────────────────────────────────────

/// A gateway that serves a fixed snapshot (or fails) and counts its calls.
pub struct FakeGateway {
    snapshot: Option<PolicySnapshot>,
    pub calls: AtomicUsize,
}

impl FakeGateway {
    pub fn serving(snapshot: PolicySnapshot) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Some(snapshot),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            snapshot: None,
            calls: AtomicUsize::new(0),
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn answer(&self) -> Result<PolicySnapshot, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.snapshot
            .clone()
            .ok_or_else(|| DomainError::PolicyUnavailable {
                cause: "fake gateway failure with provider-secret-detail".to_owned(),
            })
    }
}

#[async_trait]
impl ModelPolicyGateway for FakeGateway {
    async fn current_snapshot(&self, _user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        self.answer()
    }

    async fn snapshot_of_version(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.answer()
    }

    async fn user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(DomainError::internal("fake gateway has no user limits"))
    }
}

// ── Types registry that records registrations ────────────────────────────────

/// A `TypesRegistryClient` that records `register` payloads and delegates the reads to a
/// [`MockTypesRegistryClient`].
pub struct RecordingRegistry {
    inner: MockTypesRegistryClient,
    pub registered: Mutex<Vec<Value>>,
    /// Number of `list_instances` calls (plugin lookups).
    pub list_calls: AtomicUsize,
    /// When set, `list_instances` fails (a types-registry resolution error).
    pub fail_list: AtomicBool,
    /// Instances added after construction (a plugin registered later).
    extra_instances: Mutex<Vec<GtsInstance>>,
}

impl RecordingRegistry {
    pub fn new(inner: MockTypesRegistryClient) -> Arc<Self> {
        Arc::new(Self {
            inner,
            registered: Mutex::new(Vec::new()),
            list_calls: AtomicUsize::new(0),
            fail_list: AtomicBool::new(false),
            extra_instances: Mutex::new(Vec::new()),
        })
    }

    pub fn registered(&self) -> Vec<Value> {
        self.registered.lock().unwrap().clone()
    }

    /// Make `instance` visible to `list_instances` from now on.
    pub fn add_instance(&self, instance: GtsInstance) {
        self.extra_instances.lock().unwrap().push(instance);
    }

    pub fn list_call_count(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TypesRegistryClient for RecordingRegistry {
    async fn register(&self, entities: Vec<Value>) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.registered.lock().unwrap().extend(entities.clone());
        Ok(entities
            .iter()
            .map(|entity| RegisterResult::Ok {
                gts_id: entity
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect())
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.inner.register_type_schemas(type_schemas).await
    }

    async fn get_type_schema(&self, type_id: &str) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema(type_id).await
    }

    async fn get_type_schema_by_uuid(
        &self,
        type_uuid: Uuid,
    ) -> Result<GtsTypeSchema, CanonicalError> {
        self.inner.get_type_schema_by_uuid(type_uuid).await
    }

    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas(type_ids).await
    }

    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
        self.inner.get_type_schemas_by_uuid(type_uuids).await
    }

    async fn list_type_schemas(
        &self,
        query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
        self.inner.list_type_schemas(query).await
    }

    async fn register_instances(
        &self,
        instances: Vec<Value>,
    ) -> Result<Vec<RegisterResult>, CanonicalError> {
        self.register(instances).await
    }

    async fn get_instance(&self, id: &str) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance(id).await
    }

    async fn get_instance_by_uuid(&self, uuid: Uuid) -> Result<GtsInstance, CanonicalError> {
        self.inner.get_instance_by_uuid(uuid).await
    }

    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances(ids).await
    }

    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<Uuid>,
    ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
        self.inner.get_instances_by_uuid(uuids).await
    }

    async fn list_instances(
        &self,
        query: InstanceQuery,
    ) -> Result<Vec<GtsInstance>, CanonicalError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("types-registry unavailable")
                .create());
        }
        let mut found = self.inner.list_instances(query).await?;
        found.extend(self.extra_instances.lock().unwrap().iter().cloned());
        Ok(found)
    }
}

// ── Platform client stubs (presence only; S1 never calls them) ───────────────

/// An `AuthNResolverClient` whose methods are never expected to be called in S1.
pub struct StubAuthN;

#[async_trait]
impl AuthNResolverClient for StubAuthN {
    async fn authenticate(
        &self,
        _bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        unimplemented!("S1 tests never authenticate")
    }

    async fn exchange_client_credentials(
        &self,
        _request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        unimplemented!("S1 tests never exchange credentials")
    }
}

/// A `ServiceGatewayClientV1` whose methods are never expected to be called in S1.
pub struct StubOagw;

#[async_trait]
impl ServiceGatewayClientV1 for StubOagw {
    async fn create_upstream(
        &self,
        _ctx: SecurityContext,
        _req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        _query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn create_route(
        &self,
        _ctx: SecurityContext,
        _req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn get_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<Route, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        _upstream_id: Option<Uuid>,
        _query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        unimplemented!("S1 tests never provision")
    }
    async fn proxy_request(
        &self,
        _ctx: SecurityContext,
        _req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        unimplemented!("S1 tests never proxy")
    }
}

// ── GearCtx harness ──────────────────────────────────────────────────────────

struct StaticConfig(HashMap<String, Value>);

impl ConfigProvider for StaticConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&Value> {
        self.0.get(gear_name)
    }
}

/// A gear context for `gear_name` whose `config` section is `config` (when `Some`).
pub fn gear_ctx(
    gear_name: &str,
    config: Option<Value>,
    hub: Arc<ClientHub>,
    db: Option<Db>,
) -> GearCtx {
    let mut gears = HashMap::new();
    if let Some(config) = config {
        gears.insert(gear_name.to_owned(), json!({ "config": config }));
    }
    let ctx = GearCtx::new(
        gear_name,
        Uuid::new_v4(),
        Arc::new(StaticConfig(gears)),
        hub,
        CancellationToken::new(),
    );
    match db {
        Some(db) => ctx.with_db(toolkit_db::DBProvider::new(db)),
        None => ctx,
    }
}

/// The minimal valid `gears.mini-chat.config` value.
pub fn minimal_gear_config() -> Value {
    json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" }
    })
}
