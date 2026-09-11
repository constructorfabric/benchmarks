//! Test support: the client fakes and the configuration provider the gear
//! tests (unit and integration) build a `GearCtx` from.
//!
//! Compiled only under the `test-utils` feature, which the crate's own
//! dev-dependency enables for both the lib test target and the `tests/`
//! targets.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext};
use authz_resolver_sdk::{AuthZResolverClient, AuthZResolverError};
use credstore_sdk::{CredStoreClientV1, CredStoreError};
use crate::OagwGear;
use toolkit::Gear;
use toolkit::client_hub::ClientHub;
use toolkit::config::ConfigProvider;
use toolkit::context::GearCtx;
use toolkit_security::SecurityContext;
use tenant_resolver_sdk::models::{
    GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
    GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef, TenantStatus,
};
use tenant_resolver_sdk::{TenantResolverClient, TenantResolverError};
use toolkit_canonical_errors::resource_error;
use types_registry_sdk::models::{GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

/// A test registry resource type, so the fakes below can synthesize the same
/// canonical envelopes the real client emits.
#[resource_error("gts.cf.core.oagw.test_registry_type.v1~")]
pub struct TestRegistryResource;

/// The canonical error the real registry answers for a repeat registration.
#[must_use]
pub fn already_exists(id: &str) -> toolkit_canonical_errors::CanonicalError {
    TestRegistryResource::already_exists(format!("already registered: {id}"))
        .with_resource(id.to_owned())
        .create()
}

/// A generic canonical error a fake can answer instead of a real one.
#[must_use]
pub fn unimplemented() -> toolkit_canonical_errors::CanonicalError {
    TestRegistryResource::unimplemented("not implemented").create()
}

/// A `types-registry` fake that answers every registration with `error`,
/// standing in for an unreachable or rejecting registry.
#[must_use]
pub fn failing_registry(error: toolkit_canonical_errors::CanonicalError) -> Arc<FakeTypesRegistry> {
    let mut fake = FakeTypesRegistry::new();
    fake.failure = Some(error);
    Arc::new(fake)
}

/// A `types-registry` fake that accepts the call but answers every submitted
/// schema with `error`, standing in for a registry that rejects the gear's
/// base types per item.
#[must_use]
pub fn rejecting_registry(error: toolkit_canonical_errors::CanonicalError) -> Arc<FakeTypesRegistry> {
    let mut fake = FakeTypesRegistry::new();
    fake.item_failure = Some(error);
    Arc::new(fake)
}

/// A minimal valid `https` upstream over one endpoint, owned by `tenant_id`.
#[must_use]
pub fn upstream(tenant_id: Uuid, alias: &str) -> crate::domain::dto::Upstream {
    crate::domain::dto::Upstream {
        id: Uuid::new_v4(),
        tenant_id,
        alias: alias.to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: crate::domain::dto::ServerConfig {
            endpoints: vec![crate::domain::dto::Endpoint {
                scheme: crate::domain::dto::EndpointScheme::Https,
                host: "backend.example.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// A fake `types-registry` client: it records what it was asked to register
/// and answers `AlreadyExists` on a repeat, so the idempotency path is
/// exercised against a stateful fake.
#[derive(Default)]
pub struct FakeTypesRegistry {
    registered: Mutex<Vec<String>>,
    /// `None` records; `Some` answers every registration with this error.
    pub failure: Option<toolkit_canonical_errors::CanonicalError>,
    /// `Some` answers every *submitted item* with this error, standing in for
    /// a registry that holds the connection but rejects the payload.
    pub item_failure: Option<toolkit_canonical_errors::CanonicalError>,
}

impl FakeTypesRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `$id`s the registry holds.
    #[must_use]
    pub fn registered_ids(&self) -> Vec<String> {
        self.registered.lock().expect("FakeTypesRegistry lock").clone()
    }
}

#[async_trait]
impl TypesRegistryClient for FakeTypesRegistry {
    async fn register(
        &self,
        _entities: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, toolkit_canonical_errors::CanonicalError> {
        Ok(vec![])
    }

    async fn register_type_schemas(
        &self,
        type_schemas: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, toolkit_canonical_errors::CanonicalError> {
        let mut store = self.registered.lock().expect("FakeTypesRegistry lock");
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if let Some(error) = &self.item_failure {
            return Ok(type_schemas
                .into_iter()
                .map(|schema| RegisterResult::Err {
                    gts_id: schema.get("$id").and_then(serde_json::Value::as_str).map(str::to_owned),
                    error: error.clone(),
                })
                .collect());
        }
        Ok(type_schemas
            .into_iter()
            .map(|schema| {
                let id = schema.get("$id").and_then(serde_json::Value::as_str).map(str::to_owned);
                match id {
                    Some(id) if store.contains(&id) => RegisterResult::Err {
                        gts_id: Some(id.clone()),
                        error: already_exists(&id),
                    },
                    Some(id) => {
                        store.push(id.clone());
                        RegisterResult::Ok { gts_id: id }
                    }
                    None => RegisterResult::Err { gts_id: None, error: unimplemented() },
                }
            })
            .collect())
    }

    async fn get_type_schema(
        &self,
        _type_id: &str,
    ) -> Result<GtsTypeSchema, toolkit_canonical_errors::CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schema_by_uuid(
        &self,
        _type_uuid: uuid::Uuid,
    ) -> Result<GtsTypeSchema, toolkit_canonical_errors::CanonicalError> {
        Err(unimplemented())
    }
    async fn get_type_schemas(
        &self,
        type_ids: Vec<String>,
    ) -> HashMap<String, Result<GtsTypeSchema, toolkit_canonical_errors::CanonicalError>> {
        type_ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_type_schemas_by_uuid(
        &self,
        type_uuids: Vec<uuid::Uuid>,
    ) -> HashMap<uuid::Uuid, Result<GtsTypeSchema, toolkit_canonical_errors::CanonicalError>> {
        type_uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_type_schemas(
        &self,
        _query: TypeSchemaQuery,
    ) -> Result<Vec<GtsTypeSchema>, toolkit_canonical_errors::CanonicalError> {
        Ok(vec![])
    }
    async fn register_instances(
        &self,
        _instances: Vec<serde_json::Value>,
    ) -> Result<Vec<RegisterResult>, toolkit_canonical_errors::CanonicalError> {
        Ok(vec![])
    }
    async fn get_instance(
        &self,
        _id: &str,
    ) -> Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instance_by_uuid(
        &self,
        _uuid: uuid::Uuid,
    ) -> Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError> {
        Err(unimplemented())
    }
    async fn get_instances(
        &self,
        ids: Vec<String>,
    ) -> HashMap<String, Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>>
    {
        ids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn get_instances_by_uuid(
        &self,
        uuids: Vec<uuid::Uuid>,
    ) -> HashMap<uuid::Uuid, Result<types_registry_sdk::GtsInstance, toolkit_canonical_errors::CanonicalError>>
    {
        uuids.into_iter().map(|id| (id, Err(unimplemented()))).collect()
    }
    async fn list_instances(
        &self,
        _query: InstanceQuery,
    ) -> Result<Vec<types_registry_sdk::GtsInstance>, toolkit_canonical_errors::CanonicalError> {
        Ok(vec![])
    }
}

/// A fake `authz-resolver` client: it allows everything, because the gear
/// resolves the handle at init and first invokes it from a later entry.
#[derive(Default)]
pub struct FakeAuthZResolver;

#[async_trait]
impl AuthZResolverClient for FakeAuthZResolver {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse { decision: true, context: EvaluationResponseContext::default() })
    }
}

/// The permission names the management surface evaluates, for the fakes that
/// decide per permission.
pub const MANAGEMENT_PERMISSIONS: [&str; 9] = [
    "gts.cf.core.oagw.upstream.v1~:create",
    "gts.cf.core.oagw.upstream.v1~:read",
    "gts.cf.core.oagw.upstream.v1~:override",
    "gts.cf.core.oagw.upstream.v1~:delete",
    "gts.cf.core.oagw.upstream.v1~:bind",
    "gts.cf.core.oagw.route.v1~:create",
    "gts.cf.core.oagw.route.v1~:read",
    "gts.cf.core.oagw.route.v1~:override",
    "gts.cf.core.oagw.route.v1~:delete",
];

/// A minimal valid route over one HTTP match, addressed to `upstream_id`.
#[must_use]
pub fn route(upstream_id: Uuid, path: &str) -> crate::domain::dto::Route {
    crate::domain::dto::Route {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        upstream_id,
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: crate::domain::dto::MatchConfig {
            http: Some(crate::domain::dto::HttpMatch {
                methods: vec![crate::domain::dto::HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: crate::domain::dto::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// The request body of `POST /oagw/v1/routes` over one HTTP match.
#[must_use]
pub fn route_body(upstream_id: Uuid, path: &str) -> serde_json::Value {
    serde_json::json!({
        "upstream_id": upstream_id.to_string(),
        "match": { "http": { "path": path, "methods": ["GET"] } }
    })
}

/// A policy-driven `authz-resolver` fake: every permission is allowed unless
/// the test denies it by name, so a suite can exercise the `403` surface of
/// one operation while the others stay reachable.
#[derive(Default)]
pub struct FakePolicyAuthZ {
    denied: Mutex<HashSet<String>>,
    /// When set, every evaluation for that permission fails instead of
    /// denying, standing in for an unreachable PDP.
    unavailable: Mutex<HashSet<String>>,
    /// The actions that were evaluated, in order.
    pub evaluated: Mutex<Vec<String>>,
}

impl FakePolicyAuthZ {
    /// A fake that denies `permissions` and allows everything else.
    #[must_use]
    pub fn denying(permissions: &[&str]) -> Arc<Self> {
        let fake = Self::default();
        *fake.denied.lock().expect("denied lock") =
            permissions.iter().map(|permission| (*permission).to_owned()).collect();
        Arc::new(fake)
    }

    /// A fake that makes `permissions` fail with an evaluation failure.
    #[must_use]
    pub fn unavailable_for(permissions: &[&str]) -> Arc<Self> {
        let fake = Self::default();
        *fake.unavailable.lock().expect("unavailable lock") =
            permissions.iter().map(|permission| (*permission).to_owned()).collect();
        Arc::new(fake)
    }

    /// Deny `permission` from now on, so a single surface can exercise both
    /// outcomes of one gate.
    pub fn deny(&self, permission: &str) {
        self.denied
            .lock()
            .expect("denied lock")
            .insert(permission.to_owned());
    }

    /// Whether `permission` was evaluated at all.
    #[must_use]
    pub fn evaluated(&self, permission: &str) -> bool {
        self.evaluated
            .lock()
            .expect("evaluated lock")
            .iter()
            .any(|action| action == permission)
    }
}

#[async_trait]
impl AuthZResolverClient for FakePolicyAuthZ {
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let action = request.action.name.clone();
        self.evaluated
            .lock()
            .expect("evaluated lock")
            .push(action.clone());
        if self.unavailable.lock().expect("unavailable lock").contains(&action) {
            return Err(AuthZResolverError::ServiceUnavailable("the PDP is unreachable".to_owned()));
        }
        let denied = self.denied.lock().expect("denied lock").contains(&action);
        Ok(EvaluationResponse { decision: !denied, context: EvaluationResponseContext::default() })
    }
}

/// A fake `tenant-resolver` client.
#[derive(Default)]
pub struct FakeTenantResolver;

impl FakeTenantResolver {
    fn info(id: Uuid) -> TenantInfo {
        TenantInfo {
            id: TenantId(id),
            name: "tenant".to_owned(),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }
}

#[async_trait]
impl TenantResolverClient for FakeTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(Self::info(id.0))
    }
    async fn get_root_tenant(&self, _ctx: &SecurityContext) -> Result<TenantInfo, TenantResolverError> {
        Ok(Self::info(uuid::Uuid::nil()))
    }
    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids.iter().map(|id| Self::info(id.0)).collect())
    }
    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        Ok(GetAncestorsResponse { tenant: TenantRef::from(Self::info(id.0)), ancestors: vec![] })
    }
    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse { tenant: TenantRef::from(Self::info(id.0)), descendants: vec![] })
    }
    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor_id: TenantId,
        _descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(false)
    }
}

/// A fake `credstore` client: no secret resolves, because entry 2.1 resolves
/// the handle only.
#[derive(Default)]
pub struct FakeCredStore;

#[async_trait]
impl CredStoreClientV1 for FakeCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, CredStoreError> {
        Ok(None)
    }
}

/// A `ConfigProvider` over a fixed `oagw` block.
///
/// The toolkit lookup reads the gear entry's `config` section, so the value
/// handed over here is wrapped the way the runtime configuration file wraps a
/// gear block.
pub struct TestConfig {
    oagw: Option<serde_json::Value>,
}

impl TestConfig {
    /// A provider carrying the given `oagw` block.
    #[must_use]
    pub fn new(value: Option<serde_json::Value>) -> Self {
        Self { oagw: value.map(|value| serde_json::json!({ "config": value })) }
    }
}

impl ConfigProvider for TestConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        if gear_name == "oagw" {
            self.oagw.as_ref()
        } else {
            None
        }
    }
}

/// Build a gear context whose client hub holds the four dependency fakes.
///
/// The `oagw` configuration block is `config` when given, and absent
/// otherwise, so `config_or_default` falls back to the documented defaults.
#[must_use]
pub fn test_context(config: Option<serde_json::Value>) -> GearCtx {
    test_context_with_registry(config, Arc::new(FakeTypesRegistry::default()))
}

/// [`test_context`] with a caller-owned `types-registry` fake, so the test can
/// inspect what the gear registered.
#[must_use]
pub fn test_context_with_registry(
    config: Option<serde_json::Value>,
    types_registry: Arc<FakeTypesRegistry>,
) -> GearCtx {
    context(
        config,
        Some(types_registry),
        Some(Arc::new(FakeAuthZResolver)),
        Some(Arc::new(FakeTenantResolver)),
        Some(Arc::new(FakeCredStore)),
    )
}

/// [`test_context`] with the `types-registry` handle absent, so init fails at
/// dependency resolution.
#[must_use]
pub fn context_without_types_registry(config: Option<serde_json::Value>) -> GearCtx {
    context(
        config,
        None,
        Some(Arc::new(FakeAuthZResolver)),
        Some(Arc::new(FakeTenantResolver)),
        Some(Arc::new(FakeCredStore)),
    )
}

/// The general context builder the shaped constructors above delegate to.
pub fn context(
    config: Option<serde_json::Value>,
    types_registry: Option<Arc<FakeTypesRegistry>>,
    authz: Option<Arc<dyn AuthZResolverClient>>,
    tenant: Option<Arc<dyn TenantResolverClient>>,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
) -> GearCtx {
    let hub = Arc::new(ClientHub::new());
    if let Some(registry) = types_registry {
        hub.register::<dyn TypesRegistryClient>(registry);
    }
    if let Some(authz) = authz {
        hub.register::<dyn AuthZResolverClient>(authz);
    }
    if let Some(tenant) = tenant {
        hub.register::<dyn TenantResolverClient>(tenant);
    }
    if let Some(credstore) = credstore {
        hub.register::<dyn CredStoreClientV1>(credstore);
    }
    GearCtx::new(
        "oagw",
        uuid::Uuid::nil(),
        Arc::new(TestConfig::new(config)),
        hub,
        tokio_util::sync::CancellationToken::new(),
    )
}

/// The UUID of the test context's hub, for asserting the published handles.
#[must_use]
pub fn hub_of(ctx: &GearCtx) -> Arc<ClientHub> {
    ctx.client_hub().clone()
}

/// A hierarchical `tenant-resolver` fake: the test declares a child → direct
/// parent map and [`TenantResolverClient::get_ancestors`] walks it in the
/// *direct parent first* order the real client documents.
#[derive(Default)]
pub struct FakeHierarchyTenantResolver {
    parents: Mutex<HashMap<Uuid, Uuid>>,
}

impl FakeHierarchyTenantResolver {
    /// A resolver over `chain`, where `chain[0]` is the leaf tenant and every
    /// following entry is its direct parent.
    #[must_use]
    pub fn over(chain: &[Uuid]) -> Arc<Self> {
        let resolver = Self::default();
        let mut parents = resolver.parents.lock().expect("parents lock");
        for pair in chain.windows(2) {
            parents.insert(pair[0], pair[1]);
        }
        drop(parents);
        Arc::new(resolver)
    }
}

#[async_trait]
impl TenantResolverClient for FakeHierarchyTenantResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(FakeTenantResolver::info(id.0))
    }
    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Ok(FakeTenantResolver::info(Uuid::nil()))
    }
    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        ids: &[TenantId],
        _options: &GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(ids.iter().map(|id| FakeTenantResolver::info(id.0)).collect())
    }
    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let parents = self.parents.lock().expect("parents lock");
        let mut ancestors = Vec::new();
        let mut current = id;
        while let Some(parent) = parents.get(&current.0) {
            ancestors.push(TenantRef::from(FakeTenantResolver::info(*parent)));
            current = TenantId(*parent);
        }
        Ok(GetAncestorsResponse {
            tenant: TenantRef::from(FakeTenantResolver::info(id.0)),
            ancestors,
        })
    }
    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetDescendantsOptions,
    ) -> Result<GetDescendantsResponse, TenantResolverError> {
        Ok(GetDescendantsResponse {
            tenant: TenantRef::from(FakeTenantResolver::info(id.0)),
            descendants: vec![],
        })
    }
    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        ancestor_id: TenantId,
        descendant_id: TenantId,
        _options: &IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        let parents = self.parents.lock().expect("parents lock");
        let mut current = descendant_id;
        while let Some(parent) = parents.get(&current.0) {
            if *parent == ancestor_id.0 {
                return Ok(true);
            }
            current = TenantId(*parent);
        }
        Ok(false)
    }
}

/// A security context for an authenticated management caller, carrying the
/// subject and the tenant the handlers read.
#[must_use]
pub fn security_context(tenant_id: Uuid, principal_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(principal_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("a two-field security context always builds")
}

/// A configuration-write hook that records every notification it was handed,
/// so a test can assert the audit fields of DESIGN §4.3 and the
/// store-write → hook order without entry 2.9's emitter.
#[derive(Default)]
pub struct RecordingConfigWriteHook {
    notifications: Mutex<Vec<crate::domain::services::management::ConfigWriteNotification>>,
}

impl RecordingConfigWriteHook {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The notifications, in the order the writes produced them.
    #[must_use]
    pub fn notifications(&self) -> Vec<crate::domain::services::management::ConfigWriteNotification> {
        self.notifications.lock().expect("notifications lock").clone()
    }
}

#[async_trait]
impl crate::domain::services::management::ConfigWriteHook for RecordingConfigWriteHook {
    async fn on_config_written(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<(), crate::domain::error::DomainError> {
        self.notifications
            .lock()
            .expect("notifications lock")
            .push(crate::domain::services::management::ConfigWriteNotification {
                event: "upstream.write",
                tenant_id,
                principal_id: Uuid::nil(),
                resource_id: upstream_id.to_string(),
                upstream_id: Some(upstream_id),
                upstream_alias: None,
                route: None,
                plugin_id: None,
                status: 201,
                outcome: "accepted",
            });
        Ok(())
    }

    async fn on_route_written(
        &self,
        notification: crate::domain::services::management::ConfigWriteNotification,
    ) -> Result<(), crate::domain::error::DomainError> {
        self.notifications.lock().expect("notifications lock").push(notification);
        Ok(())
    }
}

/// The initialized gear and its registered upstream-management router.
///
/// The harness drives the *real* `Gear::init` over the fakes, then registers
/// the management surface through `RestApiCapability`, so the integration
/// tests exercise the same wiring the runtime drives.
pub struct ManagementSurface {
    /// The initialized context, for asserting the published handles.
    pub ctx: GearCtx,
    /// The initialized gear.
    pub gear: OagwGear,
    /// The router carrying the five upstream operations.
    pub router: axum::Router,
    /// The policy-driven authz fake the surface was initialized over.
    pub authz: Arc<FakePolicyAuthZ>,
}

/// Build a [`ManagementSurface`] over the given configuration and fakes.
///
/// # Panics
///
/// Panics when `Gear::init` fails, which is always a defect of the fixture
/// rather than of the code under test.
pub async fn management_surface(
    config: Option<serde_json::Value>,
    authz: Arc<FakePolicyAuthZ>,
    tenant: Arc<FakeHierarchyTenantResolver>,
) -> ManagementSurface {
    management_surface_over(config, authz, tenant, None, None).await
}

/// [`management_surface`] over a caller-owned `credstore` handle: `None` keeps
/// the [`FakeCredStore`] default.
async fn management_surface_over(
    config: Option<serde_json::Value>,
    authz: Arc<FakePolicyAuthZ>,
    tenant: Arc<FakeHierarchyTenantResolver>,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    audit: Option<Arc<crate::infra::audit::AuditSink>>,
) -> ManagementSurface {
    use toolkit::contracts::RestApiCapability as _;

    let ctx = context(
        config,
        Some(Arc::new(FakeTypesRegistry::default())),
        Some(authz.clone()),
        Some(tenant),
        credstore.map_or_else(|| Some(Arc::new(FakeCredStore) as Arc<dyn CredStoreClientV1>), Some),
    );
    let gear = OagwGear::default();
    if let Some(audit) = audit {
        gear.install_audit_sink(audit);
    }
    <OagwGear as Gear>::init(&gear, &ctx)
        .await
        .expect("the gear initializes against the fakes");
    let router = gear
        .register_rest(&ctx, axum::Router::new(), &toolkit::api::OpenApiRegistryImpl::new())
        .expect("the management surface registers");
    ManagementSurface { ctx, gear, router, authz }
}

/// A surface whose gear writes its audit records into a captured sink, so the
/// lines the real proxy and management surfaces emit are assertable
/// (`cpt-cf-oagw-dod-observability-and-state-audit-log`).
#[must_use]
pub async fn audited_surface(
    config: Option<serde_json::Value>,
) -> (ManagementSurface, Arc<crate::infra::audit::AuditSink>) {
    audited_surface_over(config, Arc::new(FakePolicyAuthZ::default())).await
}

/// [`audited_surface`] over a caller-owned authz fake, so a test can deny one
/// permission while granting the rest.
#[must_use]
pub async fn audited_surface_over(
    config: Option<serde_json::Value>,
    authz: Arc<FakePolicyAuthZ>,
) -> (ManagementSurface, Arc<crate::infra::audit::AuditSink>) {
    let audit = crate::infra::audit::AuditSink::captured();
    let surface = management_surface_over(
        config,
        authz,
        Arc::new(FakeHierarchyTenantResolver::default()),
        None,
        Some(Arc::clone(&audit)),
    )
    .await;
    (surface, audit)
}

/// A surface over the permissive fakes: every permission is allowed and the
/// tenant hierarchy is flat, which is the posture of the CRUD tests.
#[must_use]
pub async fn permissive_surface(config: Option<serde_json::Value>) -> ManagementSurface {
    management_surface(
        config,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
    )
    .await
}

/// [`permissive_surface`] over a caller-owned `credstore` handle, so a test can
/// seed material the request-time resolution of entry 2.6 resolves.
///
/// The management surface the gear builds never calls `cred_store`, so this
/// only changes what a proxied request resolves.
#[must_use]
pub async fn permissive_surface_with_credstore(
    config: Option<serde_json::Value>,
    credstore: Arc<dyn CredStoreClientV1>,
) -> ManagementSurface {
    management_surface_over(
        config,
        Arc::new(FakePolicyAuthZ::default()),
        Arc::new(FakeHierarchyTenantResolver::default()),
        Some(credstore),
        None,
    )
    .await
}

impl ManagementSurface {
    /// Issue a request against the router and return the status and the body
    /// bytes.
    ///
    /// # Panics
    ///
    /// Panics when the router does not answer, which is a wiring defect.
    pub async fn send(
        &self,
        method: http::Method,
        path: &str,
        security: Option<SecurityContext>,
        body: Option<serde_json::Value>,
    ) -> (http::StatusCode, Vec<u8>) {
        let mut builder = http::Request::builder().method(method).uri(path);
        if body.is_some() {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }
        let mut request = builder.body(axum::body::Body::from(
            body.map_or_else(String::new, |body| body.to_string()),
        ))
        .expect("the request is well formed");
        if let Some(security) = security {
            request.extensions_mut().insert(security);
        }
        let response = tower::ServiceExt::oneshot(self.router.clone(), request)
            .await
            .expect("the router answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body is readable")
            .to_vec();
        (status, bytes)
    }

    /// Issue an authenticated `POST /oagw/v1/upstreams`.
    pub async fn create(
        &self,
        tenant: Uuid,
        principal: Uuid,
        body: serde_json::Value,
    ) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::POST,
            "/oagw/v1/upstreams",
            Some(security_context(tenant, principal)),
            Some(body),
        )
        .await
    }

    /// Issue an authenticated `GET /oagw/v1/upstreams/{id}`.
    pub async fn get(&self, tenant: Uuid, principal: Uuid, id: Uuid) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::GET,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await
    }

    /// Issue an authenticated `POST /oagw/v1/routes`.
    pub async fn create_route(
        &self,
        tenant: Uuid,
        principal: Uuid,
        body: serde_json::Value,
    ) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::POST,
            "/oagw/v1/routes",
            Some(security_context(tenant, principal)),
            Some(body),
        )
        .await
    }

    /// Issue an authenticated `GET /oagw/v1/routes/{id}`.
    pub async fn get_route(
        &self,
        tenant: Uuid,
        principal: Uuid,
        id: Uuid,
    ) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::GET,
            &format!("/oagw/v1/routes/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await
    }

    /// Issue an authenticated `GET /oagw/v1/routes` with `query` appended.
    pub async fn list_routes(
        &self,
        tenant: Uuid,
        principal: Uuid,
        query: &str,
    ) -> (http::StatusCode, Vec<u8>) {
        let suffix = if query.is_empty() { String::new() } else { format!("?{query}") };
        self.send(
            http::Method::GET,
            &format!("/oagw/v1/routes{suffix}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await
    }

    /// Issue an authenticated `PUT /oagw/v1/routes/{id}`.
    pub async fn replace_route(
        &self,
        tenant: Uuid,
        principal: Uuid,
        id: Uuid,
        body: serde_json::Value,
    ) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::PUT,
            &format!("/oagw/v1/routes/{id}"),
            Some(security_context(tenant, principal)),
            Some(body),
        )
        .await
    }

    /// Issue an authenticated `DELETE /oagw/v1/routes/{id}`.
    pub async fn delete_route(&self, tenant: Uuid, principal: Uuid, id: Uuid) -> (http::StatusCode, Vec<u8>) {
        self.send(
            http::Method::DELETE,
            &format!("/oagw/v1/routes/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await
    }
}

/// A proxy request a raw stub upstream received, in the form the header
/// pipeline asserts need.
#[derive(Debug, Clone)]
pub struct StubUpstreamRequest {
    /// The request line, e.g. `GET /v1/orders?limit=1 HTTP/1.1`.
    pub request_line: String,
    /// The header lines, lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The body bytes, when the request declared and carried any.
    pub body: Vec<u8>,
}

impl StubUpstreamRequest {
    /// The header value of `name`, compared exactly as received.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The request target, without the version.
    pub fn target(&self) -> &str {
        self.request_line.split(' ').nth(1).unwrap_or_default()
    }

    /// The request method.
    pub fn method(&self) -> &str {
        self.request_line.split(' ').next().unwrap_or_default()
    }

    /// The body as UTF-8, for the assertions that read it back.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// A stub upstream the proxy path connects to over plaintext HTTP/1.1.
///
/// One stub serves one connection at a time and answers each request with the
/// next scripted raw response, so the tests control the bytes exactly — a
/// buffered response, a streamed one, an upgrade, or a refusal.
pub struct StubUpstream {
    /// The bound address an upstream endpoint points at.
    pub address: std::net::SocketAddr,
    /// The requests received, in arrival order.
    pub requests: Arc<Mutex<Vec<StubUpstreamRequest>>>,
}

impl StubUpstream {
    /// The address as `host:port`, for the endpoint configuration.
    #[must_use]
    pub fn endpoint(&self) -> (String, u16) {
        let host = "127.0.0.1".to_owned();
        (host, self.address.port())
    }

    /// Whether any request reached the stub.
    #[must_use]
    pub fn received(&self) -> Vec<StubUpstreamRequest> {
        self.requests.lock().expect("stub requests").clone()
    }
}

/// Run a stub upstream answering each request with the next scripted raw
/// response; when the script runs out it answers `200 OK` with `payload`.
///
/// The stub never parses what it receives beyond the head/body split, so an
/// upgrade session's framing is relayed untouched.
pub async fn stub_upstream(script: Vec<String>) -> StubUpstream {
    stub_upstream_at(0, script).await
}

/// [`stub_upstream`] bound to `port`, so a test can make an endpoint that was
/// unreachable reachable again without changing the endpoint address the
/// metrics label carries.
pub async fn stub_upstream_at(port: u16, script: Vec<String>) -> StubUpstream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("the stub upstream binds");
    let address = listener.local_addr().expect("the stub address");
    let requests: Arc<Mutex<Vec<StubUpstreamRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let scripted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(script));
    let requests_for_loop = Arc::clone(&requests);
    tokio::spawn(async move {
        let requests = requests_for_loop;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let scripted = Arc::clone(&scripted);
            let recorded = Arc::clone(&requests);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 2048];
                // Read the head, then whatever body the framing declares.
                let head_end = loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break buffer.len();
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(index) = find_head_end(&buffer) {
                        break index;
                    }
                    if buffer.len() > 1 << 20 {
                        break buffer.len();
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let declared = headers
                    .iter()
                    .find(|(name, _)| name == "content-length")
                    .and_then(|(_, value)| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = buffer[head_end.min(buffer.len())..].to_vec();
                while body.len() < declared {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                recorded.lock().expect("stub requests").push(StubUpstreamRequest {
                    request_line,
                    headers,
                    body,
                });
                let next = {
                    let mut script = scripted.lock().expect("stub script");
                    if script.is_empty() {
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 7\r\n\r\npayload".to_owned()
                    } else {
                        script.remove(0)
                    }
                };
                let _ = socket.write_all(next.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    StubUpstream { address, requests }
}

/// The index of the head/body boundary of a raw HTTP/1.1 request.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4)
}

/// An upstream record over the minimal valid `https` upstream, with no plugin
/// bindings, for the cache and observability tests.
#[must_use]
pub fn record(tenant_id: Uuid, alias: &str) -> crate::domain::repo::UpstreamRecord {
    crate::domain::repo::UpstreamRecord {
        upstream: upstream(tenant_id, alias),
        plugin_bindings: Vec::new(),
    }
}

/// A minimal enabled HTTP route under `upstream_id`, matching `GET /v1/*`.
#[must_use]
pub fn route_record(upstream_id: Uuid) -> crate::domain::dto::Route {
    route_for(
        Uuid::nil(),
        upstream_id,
        "/v1",
        &[crate::domain::dto::HttpMethod::Get, crate::domain::dto::HttpMethod::Post],
    )
}

/// An upstream record an alias-resolution test points at a stub upstream.
#[must_use]
pub fn upstream_at(
    tenant_id: Uuid,
    alias: &str,
    scheme: crate::domain::dto::EndpointScheme,
    host: &str,
    port: u16,
) -> crate::domain::dto::Upstream {
    let mut record = upstream(tenant_id, alias);
    record.server = crate::domain::dto::ServerConfig {
        endpoints: vec![crate::domain::dto::Endpoint { scheme, host: host.to_owned(), port }],
    };
    record
}

/// The pipeline-boundary observation one completed exchange carries, with
/// `route` as the normalized match pattern, for the observability tests.
#[must_use]
pub fn observation(status: u16, route: &str) -> crate::domain::proxy::ProxyObservation {
    crate::domain::proxy::ProxyObservation {
        status,
        duration_ms: 12,
        request_size: 3,
        response_size: 40,
        error_type: None,
        rate_limit: None,
        cors: None,
        host: Some("api.vendor.com".to_owned()),
        route: Some(route.to_owned()),
        routing: None,
        phases: crate::domain::proxy::PhaseObservation::default(),
    }
}

/// Persist `record` through the store the gear was initialized over.
pub fn seed_upstream(surface: &ManagementSurface, record: crate::domain::dto::Upstream) -> Uuid {
    let storage = surface.gear.storage().expect("the store is initialized");
    let (upstreams, _, _) = storage.repositories();
    upstreams
        .create(
            record.tenant_id,
            crate::domain::repo::UpstreamRecord { upstream: record.clone(), plugin_bindings: Vec::new() },
        )
        .expect("the upstream is seeded");
    record.id
}

/// Persist `route` through the store the gear was initialized over.
pub fn seed_route(surface: &ManagementSurface, route: crate::domain::dto::Route) -> Uuid {
    let storage = surface.gear.storage().expect("the store is initialized");
    let (_, routes, _) = storage.repositories();
    routes
        .create(
            route.tenant_id,
            crate::domain::repo::RouteRecord { route: route.clone(), plugin_bindings: Vec::new() },
        )
        .expect("the route is seeded");
    route.id
}

/// An enabled HTTP route under `upstream_id` matching `path` for `methods`.
#[must_use]
pub fn route_for(
    tenant_id: Uuid,
    upstream_id: Uuid,
    path: &str,
    methods: &[crate::domain::dto::HttpMethod],
) -> crate::domain::dto::Route {
    crate::domain::dto::Route {
        id: Uuid::new_v4(),
        tenant_id,
        upstream_id,
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: crate::domain::dto::MatchConfig {
            http: Some(crate::domain::dto::HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::dto::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

/// The response the proxy handler produced.
#[derive(Debug)]
pub struct ProxyExchange {
    /// The response status.
    pub status: http::StatusCode,
    /// The response headers, lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The body bytes.
    pub body: bytes::Bytes,
}

impl ProxyExchange {
    /// The value of `name`, compared exactly as the framework lowercased it.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The body as UTF-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

impl ManagementSurface {
    /// Issue a raw request against the registered router.
    ///
    /// # Panics
    ///
    /// Panics when the router does not answer, which is a wiring defect.
    pub async fn proxy(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        security: Option<SecurityContext>,
    ) -> ProxyExchange {
        let mut builder = http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(axum::body::Body::from(body.to_vec())).expect("the request is well formed");
        if let Some(security) = security {
            request.extensions_mut().insert(security);
        }
        let response = tower::ServiceExt::oneshot(self.router.clone(), request)
            .await
            .expect("the router answers");
        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or_default().to_owned()))
            .collect();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body is readable");
        ProxyExchange { status, headers, body }
    }

    /// An authenticated proxy request of the calling tenant.
    pub async fn proxy_for(
        &self,
        tenant: Uuid,
        principal: Uuid,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> ProxyExchange {
        self.proxy(method, path, headers, body, Some(security_context(tenant, principal))).await
    }
}
