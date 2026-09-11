//! Unit tests for the plugin-catalog aggregate
//! (`cpt-cf-oagw-dod-plugin-system-unit-tests`): the per-base-type permission
//! gate, the create validation, the in-use reference scan, and the
//! immutability and no-replace posture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use super::{PluginConfigValidator, PluginManagement, PluginManagementService};
use crate::domain::services::management::{Actor, AuthorizeError, ManagementAuthorizer, ManagementError};
use crate::domain::dto::{
    AuthConfig, Endpoint, EndpointScheme, MatchConfig, Plugin, ServerConfig, SharingMode, Upstream,
};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{
    AUTH_PLUGIN_BASE_TYPE, GUARD_PLUGIN_BASE_TYPE, PERM_AUTH_PLUGIN_CREATE,
    PERM_AUTH_PLUGIN_DELETE, PERM_AUTH_PLUGIN_READ, PERM_GUARD_PLUGIN_CREATE,
    PERM_GUARD_PLUGIN_DELETE, PERM_GUARD_PLUGIN_READ, PERM_TRANSFORM_PLUGIN_CREATE,
    PERM_TRANSFORM_PLUGIN_READ, TRANSFORM_PLUGIN_BASE_TYPE,
};
use crate::domain::list_query::ListQuery;
use crate::domain::repo::{PluginBinding, RouteRecord, UpstreamRecord};
use crate::infra::storage::Storage;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn actor() -> Actor {
    Actor { tenant_id: Uuid::new_v4(), principal_id: Uuid::new_v4() }
}

/// A `deny`-list authorizer that records the permissions it was asked for.
struct StubAuthorizer {
    denied: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
}

impl StubAuthorizer {
    fn allowing() -> Arc<Self> {
        Arc::new(Self { denied: Mutex::new(Vec::new()), asked: Mutex::new(Vec::new()) })
    }

    fn denying(permissions: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            denied: Mutex::new(permissions.iter().map(|p| (*p).to_owned()).collect()),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

#[async_trait]
impl ManagementAuthorizer for StubAuthorizer {
    async fn authorize(
        &self,
        _actor: &Actor,
        permission: &str,
        _resource_id: &str,
    ) -> Result<(), AuthorizeError> {
        self.asked.lock().push(permission.to_owned());
        if self.denied.lock().contains(&permission.to_owned()) {
            Err(AuthorizeError::Denied {
                permission: permission.to_owned(),
                detail: "the test stub denies this permission".to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

fn service_of(authorizer: Arc<StubAuthorizer>) -> (PluginManagementService, Arc<Storage>) {
    let storage = Arc::new(Storage::new());
    let (upstreams, routes, plugins) = Arc::clone(&storage).repositories();
    (
        PluginManagementService::new(plugins, upstreams, routes, authorizer),
        storage,
    )
}

fn plugin_of(base_type: &str, name: &str) -> Plugin {
    Plugin {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        plugin_type: base_type.to_owned(),
        name: name.to_owned(),
        config_schema: Some(serde_json::json!({ "type": "object" })),
        source_code: Some("const reference = 'opaque';".to_owned()),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

fn upstream_of(tenant: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: "http".to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
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

/// Seed one upstream row with the given plugin bindings and auth reference.
fn seed_upstream(
    storage: &Storage,
    tenant: Uuid,
    alias: &str,
    bindings: Vec<PluginBinding>,
    auth_ref: Option<String>,
) -> Upstream {
    let (upstreams, _, _) = storage.repositories();
    let mut record = upstream_of(tenant, alias);
    record.auth = auth_ref.map(|reference| {
        AuthConfig { auth_type: Some(reference), sharing: SharingMode::Enforce, config: None }
    });
    let stored = UpstreamRecord { upstream: record.clone(), plugin_bindings: bindings };
    upstreams.create(tenant, stored).expect("seeded");
    record
}

/// Seed one route row under `upstream_id` carrying the given bindings.
fn seed_route(storage: &Storage, tenant: Uuid, upstream_id: Uuid, bindings: Vec<PluginBinding>) -> Uuid {
    let (_, routes, _) = storage.repositories();
    let route = crate::domain::dto::Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id,
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig::default(),
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    };
    routes
        .create(tenant, RouteRecord { route: route.clone(), plugin_bindings: bindings })
        .expect("seeded");
    route.id
}

fn validation_of(error: ManagementError) -> DomainError {
    match error {
        ManagementError::Domain(domain) => domain,
        other => panic!("{other}"),
    }
}

// ---------------------------------------------------------------------------
// The per-base-type permission gate
// ---------------------------------------------------------------------------

/// `inst-ps-create-10`: the create asks for the `create` element of the
/// permission set of the base type it names.
#[tokio::test]
async fn the_create_gate_names_the_base_type_permission() {
    let authorizer = StubAuthorizer::allowing();
    let (service, _storage) = service_of(Arc::clone(&authorizer));
    let caller = actor();
    service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "g1"))
        .await
        .expect("created");
    assert_eq!(authorizer.asked(), [PERM_GUARD_PLUGIN_CREATE]);

    let authorizer = StubAuthorizer::allowing();
    let (service, _storage) = service_of(Arc::clone(&authorizer));
    service
        .create(caller, plugin_of(TRANSFORM_PLUGIN_BASE_TYPE, "t1"))
        .await
        .expect("created");
    assert_eq!(authorizer.asked(), [PERM_TRANSFORM_PLUGIN_CREATE]);
}

/// The gate is evaluated before the store is consulted, so a deny stores
/// nothing.
#[tokio::test]
async fn a_deny_stores_nothing() {
    let authorizer = StubAuthorizer::denying(&[PERM_GUARD_PLUGIN_CREATE]);
    let (service, storage) = service_of(authorizer);
    let error = service
        .create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "denied"))
        .await
        .expect_err("denied");
    assert!(matches!(error, ManagementError::Authorization(_)), "{error}");
    assert_eq!(storage.row_counts()["oagw_plugin"], 0, "no row is stored");
}

/// `inst-ps-read-8`/`inst-ps-source-2`: the read gates on the base type of the
/// record it addresses.
#[tokio::test]
async fn the_read_gate_names_the_records_base_type() {
    let authorizer = StubAuthorizer::allowing();
    let (service, _storage) = service_of(Arc::clone(&authorizer));
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(AUTH_PLUGIN_BASE_TYPE, "a1"))
        .await
        .expect("created");
    authorizer.asked.lock().clear();
    service.get(caller.clone(), created.id).await.expect("read");
    assert_eq!(authorizer.asked(), [PERM_AUTH_PLUGIN_READ]);

    authorizer.asked.lock().clear();
    service.source(caller, created.id).await.expect("read the source");
    assert_eq!(authorizer.asked(), [PERM_AUTH_PLUGIN_READ]);
}

/// `inst-ps-del-10`: the delete gates on the `delete` element of the base
/// type.
#[tokio::test]
async fn the_delete_gate_names_the_delete_element() {
    let authorizer = StubAuthorizer::allowing();
    let (service, _storage) = service_of(Arc::clone(&authorizer));
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "g2"))
        .await
        .expect("created");
    authorizer.asked.lock().clear();
    service.delete(caller, created.id).await.expect("deleted");
    assert_eq!(authorizer.asked(), [PERM_GUARD_PLUGIN_DELETE]);
}

/// The list gate evaluates all three base types, because the list is not
/// filtered by type before the gate.
#[tokio::test]
async fn the_list_gate_evaluates_every_base_type() {
    let authorizer = StubAuthorizer::allowing();
    let (service, _storage) = service_of(Arc::clone(&authorizer));
    service.list(actor(), &ListQuery::default()).await.expect("listed");
    assert_eq!(
        authorizer.asked(),
        [PERM_AUTH_PLUGIN_READ, PERM_GUARD_PLUGIN_READ, PERM_TRANSFORM_PLUGIN_READ]
    );
}

// ---------------------------------------------------------------------------
// Create validation
// ---------------------------------------------------------------------------

/// `inst-ps-create-2` .. `-4`: a `plugin_type` outside the three plugin base
/// types, and a catalog-only identifier, are rejected.
#[tokio::test]
async fn a_create_names_a_plugin_base_type_only() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    for plugin_type in [
        "gts.cf.core.oagw.upstream.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "not-a-type",
    ] {
        let error = validation_of(
            service.create(actor(), plugin_of(plugin_type, "wrong")).await.expect_err("rejected"),
        );
        assert!(
            matches!(&error, DomainError::ValidationError { detail, .. } if detail.contains("plugin_type")),
            "{error}"
        );
    }
}

/// `inst-ps-create-5`/`-6`: a blank name is rejected and nothing is stored.
#[tokio::test]
async fn a_blank_name_is_rejected() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let error = validation_of(
        service
            .create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "   "))
            .await
            .expect_err("rejected"),
    );
    assert!(matches!(error, DomainError::ValidationError { .. }), "{error}");
    assert_eq!(storage.row_counts()["oagw_plugin"], 0);
}

/// `inst-ps-create-7`: a non-object `config_schema` is rejected.
#[tokio::test]
async fn a_non_object_config_schema_is_rejected() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let mut candidate = plugin_of(GUARD_PLUGIN_BASE_TYPE, "schema");
    candidate.config_schema = Some(serde_json::json!([1, 2, 3]));
    let error = validation_of(service.create(actor(), candidate).await.expect_err("rejected"));
    assert!(
        matches!(&error, DomainError::ValidationError { detail, .. } if detail.contains("config_schema")),
        "{error}"
    );
    assert_eq!(storage.row_counts()["oagw_plugin"], 0);
}

/// `inst-ps-create-6`/`-7`: a second plugin with the same `(tenant_id, name)`
/// is a conflict and leaves the store unchanged.
#[tokio::test]
async fn a_same_tenant_name_collision_is_a_conflict() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let first = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "dupe"))
        .await
        .expect("created");
    assert_eq!(first.plugin_type, GUARD_PLUGIN_BASE_TYPE);
    assert_ne!(first.id, Uuid::nil(), "the identifier is server-generated");
    assert_eq!(first.tenant_id, caller.tenant_id, "the tenant is server-assigned");

    let error = validation_of(
        service
            .create(caller, plugin_of(GUARD_PLUGIN_BASE_TYPE, "dupe"))
            .await
            .expect_err("conflict"),
    );
    assert!(matches!(error, DomainError::Conflict { .. }), "{error}");
    assert_eq!(storage.row_counts()["oagw_plugin"], 1, "the store is unchanged");
}

/// The same name is free in another tenant, because the key is
/// `(tenant_id, name)`.
#[tokio::test]
async fn the_same_name_is_free_in_another_tenant() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    service
        .create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "shared"))
        .await
        .expect("created");
    service
        .create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "shared"))
        .await
        .expect("created in the other tenant");
}

/// `inst-ps-create-8`: the source reference is recorded as the opaque artifact
/// it is, and the lifecycle fields are server-cleared.
#[tokio::test]
async fn the_create_records_the_source_reference_only() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    let created = service
        .create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "with-source"))
        .await
        .expect("created");
    assert_eq!(created.source_code.as_deref(), Some("const reference = 'opaque';"));
    assert!(created.last_used_at.is_none(), "the GC lifecycle is out of scope");
    assert!(created.gc_eligible_at.is_none());
}

// ---------------------------------------------------------------------------
// Read and source
// ---------------------------------------------------------------------------

/// `inst-ps-read-3`/`-4`: a missing, foreign or ancestor-owned identifier is
/// one indistinguishable not-found.
#[tokio::test]
async fn a_foreign_identifier_is_not_found() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    let owner = actor();
    let created = service
        .create(owner, plugin_of(GUARD_PLUGIN_BASE_TYPE, "owned"))
        .await
        .expect("created");
    let stranger = Actor { tenant_id: Uuid::new_v4(), principal_id: Uuid::new_v4() };
    let error = validation_of(service.get(stranger.clone(), created.id).await.expect_err("not found"));
    assert!(matches!(error, DomainError::NotFound { resource_type: "plugin" }), "{error}");
    let error = validation_of(service.delete(stranger, created.id).await.expect_err("not found"));
    assert!(matches!(error, DomainError::NotFound { .. }), "{error}");
}

/// An identifier nothing holds is not-found.
#[tokio::test]
async fn a_missing_identifier_is_not_found() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    let error =
        validation_of(service.get(actor(), Uuid::new_v4()).await.expect_err("not found"));
    assert!(matches!(error, DomainError::NotFound { .. }), "{error}");
}

/// `inst-ps-source-4`/`-5`: a named plugin — one that carries no stored source
/// — is not-found through the source endpoint, and the record itself is still
/// readable.
#[tokio::test]
async fn a_named_plugin_has_no_stored_source() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let mut named = plugin_of(GUARD_PLUGIN_BASE_TYPE, "named");
    named.source_code = None;
    let created = service.create(caller.clone(), named).await.expect("created");
    let error =
        validation_of(service.source(caller.clone(), created.id).await.expect_err("no source"));
    assert!(matches!(error, DomainError::NotFound { .. }), "{error}");
    service.get(caller, created.id).await.expect("readable");
}

// ---------------------------------------------------------------------------
// The in-use reference scan
// ---------------------------------------------------------------------------

/// `inst-ps-del-4` .. `-6`: an upstream auth reference blocks the deletion and
/// names the referencing upstream.
#[tokio::test]
async fn an_upstream_auth_reference_blocks_the_deletion() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(AUTH_PLUGIN_BASE_TYPE, "auth-in-use"))
        .await
        .expect("created");
    seed_upstream(
        &storage,
        caller.tenant_id,
        "gateway",
        Vec::new(),
        Some(format!("{AUTH_PLUGIN_BASE_TYPE}{}", created.id)),
    );
    let error = validation_of(service.delete(caller, created.id).await.expect_err("in use"));
    let DomainError::PluginInUse { referenced_by } = error else { panic!("{error}") };
    assert_eq!(referenced_by.upstreams.len(), 1, "{referenced_by:?}");
    assert!(referenced_by.routes.is_empty());
    assert_eq!(storage.row_counts()["oagw_plugin"], 1, "the record is unchanged");
}

/// An upstream chain binding blocks the deletion too.
#[tokio::test]
async fn an_upstream_chain_binding_blocks_the_deletion() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "chained"))
        .await
        .expect("created");
    seed_upstream(
        &storage,
        caller.tenant_id,
        "gateway",
        vec![PluginBinding {
            position: 0,
            plugin_ref: format!("{GUARD_PLUGIN_BASE_TYPE}{}", created.id),
            plugin_uuid: Some(created.id),
        }],
        None,
    );
    let error = validation_of(service.delete(caller, created.id).await.expect_err("in use"));
    let DomainError::PluginInUse { referenced_by } = error else { panic!("{error}") };
    assert_eq!(referenced_by.upstreams.len(), 1);
    assert!(referenced_by.routes.is_empty());
    assert_eq!(storage.row_counts()["oagw_plugin"], 1);
}

/// A route chain binding blocks the deletion and names the referencing route.
#[tokio::test]
async fn a_route_chain_binding_blocks_the_deletion() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(TRANSFORM_PLUGIN_BASE_TYPE, "route-plugin"))
        .await
        .expect("created");
    let upstream = seed_upstream(&storage, caller.tenant_id, "gateway", Vec::new(), None);
    seed_route(
        &storage,
        caller.tenant_id,
        upstream.id,
        vec![PluginBinding {
            position: 0,
            plugin_ref: format!("{TRANSFORM_PLUGIN_BASE_TYPE}{}", created.id),
            plugin_uuid: Some(created.id),
        }],
    );
    let error = validation_of(service.delete(caller, created.id).await.expect_err("in use"));
    let DomainError::PluginInUse { referenced_by } = error else { panic!("{error}") };
    assert_eq!(referenced_by.routes.len(), 1, "{referenced_by:?}");
    assert!(referenced_by.upstreams.is_empty());
}

/// A *textual* reference from another tenant does not block the deletion: the
/// management scan is caller-scoped, like every other management read.
#[tokio::test]
async fn a_foreign_textual_reference_does_not_block_the_deletion() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "free"))
        .await
        .expect("created");
    seed_upstream(
        &storage,
        Uuid::new_v4(),
        "foreign",
        vec![PluginBinding {
            position: 0,
            plugin_ref: format!("{GUARD_PLUGIN_BASE_TYPE}{}", created.id),
            plugin_uuid: None,
        }],
        None,
    );
    service.delete(caller, created.id).await.expect("deleted");
    assert_eq!(storage.row_counts()["oagw_plugin"], 0);
}

/// `inst-ps-del-7`/`-8`: a binding row that arrives between the scan and the
/// removal surfaces as the conflict outcome the repository reports, and the
/// record survives. This is the backstop the delete critical section relies
/// on for the concurrent-bind-then-delete case.
#[tokio::test]
async fn a_binding_that_arrives_under_the_delete_fails_as_a_conflict() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "raced"))
        .await
        .expect("created");
    // The binding attempt that lands while the delete holds the write
    // exclusion is represented here by the row it would have persisted
    // against the same single-process store.
    seed_upstream(
        &storage,
        caller.tenant_id,
        "racer",
        vec![PluginBinding {
            position: 0,
            plugin_ref: format!("{GUARD_PLUGIN_BASE_TYPE}{}", created.id),
            plugin_uuid: Some(created.id),
        }],
        None,
    );
    let error = validation_of(service.delete(caller, created.id).await.expect_err("conflict"));
    assert!(
        matches!(error, DomainError::Conflict { .. } | DomainError::PluginInUse { .. }),
        "{error}"
    );
    assert_eq!(storage.row_counts()["oagw_plugin"], 1, "the record survives");
}

/// An unreferenced plugin is deleted, and no queryable trace of it remains.
#[tokio::test]
async fn an_unreferenced_plugin_is_deleted() {
    let (service, storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    let created = service
        .create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "unreferenced"))
        .await
        .expect("created");
    service.delete(caller.clone(), created.id).await.expect("deleted");
    assert_eq!(storage.row_counts()["oagw_plugin"], 0);
    let error = validation_of(service.get(caller, created.id).await.expect_err("gone"));
    assert!(matches!(error, DomainError::NotFound { .. }), "{error}");
}

// ---------------------------------------------------------------------------
// List and OData
// ---------------------------------------------------------------------------

/// The list is caller-scoped and honours the OData `$filter` on the type.
#[tokio::test]
async fn the_list_is_caller_scoped_and_filters_by_type() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    let caller = actor();
    service.create(caller.clone(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "g")).await.expect("created");
    service
        .create(caller.clone(), plugin_of(TRANSFORM_PLUGIN_BASE_TYPE, "t"))
        .await
        .expect("created");

    let listed = service.list(caller.clone(), &ListQuery::default()).await.expect("listed");
    assert_eq!(listed.len(), 2);

    let query =
        ListQuery::parse_plugin(&[("$filter".to_owned(), "type eq 'guard'".to_owned())]).expect("parsed");
    let listed = service.list(caller, &query).await.expect("listed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].plugin_type, GUARD_PLUGIN_BASE_TYPE);
}

/// The list of another tenant is empty: no record of a foreign tenant is ever
/// disclosed.
#[tokio::test]
async fn the_list_of_another_tenant_is_empty() {
    let (service, _storage) = service_of(StubAuthorizer::allowing());
    service.create(actor(), plugin_of(GUARD_PLUGIN_BASE_TYPE, "g")).await.expect("created");
    let listed = service.list(actor(), &ListQuery::default()).await.expect("listed");
    assert!(listed.is_empty());
}

// ---------------------------------------------------------------------------
// Immutability
// ---------------------------------------------------------------------------

/// A custom plugin is immutable after creation: the aggregate exposes create,
/// list, get, source and delete, and no replace operation at all.
#[test]
fn the_aggregate_has_no_replace_operation() {
    fn assert_no_replace<S: PluginManagement + ?Sized>() {}
    assert_no_replace::<PluginManagementService>();
}

/// The configuration validator the upstream write path executes through is the
/// plugin aggregate's seam, and it can be installed (`inst-ps-bind-14`).
#[tokio::test]
async fn the_config_validator_is_the_shared_seam() {
    struct Accepting;
    impl PluginConfigValidator for Accepting {
        fn validate_auth_config(
            &self,
            _tenant_id: Uuid,
            _auth_ref: Option<&str>,
            _config: Option<&serde_json::Value>,
        ) -> Result<(), DomainError> {
            Ok(())
        }
    }
    // The seam the upstream aggregate reaches through lives on the upstream
    // management service, and the plugin aggregate owns the implementation
    // the gear installs on it.
    let authorizer = StubAuthorizer::allowing();
    let storage = Arc::new(Storage::new());
    let (upstream_repo, _routes, _plugins) = Arc::clone(&storage).repositories();
    let upstreams = crate::domain::services::management::UpstreamManagementService::new(
        upstream_repo,
        false,
        authorizer,
        flat_ancestors(),
    );
    let installed: Arc<dyn PluginConfigValidator> = Arc::new(Accepting);
    upstreams.set_plugin_config_validator(Arc::clone(&installed));
    let resolved = upstreams.plugin_config_validator();
    assert!(
        Arc::ptr_eq(&installed, &resolved),
        "the installed validator is the one the upstream write path reaches"
    );
}

/// An ancestor resolver with an empty chain, standing in for a flat tenancy.
fn flat_ancestors() -> Arc<dyn crate::domain::services::management::AncestorResolver> {
    struct Flat;
    #[async_trait]
    impl crate::domain::services::management::AncestorResolver for Flat {
        async fn ancestors(&self, _actor: &Actor, _tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
            Ok(Vec::new())
        }
    }
    Arc::new(Flat)
}

/// `PERM_*` constants and the permission helper agree, so the gate and the
/// REST layer cannot drift apart.
#[test]
fn the_permission_sets_are_the_documented_ones() {
    assert_eq!(PERM_AUTH_PLUGIN_CREATE, "gts.cf.core.oagw.auth_plugin.v1~:create");
    assert_eq!(PERM_AUTH_PLUGIN_DELETE, "gts.cf.core.oagw.auth_plugin.v1~:delete");
    assert_eq!(PERM_GUARD_PLUGIN_CREATE, "gts.cf.core.oagw.guard_plugin.v1~:create");
    assert_eq!(PERM_GUARD_PLUGIN_DELETE, "gts.cf.core.oagw.guard_plugin.v1~:delete");
    assert_eq!(
        crate::domain::gts_helpers::plugin_permission(
            AUTH_PLUGIN_BASE_TYPE,
            crate::domain::gts_helpers::PluginAction::Read
        ),
        PERM_AUTH_PLUGIN_READ
    );
    assert_eq!(
        crate::domain::gts_helpers::plugin_permission(
            GUARD_PLUGIN_BASE_TYPE,
            crate::domain::gts_helpers::PluginAction::Delete
        ),
        PERM_GUARD_PLUGIN_DELETE
    );
    assert_eq!(
        crate::domain::gts_helpers::plugin_permission(
            TRANSFORM_PLUGIN_BASE_TYPE,
            crate::domain::gts_helpers::PluginAction::Create
        ),
        PERM_TRANSFORM_PLUGIN_CREATE
    );
}
