//! The tenant chain walk.
//!
//! Covers `cpt-cf-oagw-dod-tenant-chain-walk` and
//! `cpt-cf-oagw-algo-tenant-chain-walk`: the per-element tenant-scoped alias
//! read, the ordered candidate set, the unavailable-chain failure, and the
//! adapter that turns the platform tenant-resolver's answer into a chain.
//! Retired tenants are dropped by the adapter before the chain is ordered, so
//! no walk ever reads a retired tenant's rows.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use async_trait::async_trait;
use oagw::control_plane::chain::{UnavailableChain, cache_key, chain_of, modes_of, walk_candidates};
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::domain::effective::Family;
use oagw::domain::upstream::SharingMode;
use oagw::store::OagwStore;
use oagw::{Alias, OagwConfig};
use serde_json::{Value, json};
use tenant_resolver_sdk::{
    GetAncestorsOptions, GetAncestorsResponse, TenantId, TenantInfo, TenantRef,
    TenantResolverClient, TenantResolverError, TenantStatus,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// A management service over its own empty store and cache.
fn service() -> ManagementService {
    service_with_store().0
}

/// The same service, with the store handle the walk reads through.
fn service_with_store() -> (ManagementService, Arc<OagwStore>) {
    let store = Arc::new(OagwStore::new());
    let service = ManagementService::new(
        Arc::clone(&store),
        &OagwConfig::default(),
        Arc::new(ControlPlaneCache::new()),
    )
    .expect("the validators compile");
    (service, store)
}

/// A valid upstream body whose sharing-bearing families declare one mode each.
///
/// `rate_limit.sustained` is required by the shipped schema whenever the
/// family is present, so the fixture always carries a full limit.
fn upstream_body(alias: Option<&str>, sharing: Option<&str>) -> Value {
    let mut body = json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }]
        },
        "protocol": HTTP_PROTOCOL,
        "tags": ["llm"],
        "plugins": { "sharing": "enforce" },
        "cors": {
            "sharing": "inherit",
            "enabled": true,
            "allowed_origins": ["https://console.example.com"]
        },
        "rate_limit": {
            "sharing": sharing.unwrap_or("private"),
            "algorithm": "token_bucket",
            "sustained": { "rate": 100, "window": "second" },
            "burst": { "capacity": 200 },
            "scope": "tenant",
            "strategy": "reject",
            "cost": 1
        }
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    if let Some(sharing) = sharing {
        body["auth"] = json!({ "sharing": sharing });
    }
    body
}

/// A route body addressing one upstream.
fn route_body(upstream_id: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": 1,
        "tags": ["edge"]
    })
}

/// A tenant-resolver that answers the ancestor list it was built with.
struct StaticResolver {
    chain: Vec<(Uuid, TenantStatus)>,
}

#[async_trait]
impl TenantResolverClient for StaticResolver {
    async fn get_tenant(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::TenantNotFound {
            tenant_id: TenantId::nil(),
        })
    }

    async fn get_root_tenant(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<TenantInfo, TenantResolverError> {
        Err(TenantResolverError::TenantNotFound {
            tenant_id: TenantId::nil(),
        })
    }

    async fn get_tenants(
        &self,
        _ctx: &SecurityContext,
        _ids: &[TenantId],
        _options: &tenant_resolver_sdk::GetTenantsOptions,
    ) -> Result<Vec<TenantInfo>, TenantResolverError> {
        Ok(Vec::new())
    }

    async fn get_ancestors(
        &self,
        _ctx: &SecurityContext,
        id: TenantId,
        _options: &GetAncestorsOptions,
    ) -> Result<GetAncestorsResponse, TenantResolverError> {
        let Some((_, status)) = self.chain.first().copied() else {
            return Err(TenantResolverError::TenantNotFound { tenant_id: id });
        };
        let rest = &self.chain[1..];
        Ok(GetAncestorsResponse {
            tenant: TenantRef {
                id,
                status,
                tenant_type: None,
                parent_id: rest.first().map(|(ancestor, _)| TenantId(*ancestor)),
                self_managed: false,
            },
            ancestors: rest
                .iter()
                .map(|(id, status)| TenantRef {
                    id: TenantId(*id),
                    status: *status,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                })
                .collect(),
        })
    }

    async fn get_descendants(
        &self,
        _ctx: &SecurityContext,
        _id: TenantId,
        _options: &tenant_resolver_sdk::GetDescendantsOptions,
    ) -> Result<tenant_resolver_sdk::GetDescendantsResponse, TenantResolverError> {
        Err(TenantResolverError::TenantNotFound {
            tenant_id: TenantId::nil(),
        })
    }

    async fn is_ancestor(
        &self,
        _ctx: &SecurityContext,
        _ancestor: TenantId,
        _descendant: TenantId,
        _options: &tenant_resolver_sdk::IsAncestorOptions,
    ) -> Result<bool, TenantResolverError> {
        Ok(false)
    }
}

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(tenant(0xa001))
        .subject_tenant_id(tenant(0xa001))
        .build()
        .expect("the subject is complete")
}

#[test]
fn the_alias_read_matches_the_normalized_form_and_stays_tenant_scoped() {
    let (management, store) = service_with_store();
    let owner = tenant(0xa001);
    let other = tenant(0xa009);
    let row = management
        .create_upstream(owner, &upstream_body(Some("api.openai.com"), None))
        .expect("the create succeeds");
    management
        .create_upstream(other, &upstream_body(Some("api.openai.com"), None))
        .expect("another tenant may hold the same alias");

    let bare = Alias::parse("api.openai.com").expect("a valid alias");
    assert_eq!(store.upstream_by_alias(owner, &bare), Some(row));
    assert_eq!(
        store.upstream_by_alias(other, &bare).map(|row| row.tenant_id),
        Some(other)
    );

    let dotted = Alias::parse("API.OpenAI.com.").expect("a valid alias");
    assert!(
        store.upstream_by_alias(owner, &dotted).is_some(),
        "case and a trailing dot are not identity"
    );
    let ported = Alias::parse("api.openai.com:8443").expect("a valid alias");
    assert!(
        store.upstream_by_alias(owner, &ported).is_none(),
        "the port participates in identity"
    );
}

#[test]
fn routes_of_upstream_reads_only_the_calling_tenant_rows_of_that_upstream() {
    let (management, store) = service_with_store();
    let owner = tenant(0xa001);
    let other = tenant(0xa009);
    let mine = management
        .create_upstream(owner, &upstream_body(Some("api.openai.com"), None))
        .expect("the create succeeds");
    let theirs = management
        .create_upstream(other, &upstream_body(Some("api.openai.com"), None))
        .expect("the create succeeds");
    let route = route_body(mine.upstream.id, "/v1/chat");
    management.create_route(owner, &route).expect("the route create succeeds");
    let mut foreign = route.clone();
    foreign["upstream_id"] = json!(theirs.upstream.id);
    management.create_route(other, &foreign).expect("the route create succeeds");

    let rows = store.routes_of_upstream(owner, mine.upstream.id);
    assert_eq!(rows.len(), 1, "the other tenant's route is never a candidate");
    assert_eq!(rows[0].route.upstream_id, mine.upstream.id);
    assert!(store.routes_of_upstream(other, mine.upstream.id).is_empty());
}

#[test]
fn the_walk_orders_candidates_from_the_calling_tenant_to_the_root() {
    let (management, store) = service_with_store();
    let leaf = tenant(0xa001);
    let mid = tenant(0xa002);
    let root = tenant(0xa003);
    let leaf_row = management
        .create_upstream(leaf, &upstream_body(Some("api.openai.com"), Some("inherit")))
        .expect("the create succeeds");
    let root_row = management
        .create_upstream(root, &upstream_body(Some("api.openai.com"), Some("enforce")))
        .expect("the create succeeds");

    let candidates = walk_candidates(
        &store,
        leaf,
        &[mid, root],
        &Alias::parse("api.openai.com").expect("a valid alias"),
    )
    .expect("an ordered chain is available");

    assert_eq!(candidates.len(), 2, "the middle tenant holds no such alias");
    assert_eq!(candidates[0].depth, 0);
    assert_eq!(candidates[0].tenant_id, leaf);
    assert_eq!(candidates[0].upstream_id, leaf_row.upstream.id);
    assert_eq!(candidates[0].modes.mode_of(Family::Auth), SharingMode::Inherit);
    assert_eq!(candidates[1].depth, 2);
    assert_eq!(candidates[1].tenant_id, root);
    assert_eq!(candidates[1].upstream_id, root_row.upstream.id);
    assert_eq!(candidates[1].modes.mode_of(Family::Auth), SharingMode::Enforce);
    assert_eq!(
        candidates[1].modes.mode_of(Family::Plugins),
        SharingMode::Enforce,
        "the plugins family is read from its own sharing member"
    );
}

#[test]
fn the_walk_answers_an_empty_set_when_no_chain_element_holds_the_alias() {
    let (management, store) = service_with_store();
    management
        .create_upstream(tenant(0xa002), &upstream_body(Some("api.openai.com"), None))
        .expect("the create succeeds");
    let candidates = walk_candidates(
        &store,
        tenant(0xa001),
        &[tenant(0xa002)],
        &Alias::parse("other.example.com").expect("a valid alias"),
    )
    .expect("an ordered chain is available");
    assert!(candidates.is_empty(), "no element holds the alias");
}

#[test]
fn the_walk_fails_closed_on_an_unavailable_chain() {
    let (_management, store) = service_with_store();
    let unavailable = walk_candidates(
        &store,
        tenant(0xa001),
        &[tenant(0xa002), tenant(0xa001)],
        &Alias::parse("api.openai.com").expect("a valid alias"),
    );
    assert_eq!(unavailable, Err(UnavailableChain::Unordered));
}

#[test]
fn the_cache_key_carries_the_tenant_and_the_normalized_alias() {
    let alias = Alias::parse("API.OpenAI.COM.").expect("a valid alias");
    assert_eq!(
        cache_key(tenant(0xa001), &alias),
        "upstream:00000000-0000-0000-0000-00000000a001:api.openai.com"
    );
}

#[test]
fn modes_of_takes_the_schema_default_for_a_family_the_row_omits() {
    let mut body = upstream_body(Some("api.openai.com"), Some("inherit"));
    let object = body.as_object_mut().expect("the body is an object");
    object.remove("auth");
    object.remove("cors");
    let row = service()
        .create_upstream(tenant(0xa001), &body)
        .expect("the create succeeds");
    let modes = modes_of(&row.upstream);
    assert_eq!(modes.mode_of(Family::Auth), SharingMode::Private);
    assert_eq!(modes.mode_of(Family::Cors), SharingMode::Private);
    assert_eq!(modes.mode_of(Family::RateLimit), SharingMode::Inherit);
    assert_eq!(modes.mode_of(Family::Plugins), SharingMode::Enforce);
}

#[tokio::test]
async fn the_adapter_drops_the_tenants_the_resolver_retired() {
    let resolver = Arc::new(StaticResolver {
        chain: vec![
            (tenant(0xa001), TenantStatus::Active),
            (tenant(0xa002), TenantStatus::Active),
            (tenant(0xa004), TenantStatus::Deleted),
            (tenant(0xa003), TenantStatus::Active),
        ],
    });
    let client: Option<Arc<dyn TenantResolverClient>> = Some(resolver);
    let chain = chain_of(client.as_ref(), &context(), tenant(0xa001))
        .await
        .expect("an ordered chain is available");
    assert_eq!(
        chain.tenants(),
        &[tenant(0xa001), tenant(0xa002), tenant(0xa003)]
    );
    assert!(
        !chain.contains(tenant(0xa004)),
        "a retired tenant is never a participant"
    );
}

#[tokio::test]
async fn the_adapter_fails_closed_when_the_resolver_is_absent_or_refuses() {
    let absent: Option<Arc<dyn TenantResolverClient>> = None;
    assert!(chain_of(absent.as_ref(), &context(), tenant(0xa001)).await.is_none());

    let refusing = Arc::new(StaticResolver { chain: Vec::new() });
    let client: Option<Arc<dyn TenantResolverClient>> = Some(refusing);
    assert!(chain_of(client.as_ref(), &context(), tenant(0xa001)).await.is_none());
}
