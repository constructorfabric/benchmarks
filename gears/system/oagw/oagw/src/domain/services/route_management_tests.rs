//! Unit tests for the route-management aggregate
//! (`cpt-cf-oagw-dod-route-management-unit-tests`).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use super::*;
use crate::domain::dto::{Endpoint, EndpointScheme, GrpcMatch, HttpMatch, MatchConfig, PathSuffixMode, ServerConfig};
use crate::domain::gts_helpers::{
    CATALOG_ONLY_PLUGIN_IDS, PERM_ROUTE_CREATE, PERM_ROUTE_DELETE, PERM_ROUTE_OVERRIDE,
    PERM_ROUTE_READ, PROTOCOL_HTTP,
};
use crate::domain::list_query::ListQuery;
use crate::domain::repo::UpstreamRecord;
use crate::domain::services::management::{Actor, AuthorizeError, ManagementAuthorizer};
use crate::infra::storage::Storage;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn actor() -> Actor {
    Actor { tenant_id: Uuid::new_v4(), principal_id: Uuid::new_v4() }
}

fn upstream_of(tenant: Uuid, alias: &str) -> crate::domain::dto::Upstream {
    crate::domain::dto::Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint { scheme: EndpointScheme::Https, host: "backend.example.com".to_owned(), port: 443 }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn method_of(method: &str) -> crate::domain::dto::HttpMethod {
    use crate::domain::dto::HttpMethod;
    match method {
        "POST" => HttpMethod::Post,
        "PUT" => HttpMethod::Put,
        "DELETE" => HttpMethod::Delete,
        "PATCH" => HttpMethod::Patch,
        _ => HttpMethod::Get,
    }
}

/// An `http` match over one path.
fn http_match(path: &str, methods: &[&str]) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: methods.iter().map(|method| method_of(method)).collect::<Vec<_>>(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn http_get(path: &str) -> MatchConfig {
    http_match(path, &["GET"])
}

fn grpc_match(service: &str, method: &str) -> MatchConfig {
    MatchConfig {
        http: None,
        grpc: Some(GrpcMatch { service: service.to_owned(), method: method.to_owned() }),
    }
}

/// A create-shaped record over `upstream_id`.
fn create_body(upstream_id: Uuid, match_: MatchConfig) -> Route {
    Route {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        upstream_id,
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// A body whose `match` block omits both alternatives.
fn empty_match(upstream_id: Uuid) -> Route {
    create_body(upstream_id, MatchConfig { http: None, grpc: None })
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

/// A resolvability stub: it resolves everything except the catalog-only
/// identifiers and the references the test lists as unresolvable.
struct StubResolver {
    unresolvable: Vec<String>,
    asked: Mutex<Vec<String>>,
}

impl StubResolver {
    fn allowing() -> Arc<Self> {
        Arc::new(Self { unresolvable: Vec::new(), asked: Mutex::new(Vec::new()) })
    }

    fn rejecting(references: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            unresolvable: references.iter().map(|r| (*r).to_owned()).collect(),
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl PluginBindingResolver for StubResolver {
    fn resolve(&self, _tenant_id: Uuid, references: &[String]) -> Result<Vec<PluginBinding>, DomainError> {
        self.asked.lock().extend(references.iter().cloned());
        references
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                if self.unresolvable.contains(reference)
                    || CATALOG_ONLY_PLUGIN_IDS.contains(&reference.as_str())
                {
                    Err(DomainError::field_rejection(
                        "plugins.items",
                        &format!("plugin reference `{position}` does not resolve at binding time"),
                    ))
                } else {
                    Ok(PluginBinding {
                        position: position as u32,
                        plugin_ref: reference.clone(),
                        plugin_uuid: Uuid::parse_str(reference).ok(),
                    })
                }
            })
            .collect()
    }
}

/// The audit notifications the hook observed, for the DESIGN §4.3 assertions.
#[derive(Default)]
struct RecordingHook {
    notifications: Mutex<Vec<ConfigWriteNotification>>,
}

impl RecordingHook {
    fn events(&self) -> Vec<(String, Uuid, String)> {
        self.notifications
            .lock()
            .iter()
            .map(|n| (n.event.to_owned(), n.tenant_id, n.resource_id.clone()))
            .collect()
    }
}

#[async_trait]
impl ConfigWriteHook for RecordingHook {
    async fn on_config_written(&self, _tenant_id: Uuid, _upstream_id: Uuid) -> Result<(), DomainError> {
        Ok(())
    }

    async fn on_route_written(
        &self,
        notification: ConfigWriteNotification,
    ) -> Result<(), DomainError> {
        self.notifications.lock().push(notification);
        Ok(())
    }
}

/// A service over a fresh store, one upstream of the actor's tenant, the stub
/// authorizer and the recording hook.
struct Fixture {
    owner: Actor,
    service: RouteManagementService,
    authorizer: Arc<StubAuthorizer>,
    hook: Arc<RecordingHook>,
    upstreams: Arc<dyn UpstreamRepository>,
    upstream_id: Uuid,
    other_upstream_id: Uuid,
}

fn fixture() -> Fixture {
    let owner = actor();
    let (upstreams, routes, _plugins) = Storage::new().repositories();
    let first = upstream_of(owner.tenant_id, "api.vendor.com");
    let second = upstream_of(owner.tenant_id, "other.vendor.com");
    let first_id = first.id;
    let second_id = second.id;
    upstreams
        .create(owner.tenant_id, UpstreamRecord { upstream: first, plugin_bindings: vec![] })
        .expect("the first upstream is stored");
    upstreams
        .create(owner.tenant_id, UpstreamRecord { upstream: second, plugin_bindings: vec![] })
        .expect("the second upstream is stored");
    let authorizer = StubAuthorizer::allowing();
    let service = RouteManagementService::new(
        routes,
        Arc::clone(&upstreams),
        StubResolver::allowing() as Arc<dyn PluginBindingResolver>,
        Arc::clone(&authorizer) as Arc<dyn ManagementAuthorizer>,
    );
    let hook = Arc::new(RecordingHook::default());
    service.set_config_write_hook(Arc::clone(&hook) as Arc<dyn ConfigWriteHook>);
    Fixture {
        owner,
        service,
        authorizer,
        hook,
        upstreams,
        upstream_id: first_id,
        other_upstream_id: second_id,
    }
}

// ---------------------------------------------------------------------------
// The authorization gate
// ---------------------------------------------------------------------------

/// `inst-rm-create-13`: the permission gate precedes the store, so a deny
/// writes nothing.
#[tokio::test]
async fn the_permission_gate_precedes_the_store_and_denies_before_it() {
    let (upstreams, routes, _plugins) = Storage::new().repositories();
    let owner = actor();
    let upstream = upstream_of(owner.tenant_id, "api.vendor.com");
    let upstream_id = upstream.id;
    upstreams
        .create(owner.tenant_id, UpstreamRecord { upstream, plugin_bindings: vec![] })
        .expect("stored");
    let authorizer = StubAuthorizer::denying(&[PERM_ROUTE_CREATE]);
    let hook = Arc::new(RecordingHook::default());
    let service = RouteManagementService::new(
        routes,
        upstreams,
        StubResolver::allowing() as Arc<dyn PluginBindingResolver>,
        Arc::clone(&authorizer) as Arc<dyn ManagementAuthorizer>,
    );
    service.set_config_write_hook(Arc::clone(&hook) as Arc<dyn ConfigWriteHook>);
    let error = service
        .create(owner, create_body(upstream_id, http_get("/v1/pets")))
        .await
        .expect_err("denied");
    let ManagementError::Authorization(AuthorizeError::Denied { permission, .. }) = error else {
        panic!("expected an authorization denial, got {error}")
    };
    assert_eq!(permission, PERM_ROUTE_CREATE);
    assert_eq!(hook.notifications.lock().len(), 0, "the store was never written");
}

/// Each operation carries its own permission of the route permission set.
#[tokio::test]
async fn each_operation_checks_its_own_permission() {
    let fixture = fixture();
    let owner = fixture.owner;
    let route = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let _ = fixture.service.get(owner, route.id).await.expect("read");
    let _ = fixture.service.list(owner, &ListQuery::default()).await.expect("listed");
    let _ = fixture
        .service
        .replace(owner, route.id, create_body(Uuid::nil(), http_get("/v1/pets")))
        .await
        .expect("replaced");
    fixture.service.delete(owner, route.id).await.expect("deleted");
    let asked = fixture.authorizer.asked();
    assert!(asked.contains(&PERM_ROUTE_CREATE.to_owned()), "{asked:?}");
    assert!(asked.contains(&PERM_ROUTE_READ.to_owned()), "{asked:?}");
    assert!(asked.contains(&PERM_ROUTE_OVERRIDE.to_owned()), "{asked:?}");
    assert!(asked.contains(&PERM_ROUTE_DELETE.to_owned()), "{asked:?}");
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// `inst-rm-create-11b`/`-12`: the identifier is server-generated, the tenant
/// is the caller's, the match type is derived and the declared defaults are
/// materialized.
#[tokio::test]
async fn a_create_assigns_the_identifier_and_materializes_the_defaults() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.priority = 7;
    let stored = fixture.service.create(owner, body).await.expect("created");
    assert_ne!(stored.id, Uuid::nil(), "the identifier is server-generated");
    assert_eq!(stored.tenant_id, owner.tenant_id, "the record is bound to the caller");
    assert_eq!(stored.match_type, crate::domain::dto::RouteMatchType::Http);
    assert_eq!(stored.priority, 7);
    assert!(stored.enabled);
    assert_eq!(stored.upstream_id, fixture.upstream_id);
}

/// `inst-rm-create-7`/`-8`: a match block carrying neither alternative is
/// rejected naming the key, and stores nothing.
#[tokio::test]
async fn a_match_block_without_http_or_grpc_is_rejected() {
    let fixture = fixture();
    let owner = fixture.owner;
    let error = fixture
        .service
        .create(owner, empty_match(fixture.upstream_id))
        .await
        .expect_err("rejected");
    let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = &error else {
        panic!("expected a validation rejection, got {error}")
    };
    assert!(detail.contains("match"), "{detail}");
    assert_eq!(fixture.hook.notifications.lock().len(), 0, "nothing was stored");
}

/// `inst-rm-mv-1`: a match block carrying both alternatives is rejected.
#[tokio::test]
async fn a_match_block_with_both_alternatives_is_rejected() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.match_.grpc = Some(GrpcMatch { service: "svc".to_owned(), method: "Get".to_owned() });
    let error = fixture.service.create(owner, body).await.expect_err("rejected");
    let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = &error else {
        panic!("expected a validation rejection, got {error}")
    };
    assert!(detail.contains("exactly one"), "{detail}");
}

/// `inst-rm-mv-2`: an `http` block requires a non-empty method list and a
/// non-empty path.
#[tokio::test]
async fn an_http_block_requires_methods_and_a_path() {
    let fixture = fixture();
    let owner = fixture.owner;
    for (match_, field) in [
        (http_match("/v1/pets", &[]), "match.http.methods"),
        (http_match("", &["GET"]), "match.http.path"),
    ] {
        let error = fixture
            .service
            .create(owner, create_body(fixture.upstream_id, match_))
            .await
            .expect_err("rejected");
        let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = &error else {
            panic!("expected a validation rejection, got {error}")
        };
        assert!(detail.contains(field), "{detail}");
    }
}

/// `inst-rm-mv-3`: a `grpc` block requires a non-empty `service` and `method`,
/// and is stored as configuration surface only.
#[tokio::test]
async fn a_grpc_block_is_stored_as_configuration_surface_only() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, grpc_match("petstore.PetStore", "GetPet")))
        .await
        .expect("created");
    assert_eq!(stored.match_type, crate::domain::dto::RouteMatchType::Grpc);
    let read = fixture.service.get(owner, stored.id).await.expect("read");
    assert_eq!(read.match_.grpc.as_ref().expect("the grpc block is stored").service, "petstore.PetStore");
}

/// `inst-rm-mv-4`: the `http` defaults of `query_allowlist` and
/// `path_suffix_mode` are materialized as "allow none" and `append`.
#[tokio::test]
async fn an_omitted_query_allowlist_and_path_suffix_mode_persist_the_defaults() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let http = stored.match_.http.as_ref().expect("the http block is stored");
    assert!(http.query_allowlist.is_empty(), "an empty allowlist permits no query parameter");
    assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
}

/// `inst-rm-create-3` .. `-5`, `inst-rm-ur-2`/`-3`: an unresolvable
/// `upstream_id` is a not-found that discloses nothing.
#[tokio::test]
async fn an_unresolvable_upstream_reference_is_not_found() {
    let fixture = fixture();
    let owner = fixture.owner;
    // A foreign-tenant upstream the actor can see the identifier of.
    let foreign = actor();
    let (upstreams, _routes, _plugins) = Storage::new().repositories();
    let foreign_upstream = upstream_of(foreign.tenant_id, "foreign.vendor.com");
    let foreign_id = foreign_upstream.id;
    upstreams
        .create(foreign.tenant_id, UpstreamRecord { upstream: foreign_upstream, plugin_bindings: vec![] })
        .expect("stored");
    for upstream_id in [Uuid::new_v4(), foreign_id] {
        let error = fixture
            .service
            .create(owner, create_body(upstream_id, http_get("/v1/pets")))
            .await
            .expect_err("rejected");
        assert!(error.is_not_found(), "{error}");
        let ManagementError::Domain(DomainError::NotFound { resource_type }) = &error else {
            panic!("expected a not-found, got {error}")
        };
        assert_eq!(*resource_type, "route", "no information about the upstream is disclosed");
    }
}

/// `inst-rm-create-9` .. `-11`: a second enabled route over the same match
/// keys is a `409` and leaves the store unchanged.
#[tokio::test]
async fn a_second_enabled_route_with_the_same_match_keys_conflicts() {
    let fixture = fixture();
    let owner = fixture.owner;
    fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("the first route is stored");
    let error = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect_err("conflicted");
    assert!(matches!(
        error,
        ManagementError::Domain(DomainError::Conflict { .. })
    ), "{error}");
    assert_eq!(fixture.service.list(owner, &ListQuery::default()).await.expect("listed").len(), 1);
}

/// A route over a *different* upstream, or at a different priority, or on a
/// different method, does not collide.
#[tokio::test]
async fn a_different_upstream_priority_or_method_does_not_collide() {
    let fixture = fixture();
    let owner = fixture.owner;
    fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("stored");
    for (upstream_id, match_) in [
        (fixture.other_upstream_id, http_get("/v1/pets")),
        (fixture.upstream_id, http_get("/v1/pets/other")),
        (fixture.upstream_id, http_match("/v1/pets", &["POST"])),
    ] {
        let mut body = create_body(upstream_id, match_);
        body.priority = 5;
        fixture.service.create(owner, body).await.expect("no collision");
    }
}

/// `inst-rm-uniq-3c`: the determinism guarantee also holds for a `grpc` block
/// on its `(service, method)` keys.
#[tokio::test]
async fn a_second_grpc_route_with_the_same_service_and_method_conflicts() {
    let fixture = fixture();
    let owner = fixture.owner;
    fixture
        .service
        .create(owner, create_body(fixture.upstream_id, grpc_match("petstore.PetStore", "GetPet")))
        .await
        .expect("stored");
    let error = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, grpc_match("petstore.PetStore", "GetPet")))
        .await
        .expect_err("conflicted");
    assert!(matches!(error, ManagementError::Domain(DomainError::Conflict { .. })), "{error}");
}

// ---------------------------------------------------------------------------
// Enable and disable
// ---------------------------------------------------------------------------

/// `inst-rm-enab-5`, `inst-rm-uniq-4`: a disabled route stays addressable and
/// is excluded from the comparison, while a re-enable that would collide is
/// rejected.
#[tokio::test]
async fn a_disabled_route_stays_addressable_and_is_excluded_from_the_comparison() {
    let fixture = fixture();
    let owner = fixture.owner;
    // The enabled route that holds the match keys.
    fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    // A disabled route over the same keys is accepted, because the
    // comparison runs between *enabled* routes only.
    let mut disabled = create_body(fixture.upstream_id, http_get("/v1/pets"));
    disabled.enabled = false;
    let stored = fixture.service.create(owner, disabled).await.expect("created");
    assert!(!stored.enabled);
    // `inst-rm-uniq-3b`: re-enabling it collides with the enabled sibling.
    let error = fixture
        .service
        .replace(owner, stored.id, create_body(Uuid::nil(), http_get("/v1/pets")))
        .await
        .expect_err("the re-enable collides with the enabled sibling");
    assert!(matches!(error, ManagementError::Domain(DomainError::Conflict { .. })), "{error}");
    let read = fixture.service.get(owner, stored.id).await.expect("read");
    assert!(!read.enabled, "the store kept the disabled state");
}

/// A disabled route is out of the comparison, so an enabled route over the
/// same keys is accepted while it stays disabled.
#[tokio::test]
async fn an_enabled_route_over_a_disabled_route_keys_is_accepted() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut disabled = create_body(fixture.upstream_id, http_get("/v1/pets"));
    disabled.enabled = false;
    fixture.service.create(owner, disabled).await.expect("created");
    let second = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("the disabled route is out of the comparison");
    assert!(second.enabled);
}

/// `inst-rm-enab-6`/`-7`: a replacement writing `enabled: false` disables the
/// route, and one writing `enabled: true` re-enables it.
#[tokio::test]
async fn the_replacement_is_the_enable_and_disable_path() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let mut disabled = create_body(Uuid::nil(), http_get("/v1/pets"));
    disabled.enabled = false;
    let replaced = fixture.service.replace(owner, stored.id, disabled).await.expect("replaced");
    assert!(!replaced.enabled);
    let enabled = fixture.service.replace(owner, stored.id, create_body(Uuid::nil(), http_get("/v1/pets"))).await.expect("re-enabled");
    assert!(enabled.enabled);
}

// ---------------------------------------------------------------------------
// Replace
// ---------------------------------------------------------------------------

/// `inst-rm-replace-3`: the immutable fields are retained from the stored
/// record.
#[tokio::test]
async fn a_replacement_retains_the_immutable_fields() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let mut replacement = create_body(Uuid::nil(), http_get("/v1/pets"));
    replacement.priority = 9;
    replacement.tags = vec!["edge".to_owned()];
    let replaced = fixture.service.replace(owner, stored.id, replacement).await.expect("replaced");
    assert_eq!(replaced.id, stored.id);
    assert_eq!(replaced.tenant_id, stored.tenant_id);
    assert_eq!(replaced.upstream_id, fixture.upstream_id, "the route never moves upstream");
    assert_eq!(replaced.priority, 9);
    assert_eq!(replaced.tags, vec!["edge".to_owned()]);
}

/// `inst-rm-replace-4`/`-5`: a replacement that supplies `upstream_id` is an
/// immutable-field violation regardless of the supplied value.
#[tokio::test]
async fn a_replacement_supplying_an_upstream_id_is_rejected() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    for upstream_id in [fixture.upstream_id, fixture.other_upstream_id, Uuid::new_v4()] {
        let mut replacement = create_body(Uuid::nil(), http_get("/v1/pets"));
        replacement.upstream_id = upstream_id;
        let error = fixture.service.replace(owner, stored.id, replacement).await.expect_err("rejected");
        let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = &error else {
            panic!("expected an immutable-field rejection, got {error}")
        };
        assert!(detail.contains("upstream_id"), "{detail}");
    }
    let read = fixture.service.get(owner, stored.id).await.expect("read");
    assert_eq!(read.upstream_id, fixture.upstream_id, "the store is unchanged");
}

/// `inst-rm-replace-10`: omitted optional blocks are cleared with the declared
/// defaults materialized.
#[tokio::test]
async fn a_replacement_clears_the_omitted_optional_blocks() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.tags = vec!["edge".to_owned()];
    body.plugins = Some(crate::domain::dto::PluginsConfig {
        sharing: crate::domain::dto::SharingMode::Inherit,
        items: vec![],
    });
    let stored = fixture.service.create(owner, body).await.expect("created");
    assert_eq!(stored.tags.len(), 1);
    assert!(stored.plugins.is_some());
    let replaced = fixture
        .service
        .replace(owner, stored.id, create_body(Uuid::nil(), http_get("/v1/pets")))
        .await
        .expect("replaced");
    assert!(replaced.tags.is_empty(), "the omitted block is cleared");
    assert!(replaced.plugins.is_none(), "the omitted block is cleared");
}

/// `inst-rm-replace-2`: a missing, foreign or removed identifier is not-found.
#[tokio::test]
async fn a_replacement_of_a_foreign_or_missing_route_is_not_found() {
    let fixture = fixture();
    let owner = fixture.owner;
    fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let foreign = actor();
    let error = fixture
        .service
        .replace(foreign, Uuid::new_v4(), create_body(Uuid::nil(), http_get("/v1/pets")))
        .await
        .expect_err("not found");
    assert!(error.is_not_found(), "{error}");
}

// ---------------------------------------------------------------------------
// List and read
// ---------------------------------------------------------------------------

/// `inst-rm-list-4` .. `-7`: the OData parameters are applied inside the
/// caller's tenant scope.
#[tokio::test]
async fn the_list_applies_the_odata_query_inside_the_tenant_scope() {
    let fixture = fixture();
    let owner = fixture.owner;
    let first = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let mut second = create_body(fixture.other_upstream_id, http_get("/v1/orders"));
    second.priority = 10;
    fixture.service.create(owner, second).await.expect("created");
    // `$filter` selects one upstream's routes.
    let filtered = fixture
        .service
        .list(
            owner,
            &ListQuery::parse_route(&[("$filter".to_owned(), format!("upstream_id eq '{}'", fixture.upstream_id))])
                .expect("parsed"),
        )
        .await
        .expect("listed");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, first.id);
    // `$orderby` sorts by priority descending by default store order.
    let ordered = fixture
        .service
        .list(owner, &ListQuery::parse_route(&[("$orderby".to_owned(), "priority desc".to_owned())]).expect("parsed"))
        .await
        .expect("listed");
    assert_eq!(ordered.first().map(|route| route.priority), Some(10));
    // `$top` and `$skip` page the result.
    let paged = fixture
        .service
        .list(owner, &ListQuery::parse_route(&[("$top".to_owned(), "1".to_owned()), ("$skip".to_owned(), "1".to_owned())]).expect("parsed"))
        .await
        .expect("listed");
    assert_eq!(paged.len(), 1);
    assert_eq!(paged[0].id, first.id);
}

/// `inst-rm-list-5`/`-6`: an unsupported, malformed or out-of-range parameter
/// is a validation error naming the parameter.
#[tokio::test]
async fn an_unsupported_or_out_of_range_parameter_is_rejected() {
    for query in [
        vec![("$unsupported".to_owned(), "1".to_owned())],
        vec![("$top".to_owned(), "101".to_owned())],
        vec![("$top".to_owned(), "0".to_owned())],
        vec![("$filter".to_owned(), "not_a_field eq 'x'".to_owned())],
        vec![("$orderby".to_owned(), "not_a_field".to_owned())],
    ] {
        let error = ListQuery::parse_route(&query).expect_err("rejected");
        assert!(matches!(error, crate::domain::error::DomainError::ValidationError { .. }), "{error:?}");
    }
}

/// `inst-rm-list-3a`: a foreign-tenant route is not-found on read, and never
/// appears in a list response.
#[tokio::test]
async fn a_foreign_route_is_not_found_and_never_listed() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let foreign = actor();
    assert!(fixture.service.get(foreign, stored.id).await.expect_err("not found").is_not_found());
    let listed = fixture.service.list(foreign, &ListQuery::default()).await.expect("listed");
    assert!(listed.is_empty(), "no ancestor-tenant or foreign route is listed");
}

// ---------------------------------------------------------------------------
// Delete and the cascade
// ---------------------------------------------------------------------------

/// `inst-rm-del-3`/`-5`: the delete removes the record and its child rows, and
/// leaves no queryable trace.
#[tokio::test]
async fn a_delete_removes_the_record_and_leaves_no_queryable_trace() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.tags = vec!["edge".to_owned()];
    let stored = fixture.service.create(owner, body).await.expect("created");
    fixture.service.delete(owner, stored.id).await.expect("deleted");
    assert!(fixture.service.get(owner, stored.id).await.expect_err("gone").is_not_found());
    assert!(fixture.service.list(owner, &ListQuery::default()).await.expect("listed").is_empty());
    // A removed record is not addressable: replace and delete are not-found.
    assert!(
        fixture
            .service
            .replace(owner, stored.id, create_body(Uuid::nil(), http_get("/v1/pets")))
            .await
            .expect_err("not found")
            .is_not_found()
    );
    assert!(fixture.service.delete(owner, stored.id).await.expect_err("not found").is_not_found());
}

/// `inst-rm-del-4b`, `inst-rm-st-9`: deleting the owning upstream removes its
/// routes through the repository cascade.
#[tokio::test]
async fn deleting_the_owning_upstream_removes_its_routes() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    fixture
        .upstreams
        .delete(owner.tenant_id, fixture.upstream_id)
        .expect("the upstream cascades to its routes");
    assert!(fixture.service.get(owner, stored.id).await.expect_err("removed").is_not_found());
    assert!(fixture.service.list(owner, &ListQuery::default()).await.expect("listed").is_empty());
}

// ---------------------------------------------------------------------------
// The audit event and the write ordering
// ---------------------------------------------------------------------------

/// `inst-rm-create-14`/`inst-rm-replace-13`/`inst-rm-del-7`: every accepted
/// write supplies the DESIGN §4.3 audit fields to the hook, after the store
/// write.
#[tokio::test]
async fn every_write_supplies_the_audit_fields_to_the_hook() {
    let fixture = fixture();
    let owner = fixture.owner;
    let stored = fixture
        .service
        .create(owner, create_body(fixture.upstream_id, http_get("/v1/pets")))
        .await
        .expect("created");
    let created = fixture.hook.events();
    assert_eq!(created, vec![("route.create".to_owned(), owner.tenant_id, crate::domain::gts_helpers::route_resource_id(stored.id))]);

    fixture
        .service
        .replace(owner, stored.id, create_body(Uuid::nil(), http_get("/v1/pets")))
        .await
        .expect("replaced");
    let replaced = fixture.hook.events();
    assert_eq!(replaced.len(), 2);
    assert_eq!(replaced[1].0, "route.replace");

    fixture.service.delete(owner, stored.id).await.expect("deleted");
    let deleted = fixture.hook.events();
    assert_eq!(deleted.len(), 3);
    assert_eq!(deleted[2].0, "route.delete");

    let notifications = fixture.hook.notifications.lock().clone();
    for notification in &notifications {
        assert_eq!(notification.principal_id, owner.principal_id, "attributed to the caller");
        assert_eq!(notification.outcome, "accepted");
        assert_eq!(
            notification.resource_id,
            format!("gts.cf.core.oagw.route.v1~{}", stored.id),
            "the anonymous GTS resource identifier"
        );
        assert_eq!(notification.upstream_id, Some(fixture.upstream_id), "the flush derives the key set");
    }
}

/// A failed write supplies no audit event, because nothing was written.
#[tokio::test]
async fn a_failed_write_supplies_no_audit_event() {
    let fixture = fixture();
    let owner = fixture.owner;
    let _ = fixture
        .service
        .create(owner, empty_match(fixture.upstream_id))
        .await
        .expect_err("rejected");
    let _ = fixture
        .service
        .create(owner, create_body(Uuid::new_v4(), http_get("/v1/pets")))
        .await
        .expect_err("not found");
    assert!(fixture.hook.notifications.lock().is_empty(), "no audit event for a rejected write");
}

// ---------------------------------------------------------------------------
// Plugin bindings
// ---------------------------------------------------------------------------

/// `cpt-cf-oagw-dod-route-management-route-overrides`: the write path resolves
/// every `plugins.items[]` entry through the plugin-catalog boundary, and a
/// catalog-only reference is rejected at binding time.
#[tokio::test]
async fn the_write_path_resolves_the_plugin_bindings_at_binding_time() {
    let (upstreams, routes, _plugins) = Storage::new().repositories();
    let owner = actor();
    let upstream = upstream_of(owner.tenant_id, "api.vendor.com");
    let upstream_id = upstream.id;
    upstreams
        .create(owner.tenant_id, UpstreamRecord { upstream, plugin_bindings: vec![] })
        .expect("stored");
    let resolver = StubResolver::rejecting(&[CATALOG_ONLY_PLUGIN_IDS[0]]);
    let service = RouteManagementService::new(
        routes,
        upstreams,
        resolver.clone() as Arc<dyn PluginBindingResolver>,
        StubAuthorizer::allowing() as Arc<dyn ManagementAuthorizer>,
    );
    let mut body = create_body(upstream_id, http_get("/v1/pets"));
    body.plugins = Some(crate::domain::dto::PluginsConfig {
        sharing: crate::domain::dto::SharingMode::Inherit,
        items: vec![CATALOG_ONLY_PLUGIN_IDS[0].to_owned()],
    });
    let error = service.create(owner, body).await.expect_err("rejected");
    let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = &error else {
        panic!("expected a binding-time rejection, got {error}")
    };
    assert!(detail.contains("plugins.items"), "{detail}");
    assert_eq!(resolver.asked.lock().len(), 1, "the check ran once, before the store");
}

/// The stored route-level overrides keep the shape they were written with, and
/// the binding positions are contiguous from zero with `plugin_ref` always
/// present.
#[tokio::test]
async fn the_stored_overrides_and_binding_positions_are_preserved() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.tags = vec!["edge".to_owned()];
    body.cors = Some(crate::domain::dto::CorsConfig {
        sharing: crate::domain::dto::SharingMode::Inherit,
        enabled: true,
        allowed_origins: Some(vec!["https://app.example.com".to_owned()]),
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: vec![],
        allow_credentials: false,
    });
    body.plugins = Some(crate::domain::dto::PluginsConfig {
        sharing: crate::domain::dto::SharingMode::Inherit,
        items: vec![
            crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
            crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        ],
    });
    let stored = fixture.service.create(owner, body).await.expect("created");
    assert_eq!(stored.tags, vec!["edge".to_owned()]);
    assert!(stored.cors.as_ref().expect("the cors override is stored").enabled);
    let bindings = fixture
        .service
        .route_repository()
        .get(owner.tenant_id, stored.id)
        .expect("read")
        .plugin_bindings;
    assert_eq!(bindings.len(), 2);
    for (position, binding) in bindings.iter().enumerate() {
        assert_eq!(binding.position as usize, position, "positions are contiguous from zero");
        assert!(!binding.plugin_ref.is_empty(), "`plugin_ref` is always stored");
    }
    assert_eq!(bindings[0].plugin_ref, crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID);
}

/// A `disabled` route is still replaceable through the management API.
#[tokio::test]
async fn a_disabled_route_stays_replaceable() {
    let fixture = fixture();
    let owner = fixture.owner;
    let mut body = create_body(fixture.upstream_id, http_get("/v1/pets"));
    body.enabled = false;
    let stored = fixture.service.create(owner, body).await.expect("created");
    let read = fixture.service.get(owner, stored.id).await.expect("read");
    assert!(!read.enabled, "the record stays addressable");
}
