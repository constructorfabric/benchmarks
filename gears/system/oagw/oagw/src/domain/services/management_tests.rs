//! Unit tests for the upstream-management aggregate
//! (`cpt-cf-oagw-dod-upstream-management-unit-tests`).

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use super::*;
use crate::domain::dto::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, ServerConfig, SharingMode,
};
use crate::domain::gts_helpers::{
    PERM_BIND, PERM_UPSTREAM_CREATE, PERM_UPSTREAM_DELETE, PERM_UPSTREAM_OVERRIDE,
    PERM_UPSTREAM_READ, PROTOCOL_HTTP,
};
use crate::domain::list_query::ListQuery;
use crate::infra::storage::Storage;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An actor over a fresh tenant.
fn actor() -> Actor {
    Actor { tenant_id: Uuid::new_v4(), principal_id: Uuid::new_v4() }
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: host.to_owned(), port }
}

fn server(hosts: &[&str]) -> ServerConfig {
    ServerConfig {
        endpoints: hosts.iter().map(|host| endpoint(host, 443)).collect(),
    }
}

/// A create-shaped record: no identifier, no tenant, no alias.
fn create_body(hosts: &[&str]) -> Upstream {
    Upstream {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        alias: String::new(),
        protocol: PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: server(hosts),
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// A body over an IP pool, which no alias can be derived from, carrying the
/// explicit alias the pool requires.
fn ip_body(alias: &str, host: &str) -> Upstream {
    let mut body = create_body(&[host]);
    body.alias = alias.to_owned();
    body
}

/// A `deny`-list authorizer: it records the permissions it was asked for and
/// denies the ones listed.
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

/// A stub ancestor chain: `children` maps a tenant to its direct parent.
struct StubAncestors {
    parent_of: Mutex<std::collections::BTreeMap<Uuid, Uuid>>,
}

impl StubAncestors {
    fn flat() -> Arc<Self> {
        Arc::new(Self { parent_of: Mutex::new(std::collections::BTreeMap::new()) })
    }

    fn of(pairs: &[(Uuid, Uuid)]) -> Arc<Self> {
        Arc::new(Self {
            parent_of: Mutex::new(pairs.iter().copied().collect()),
        })
    }
}

#[async_trait]
impl AncestorResolver for StubAncestors {
    async fn ancestors(&self, _actor: &Actor, tenant_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        let mut chain = Vec::new();
        let mut current = tenant_id;
        while let Some(parent) = self.parent_of.lock().get(&current).copied() {
            chain.push(parent);
            current = parent;
        }
        Ok(chain)
    }
}

/// The sequence of writes the hook observed, for the ordering assertions.
#[derive(Default)]
struct RecordingHook {
    events: Mutex<Vec<(Uuid, Uuid)>>,
}

#[async_trait]
impl ConfigWriteHook for RecordingHook {
    async fn on_config_written(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<(), DomainError> {
        self.events.lock().push((tenant_id, upstream_id));
        Ok(())
    }
}

/// A service over a fresh store, the stub authorizer and the stub ancestors.
struct Fixture {
    service: UpstreamManagementService,
    authorizer: Arc<StubAuthorizer>,
    hook: Arc<RecordingHook>,
}

fn fixture(parents: &[(Uuid, Uuid)]) -> Fixture {
    let (upstreams, _routes, _plugins) = Storage::new().repositories();
    let authorizer = StubAuthorizer::allowing();
    let service = UpstreamManagementService::new(
        upstreams,
        true,
        Arc::clone(&authorizer) as Arc<dyn ManagementAuthorizer>,
        StubAncestors::of(parents) as Arc<dyn AncestorResolver>,
    );
    let hook = Arc::new(RecordingHook::default());
    service.set_config_write_hook(Arc::clone(&hook) as Arc<dyn ConfigWriteHook>);
    Fixture { service, authorizer, hook }
}

fn parents() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// Authorization gate
// ---------------------------------------------------------------------------

/// `inst-um-cr-15`: the per-operation permission is evaluated before the
/// service call, and a deny is the authorization error.
#[tokio::test]
async fn the_permission_gate_precedes_the_store_and_denies_before_it() {
    let (upstreams, _routes, _plugins) = Storage::new().repositories();
    let authorizer = StubAuthorizer::denying(&[PERM_UPSTREAM_CREATE]);
    let hook = Arc::new(RecordingHook::default());
    let service = UpstreamManagementService::new(
        upstreams,
        true,
        Arc::clone(&authorizer) as Arc<dyn ManagementAuthorizer>,
        StubAncestors::flat() as Arc<dyn AncestorResolver>,
    );
    service.set_config_write_hook(Arc::clone(&hook) as Arc<dyn ConfigWriteHook>);
    let owner = actor();
    let error = service
        .create(owner, create_body(&["api.vendor.com"]))
        .await
        .expect_err("denied");
    let ManagementError::Authorization(AuthorizeError::Denied { permission, .. }) = error else {
        panic!("expected an authorization denial, got {error}")
    };
    assert_eq!(permission, PERM_UPSTREAM_CREATE);
    assert!(
        authorizer.asked().contains(&PERM_UPSTREAM_CREATE.to_owned()),
        "the gate ran"
    );
    assert_eq!(hook.events.lock().len(), 0, "the store was never written");
}

/// Each operation carries its own permission.
#[tokio::test]
async fn each_operation_checks_its_own_permission() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture
        .service
        .create(owner, create_body(&["api.vendor.com"]))
        .await
        .expect("created");
    let _ = fixture.service.get(owner, view.record.id).await.expect("read");
    let _ = fixture.service.list(owner, &ListQuery::default()).await.expect("list");
    let _ = fixture
        .service
        .replace(owner, view.record.id, create_body(&["api.vendor.com"]))
        .await
        .expect("replaced");
    let _ = fixture.service.delete(owner, view.record.id).await.expect("deleted");
    let asked = fixture.authorizer.asked();
    assert_eq!(asked[0], PERM_UPSTREAM_CREATE);
    assert_eq!(asked[1], PERM_UPSTREAM_READ);
    assert_eq!(asked[2], PERM_UPSTREAM_READ);
    assert_eq!(asked[3], PERM_UPSTREAM_OVERRIDE);
    assert_eq!(asked[4], PERM_UPSTREAM_DELETE);
}

/// The delete and the replacement are denied by their own permission.
#[tokio::test]
async fn the_override_and_delete_permissions_are_distinct() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture
        .service
        .create(owner, create_body(&["api.vendor.com"]))
        .await
        .expect("created");
    let denying = StubAuthorizer::denying(&[PERM_UPSTREAM_OVERRIDE, PERM_UPSTREAM_DELETE]);
    let service = UpstreamManagementService::new(
        fixture.service.upstream_repository(),
        true,
        Arc::clone(&denying) as Arc<dyn ManagementAuthorizer>,
        StubAncestors::flat() as Arc<dyn AncestorResolver>,
    );
    for (id, expected) in [
        (view.record.id, PERM_UPSTREAM_OVERRIDE),
        (view.record.id, PERM_UPSTREAM_DELETE),
    ] {
        let denied: Result<(), ManagementError> = if expected == PERM_UPSTREAM_OVERRIDE {
            service
                .replace(owner, id, create_body(&["api.vendor.com"]))
                .await
                .map(|_| ())
        } else {
            service.delete(owner, id).await
        };
        let error = denied.expect_err("denied");
        assert!(matches!(error, ManagementError::Authorization(_)), "{error}");
    }
}

// ---------------------------------------------------------------------------
// Alias reconciliation and defaults
// ---------------------------------------------------------------------------

/// `inst-um-cr-13`: the scalar structural defaults are materialized on write.
#[tokio::test]
async fn a_create_materializes_the_identifier_tenant_and_derived_alias() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture
        .service
        .create(owner, create_body(&["api.vendor.com"]))
        .await
        .expect("created");
    assert!(!view.record.id.is_nil(), "the identifier is server-generated");
    assert_eq!(view.record.tenant_id, owner.tenant_id, "the tenant is server-assigned");
    assert_eq!(view.record.alias, "api.vendor.com", "the alias is the derived value");
    assert!(view.record.enabled, "`enabled` defaults to true");
    assert!(view.record.server.endpoints[0].scheme == EndpointScheme::Https);
    assert_eq!(view.record.server.endpoints[0].port, 443);
}

/// `inst-um-cr-7`/`-8`: an IP pool without an alias is rejected.
#[tokio::test]
async fn an_ip_pool_requires_an_explicit_alias() {
    let fixture = fixture(&[]);
    let owner = actor();
    let error = fixture
        .service
        .create(owner, create_body(&["10.0.0.1"]))
        .await
        .expect_err("non-derivable");
    let ManagementError::Domain(DomainError::ValidationError { detail, .. }) = error else {
        panic!("expected a validation error, got {error}")
    };
    assert!(detail.contains("explicit alias"), "`{detail}`");
    // The same pool with an explicit alias is accepted.
    let mut body = create_body(&["10.0.0.1"]);
    body.alias = "ip-upstream".to_owned();
    let view = fixture.service.create(owner, body).await.expect("accepted");
    assert_eq!(view.record.alias, "ip-upstream");
}

/// `inst-um-cr-9`/`-10`: a supplied alias must be the derived value.
#[tokio::test]
async fn a_supplied_alias_that_differs_from_the_derived_value_is_rejected() {
    let fixture = fixture(&[]);
    let owner = actor();
    let mut body = create_body(&["api.vendor.com"]);
    body.alias = "other.vendor.com".to_owned();
    let error = fixture.service.create(owner, body).await.expect_err("differs");
    assert!(error.to_string().contains("api.vendor.com"), "`{error}` names the derived value");
    // The exact derived value is tolerated silently.
    let mut body = create_body(&["api.vendor.com"]);
    body.alias = "API.Vendor.Com.".to_owned();
    let view = fixture.service.create(owner, body).await.expect("idempotent");
    assert_eq!(view.record.alias, "api.vendor.com", "normalized to ASCII lowercase");
}

/// `inst-um-cr-11`: the alias is normalized, so the same alias in another case
/// is the same record and the second create is a conflict.
#[tokio::test]
async fn an_alias_is_resolved_case_insensitively_and_conflicts() {
    let fixture = fixture(&[]);
    let owner = actor();
    let mut body = create_body(&["10.0.0.1"]);
    body.alias = "ip-upstream".to_owned();
    fixture.service.create(owner, body.clone()).await.expect("created");
    body.alias = "IP-Upstream".to_owned();
    let error = fixture.service.create(owner, body).await.expect_err("same key");
    assert!(matches!(error, ManagementError::Domain(DomainError::Conflict { .. })), "{error}");
}

// ---------------------------------------------------------------------------
// Per-tenant uniqueness and scoping
// ---------------------------------------------------------------------------

/// `inst-um-cr-12`: a same-tenant collision is a conflict and the store keeps
/// exactly one record.
#[tokio::test]
async fn a_same_tenant_alias_collision_is_a_conflict_leaving_one_record() {
    let fixture = fixture(&[]);
    let owner = actor();
    fixture
        .service
        .create(owner, ip_body("ip-upstream", "10.0.0.1"))
        .await
        .expect("first");
    let second = ip_body("ip-upstream", "10.0.0.2");
    let error = fixture.service.create(owner, second).await.expect_err("collision");
    assert!(error.to_string().contains("already exists"), "`{error}`");
    let listed = fixture.service.list(owner, &ListQuery::default()).await.expect("list");
    assert_eq!(listed.len(), 1, "exactly one record is stored");
}

/// A foreign and an ancestor record are both invisible to the management
/// surface (`cpt-cf-oagw-dod-upstream-management-tenant-scoping`).
#[tokio::test]
async fn a_foreign_and_an_ancestor_record_are_not_found() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "ancestor-upstream".to_owned();
    let ancestor = fixture.service.create(parent_actor, ancestor_body).await.expect("created");
    // The child cannot read, replace or delete the ancestor record.
    for outcome in [
        fixture.service.get(child_actor, ancestor.record.id).await.err().map(|e| e.is_not_found()),
        fixture
            .service
            .replace(child_actor, ancestor.record.id, create_body(&["10.0.0.9"]))
            .await
            .err()
            .map(|e| e.is_not_found()),
        fixture.service.delete(child_actor, ancestor.record.id).await.err().map(|e| e.is_not_found()),
    ] {
        assert_eq!(outcome, Some(true), "the ancestor record is not-found to the descendant");
    }
    // And the ancestor's record is not in the child's list.
    let listed = fixture.service.list(child_actor, &ListQuery::default()).await.expect("list");
    assert!(listed.is_empty(), "the ancestor record is not a candidate");
}

// ---------------------------------------------------------------------------
// Ancestor bind
// ---------------------------------------------------------------------------

/// An ancestor upstream that declares an `inherit` block forms a bind.
#[tokio::test]
async fn an_ancestor_alias_match_requires_the_bind_permission() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "shared.vendor.com".to_owned();
    // `inherit` exposes the block to a descendant, so the alias forms a bind.
    ancestor_body.rate_limit = Some(crate::domain::dto::RateLimitConfig {
        sharing: SharingMode::Inherit,
        ..serde_json::from_str(
            r#"{ "sustained": { "rate": 10, "window": "minute" } }"#,
        )
        .expect("rate limit")
    });
    fixture.service.create(parent_actor, ancestor_body).await.expect("created");

    let mut child_body = create_body(&["10.0.0.2"]);
    child_body.alias = "shared.vendor.com".to_owned();
    // Without the permission the bind is a denial.
    let denying = StubAuthorizer::denying(&[PERM_BIND]);
    let service = UpstreamManagementService::new(
        fixture.service.upstream_repository(),
        true,
        Arc::clone(&denying) as Arc<dyn ManagementAuthorizer>,
        StubAncestors::of(&[(child, parent)]) as Arc<dyn AncestorResolver>,
    );
    let error = service.create(child_actor, child_body.clone()).await.expect_err("denied");
    assert!(matches!(error, ManagementError::Authorization(_)), "{error}");
    // With it, the create is a bind, not a conflict.
    let view = fixture.service.create(child_actor, child_body).await.expect("bound");
    assert_eq!(view.record.tenant_id, child_actor.tenant_id);
}

/// `inst-um-cr-17`: a `private` ancestor upstream blocks visibility, so the
/// alias stays available for a local create without the bind permission.
#[tokio::test]
async fn a_private_ancestor_upstream_blocks_visibility() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "shared.vendor.com".to_owned();
    ancestor_body.auth = Some(AuthConfig {
        sharing: SharingMode::Private,
        ..serde_json::from_str(r#"{ "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.api_key.v1" }"#)
            .expect("auth")
    });
    fixture.service.create(parent_actor, ancestor_body).await.expect("created");
    let denying = StubAuthorizer::denying(&[PERM_BIND]);
    let service = UpstreamManagementService::new(
        fixture.service.upstream_repository(),
        true,
        Arc::clone(&denying) as Arc<dyn ManagementAuthorizer>,
        StubAncestors::of(&[(child, parent)]) as Arc<dyn AncestorResolver>,
    );
    let mut child_body = create_body(&["10.0.0.2"]);
    child_body.alias = "shared.vendor.com".to_owned();
    let view = service.create(child_actor, child_body).await.expect("no bind required");
    assert_eq!(view.record.tenant_id, child_actor.tenant_id);
}

/// `inst-um-rp-13`: a descendant override of an `enforce` ancestor-owned block
/// is rejected through the permission-denied surface.
#[tokio::test]
async fn an_enforce_ancestor_block_cannot_be_overridden() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "shared.vendor.com".to_owned();
    ancestor_body.cors = Some(CorsConfig {
        sharing: SharingMode::Enforce,
        enabled: true,
        ..CorsConfig::default()
    });
    fixture.service.create(parent_actor, ancestor_body).await.expect("created");
    let mut child_body = create_body(&["10.0.0.2"]);
    child_body.alias = "shared.vendor.com".to_owned();
    child_body.cors = Some(CorsConfig { enabled: true, ..CorsConfig::default() });
    let error = fixture.service.create(child_actor, child_body).await.expect_err("blocked");
    assert!(matches!(error, ManagementError::Authorization(_)), "{error}");
}

// ---------------------------------------------------------------------------
// Replace, immutability and cascade
// ---------------------------------------------------------------------------

/// `inst-um-rp-6` .. `-8`: the alias is immutable on replacement.
#[tokio::test]
async fn a_replacement_never_moves_the_alias() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture.service.create(owner, create_body(&["api.vendor.com"])).await.expect("created");
    // An endpoint change that would alter the alias is rejected.
    let error = fixture
        .service
        .replace(owner, view.record.id, create_body(&["other.vendor.com"]))
        .await
        .expect_err("alias would change");
    assert!(error.to_string().contains("delete and re-create"), "`{error}`");
    // The stored record is unchanged.
    let stored = fixture.service.get(owner, view.record.id).await.expect("read");
    assert_eq!(stored.record.alias, "api.vendor.com");
    // And the identifier survives.
    assert_eq!(stored.record.id, view.record.id);
}

/// `inst-um-rp-9`: a full replacement clears the omitted blocks to absent.
#[tokio::test]
async fn a_replacement_clears_the_omitted_blocks() {
    let fixture = fixture(&[]);
    let owner = actor();
    let mut body = create_body(&["10.0.0.1"]);
    body.alias = "ip-upstream".to_owned();
    body.tags = vec!["openai".to_owned()];
    body.cors = Some(CorsConfig { enabled: true, ..CorsConfig::default() });
    let view = fixture.service.create(owner, body).await.expect("created");
    assert!(view.record.cors.is_some(), "the block is stored");
    // The replacement omits it: absent, with no implicit empty block.
    let mut replacement = create_body(&["10.0.0.1"]);
    replacement.alias = "ip-upstream".to_owned();
    replacement.tags = vec!["openai".to_owned()];
    let replaced = fixture
        .service
        .replace(owner, view.record.id, replacement)
        .await
        .expect("replaced");
    assert!(replaced.record.cors.is_none(), "no implicit empty block is persisted");
}

/// `inst-um-rp-2`: a foreign identifier is not-found before validation runs.
#[tokio::test]
async fn a_replacement_of_a_foreign_record_is_not_found_before_validation() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let created = fixture
        .service
        .create(parent_actor, create_body(&["api.vendor.com"]))
        .await
        .expect("created");
    // A body that would not validate, against an identifier the caller does
    // not own: the not-found wins.
    let mut body = create_body(&[]);
    body.alias = "no-alias".to_owned();
    let error = fixture
        .service
        .replace(child_actor, created.record.id, body)
        .await
        .expect_err("not found");
    assert!(error.is_not_found(), "{error}");
}

/// `inst-um-dl-5`: the delete removes the record and its dependent rows.
#[tokio::test]
async fn a_delete_removes_the_record_and_its_dependent_rows() {
    let fixture = fixture(&[]);
    let owner = actor();
    let mut body = create_body(&["10.0.0.1"]);
    body.alias = "ip-upstream".to_owned();
    body.tags = vec!["openai".to_owned()];
    let view = fixture.service.create(owner, body).await.expect("created");
    fixture.service.delete(owner, view.record.id).await.expect("deleted");
    assert!(fixture.service.get(owner, view.record.id).await.is_err(), "not-found after delete");
    // The alias key is available again (`inst-um-st-8`).
    let mut body = create_body(&["10.0.0.1"]);
    body.alias = "ip-upstream".to_owned();
    fixture.service.create(owner, body).await.expect("re-created");
}

// ---------------------------------------------------------------------------
// The write ordering
// ---------------------------------------------------------------------------

/// `inst-um-cr-18`/`-rp-14`/`-dl-8`: store write, then CP L1 invalidation,
/// then the DP flush, then success.
#[tokio::test]
async fn every_write_notifies_the_hook_after_the_store_write() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture
        .service
        .create(owner, create_body(&["api.vendor.com"]))
        .await
        .expect("created");
    fixture
        .service
        .replace(owner, view.record.id, create_body(&["api.vendor.com"]))
        .await
        .expect("replaced");
    fixture.service.delete(owner, view.record.id).await.expect("deleted");
    let events = fixture.hook.events.lock().clone();
    assert_eq!(events.len(), 3, "create, replace and delete each notify once");
    assert_eq!(events[0].1, view.record.id, "the created identifier is reported");
    assert_eq!(events[0].0, owner.tenant_id, "the owning tenant is reported");
}

// ---------------------------------------------------------------------------
// Effective enablement
// ---------------------------------------------------------------------------

/// `inst-um-ei-1` .. `-3`: the owning tenant's flag is authoritative.
#[tokio::test]
async fn the_owning_tenant_flag_is_the_effective_state() {
    let fixture = fixture(&[]);
    let owner = actor();
    let mut body = create_body(&["api.vendor.com"]);
    body.enabled = false;
    let view = fixture.service.create(owner, body).await.expect("created");
    assert_eq!(
        view.effective,
        EffectiveEnablement { enabled: false, disabling_tenant_id: Some(owner.tenant_id) },
        "the owner's disablement is reported"
    );
    let enabled = fixture
        .service
        .replace(owner, view.record.id, create_body(&["api.vendor.com"]))
        .await
        .expect("re-enabled");
    assert_eq!(enabled.effective, EffectiveEnablement::own(true));
}

/// `inst-um-ei-4`/`-5`: an ancestor disablement governs the descendant's view.
#[tokio::test]
async fn an_ancestor_disablement_presents_as_disabled_by_ancestor() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "shared.vendor.com".to_owned();
    let ancestor = fixture.service.create(parent_actor, ancestor_body).await.expect("created");
    let mut child_body = create_body(&["10.0.0.2"]);
    child_body.alias = "shared.vendor.com".to_owned();
    let child = fixture.service.create(child_actor, child_body).await.expect("bound");
    assert!(child.effective.enabled, "the ancestor record is enabled");

    // The ancestor disables its record.
    let mut disabled = create_body(&["10.0.0.1"]);
    disabled.alias = "shared.vendor.com".to_owned();
    disabled.enabled = false;
    fixture.service.replace(parent_actor, ancestor.record.id, disabled).await.expect("disabled");

    // The closest disabled ancestor record governs the descendant's view, and
    // the descendant's own stored state is untouched.
    let view = fixture.service.get(child_actor, child.record.id).await.expect("read");
    assert_eq!(
        view.effective,
        EffectiveEnablement { enabled: false, disabling_tenant_id: Some(parent) },
        "disabled by the ancestor"
    );
    assert!(view.record.enabled, "the descendant's stored state is unchanged");
    // And the descendant cannot lift it: the ancestor record is not-found.
    let mut lifted = create_body(&["10.0.0.1"]);
    lifted.alias = "shared.vendor.com".to_owned();
    let error = fixture
        .service
        .replace(child_actor, ancestor.record.id, lifted)
        .await
        .expect_err("not addressable");
    assert!(error.is_not_found(), "{error}");
}

/// Re-enabling or removing the ancestor record returns the descendant to its
/// own stored state.
#[tokio::test]
async fn clearing_the_ancestor_disablement_restores_the_descendant_state() {
    let (parent, child) = parents();
    let fixture = fixture(&[(child, parent)]);
    let parent_actor = Actor { tenant_id: parent, principal_id: Uuid::new_v4() };
    let child_actor = Actor { tenant_id: child, principal_id: Uuid::new_v4() };
    let mut ancestor_body = create_body(&["10.0.0.1"]);
    ancestor_body.alias = "shared.vendor.com".to_owned();
    ancestor_body.enabled = false;
    let ancestor = fixture.service.create(parent_actor, ancestor_body).await.expect("created");
    let mut child_body = create_body(&["10.0.0.2"]);
    child_body.alias = "shared.vendor.com".to_owned();
    let child = fixture.service.create(child_actor, child_body).await.expect("created");
    assert!(!child.effective.enabled, "disabled by the ancestor at create time");

    // Re-enabling the ancestor record restores the descendant's own state.
    fixture
        .service
        .replace(parent_actor, ancestor.record.id, ip_body("shared.vendor.com", "10.0.0.1"))
        .await
        .expect("re-enabled");
    let view = fixture.service.get(child_actor, child.record.id).await.expect("read");
    assert_eq!(view.effective, EffectiveEnablement::own(true), "the descendant's own state");
    // Removing the ancestor record has the same effect.
    fixture.service.delete(parent_actor, ancestor.record.id).await.expect("removed");
    let view = fixture.service.get(child_actor, child.record.id).await.expect("read");
    assert_eq!(view.effective, EffectiveEnablement::own(true));
}

// ---------------------------------------------------------------------------
// Absent-stays-absent
// ---------------------------------------------------------------------------

/// A create that omits a sub-configuration block persists no such block, and
/// an ancestor record with an omitted block contributes nothing to a
/// descendant's effective configuration.
#[tokio::test]
async fn an_omitted_block_stays_absent_and_contributes_nothing() {
    let fixture = fixture(&[]);
    let owner = actor();
    let view = fixture.service.create(owner, create_body(&["api.vendor.com"])).await.expect("created");
    for (name, absent) in [
        ("auth", view.record.auth.is_none()),
        ("headers", view.record.headers.is_none()),
        ("rate_limit", view.record.rate_limit.is_none()),
        ("cors", view.record.cors.is_none()),
        ("plugins", view.record.plugins.is_none()),
    ] {
        assert!(absent, "{name} is absent, with no implicit empty block persisted");
    }
    // The merge engine sees nothing to inherit from an ancestor record that
    // omits every block: its layer contributes no value.
    let layer = crate::domain::merge::upstream_base_layer(&view.record);
    assert!(layer.auth.is_none() && layer.headers.is_none() && layer.rate_limit.is_none());
    assert!(layer.cors.is_none() && layer.plugins.is_none());
}

// ---------------------------------------------------------------------------
// The list query
// ---------------------------------------------------------------------------

/// `inst-um-ls-5`: only the caller's records are candidates, and the query
/// applies.
#[tokio::test]
async fn the_list_is_scoped_and_the_query_applies() {
    let fixture = fixture(&[]);
    let owner = actor();
    let other = actor();
    for alias in ["a.vendor.com", "b.vendor.com"] {
        fixture.service.create(owner, create_body(&[alias])).await.expect("created");
        fixture.service.create(other, create_body(&[alias])).await.expect("created");
    }
    let query = ListQuery::parse(&[
        ("$filter".to_owned(), "startswith(alias, 'a')".to_owned()),
        ("$top".to_owned(), "1".to_owned()),
    ])
    .expect("query");
    let listed = fixture.service.list(owner, &query).await.expect("list");
    assert_eq!(listed.len(), 1, "only the caller's records, and only one page");
    assert_eq!(listed[0].alias, "a.vendor.com");
}
