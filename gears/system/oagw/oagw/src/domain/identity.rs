//! Resource identifier resolution (`cpt-cf-oagw-algo-resource-identity`).
//!
//! The management paths address a record by an anonymous GTS identifier of the
//! resource type or by its bare UUID; both carry the same instance part. The
//! resolution is the place where the tenant scope is enforced on every read and
//! write path: a lookup never leaves the calling tenant's key space, so a
//! record of another tenant — an ancestor's in particular — is
//! indistinguishable from a missing one (`cpt-cf-oagw-dod-tenant-scoping`).

use std::fmt;
use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::ManagementError;
use crate::domain::model::{Route, Upstream};
use crate::domain::repo::{RouteRepository, UpstreamRepository};

// @cpt-begin:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full
/// The GTS identifier prefix of an upstream resource.
pub const UPSTREAM_ID_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";

/// The GTS identifier prefix of a route resource.
pub const ROUTE_ID_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// The resource type a management path addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// `/oagw/v1/upstreams`.
    Upstream,
    /// `/oagw/v1/routes`.
    Route,
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl ResourceKind {
    /// The GTS prefix the resource type's identifiers carry.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Upstream => UPSTREAM_ID_PREFIX,
            Self::Route => ROUTE_ID_PREFIX,
        }
    }

    /// The name used in problem details.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full

/// Extract the instance part of a path identifier.
///
/// # Errors
///
/// Returns a mapped `400` when the identifier is neither a UUID nor an
/// anonymous GTS identifier of the expected type
/// (`cpt-cf-oagw-algo-resource-identity`).
pub fn parse_identifier(raw: &str, kind: ResourceKind) -> Result<Uuid, ManagementError> {
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-03
    let raw = raw.trim();
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-02
    // A bare UUID is accepted; the endpoint path already carries the type.
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-02

    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-01
    // The anonymous GTS identifier form `gts.cf.core.oagw.<type>.v1~{uuid}`:
    // the instance part is the record's `id`. A prefix that does not match the
    // endpoint's resource type is a `400`, not a `404`.
    let Some(instance) = raw.strip_prefix(kind.prefix()) else {
        return Err(malformed(raw, kind));
    };
    let instance = instance.trim();
    if instance.is_empty() {
        return Err(malformed(raw, kind));
    }
    Uuid::parse_str(instance).map_err(|_| malformed(raw, kind))
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-01
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-03
}

/// Resolve a path identifier to the upstream of the calling tenant.
///
/// # Errors
///
/// Returns a mapped `400` when the identifier does not parse and a mapped `404`
/// when no upstream of the calling tenant carries it — including one owned by
/// an ancestor tenant, which is never addressable here.
pub fn resolve_upstream<U: UpstreamRepository + ?Sized>(
    store: &U,
    tenant_id: Uuid,
    raw: &str,
) -> Result<Arc<Upstream>, ManagementError> {
    let id = parse_identifier(raw, ResourceKind::Upstream)?;
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-05
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-04
    // The lookup is keyed by `(tenant_id, id)`: an ancestor's record is not in
    // this key space and resolves as missing.
    let found = store.find_upstream(tenant_id, id);
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-04

    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-06
    found.ok_or_else(|| not_found(ResourceKind::Upstream))
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-06
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-05
}

/// Resolve a path identifier to the route of the calling tenant.
///
/// # Errors
///
/// Same outcomes as [`resolve_upstream`], for the route collection.
pub fn resolve_route<R: RouteRepository + ?Sized>(
    store: &R,
    tenant_id: Uuid,
    raw: &str,
) -> Result<Arc<Route>, ManagementError> {
    let id = parse_identifier(raw, ResourceKind::Route)?;
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-07
    // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-08
    // The resolved record keeps its `id`, `tenant_id` and `alias` intact, so a
    // subsequent write cannot reshape the identity it was addressed by.
    store
        .find_route(tenant_id, id)
        .ok_or_else(|| not_found(ResourceKind::Route))
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-08
    // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-07
}

/// The `400` of an identifier that does not fit the resource type.
fn malformed(raw: &str, kind: ResourceKind) -> ManagementError {
    ManagementError::validation(format!(
        "{kind}: `{raw}` is not an identifier of the form `{}{{uuid}}`",
        kind.prefix()
    ))
}

/// The `404` of an identifier the calling tenant does not hold.
fn not_found(kind: ResourceKind) -> ManagementError {
    ManagementError::not_found(format!("no {} of this tenant carries this identifier", kind.name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchRule, MatchType, Protocol, Scheme, ServerConfig, Timestamp,
    };
    use crate::infra::storage::OagwStore;

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000004aa");
    const ANCESTOR: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000400");

    fn upstream(tenant_id: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            enabled: true,
            alias: alias.to_string(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.vendor.com".to_string(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn route(upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: TENANT,
            upstream_id,
            enabled: true,
            matches: MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_string()],
                    path: "/v1".to_string(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::SuffixMode::Append,
                }),
                grpc: None,
            },
            match_type: MatchType::Http,
            priority: 0,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn the_anonymous_gts_identifier_yields_the_instance_part() {
        let id = Uuid::new_v4();
        let identifier = format!("{UPSTREAM_ID_PREFIX}{id}");
        assert_eq!(
            parse_identifier(&identifier, ResourceKind::Upstream).expect("parsed"),
            id
        );
        let identifier = format!("{ROUTE_ID_PREFIX}{id}");
        assert_eq!(
            parse_identifier(&identifier, ResourceKind::Route).expect("parsed"),
            id
        );
    }

    #[test]
    fn a_bare_uuid_is_accepted_with_the_type_from_the_path() {
        let id = Uuid::new_v4();
        assert_eq!(
            parse_identifier(&id.to_string(), ResourceKind::Upstream).expect("parsed"),
            id
        );
        assert_eq!(
            parse_identifier(&id.to_string(), ResourceKind::Route).expect("parsed"),
            id
        );
    }

    #[test]
    fn a_mismatched_or_malformed_prefix_is_a_400() {
        // A route identifier does not address an upstream.
        let id = Uuid::new_v4();
        let identifier = format!("{ROUTE_ID_PREFIX}{id}");
        let error = parse_identifier(&identifier, ResourceKind::Upstream)
            .expect_err("the prefix does not match");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("not an identifier"), "{}", error.detail());

        for raw in ["", "   ", UPSTREAM_ID_PREFIX, "not-an-identifier"] {
            let error = parse_identifier(raw, ResourceKind::Upstream)
                .expect_err("malformed identifier");
            assert_eq!(error.status(), 400, "{raw}");
        }
    }

    #[test]
    fn a_record_of_another_tenant_resolves_as_missing() {
        let store = OagwStore::new();
        let own = store
            .insert_upstream(upstream(TENANT, "api.vendor.com"))
            .expect("inserted");
        let foreign = store
            .insert_upstream(upstream(ANCESTOR, "other.example.com"))
            .expect("inserted");

        let resolved = resolve_upstream(&store, TENANT, &own.id.to_string()).expect("resolved");
        assert_eq!(resolved.id, own.id);
        assert_eq!(resolved.tenant_id, TENANT);
        assert_eq!(resolved.alias, "api.vendor.com");

        for raw in [
            foreign.id.to_string(),
            format!("{UPSTREAM_ID_PREFIX}{}", foreign.id),
        ] {
            let error = resolve_upstream(&store, TENANT, &raw).expect_err("another tenant's");
            assert_eq!(error.status(), 404, "{raw}");
            assert!(
                !error.detail().contains(&ANCESTOR.to_string()),
                "the detail never names the foreign tenant"
            );
        }
    }

    #[test]
    fn the_gts_form_resolves_the_same_record_as_the_bare_uuid() {
        let store = OagwStore::new();
        let record = store
            .insert_upstream(upstream(TENANT, "api.vendor.com"))
            .expect("inserted");
        let bare = resolve_upstream(&store, TENANT, &record.id.to_string()).expect("resolved");
        let gts = resolve_upstream(
            &store,
            TENANT,
            &format!("{UPSTREAM_ID_PREFIX}{}", record.id),
        )
        .expect("resolved");
        assert_eq!(bare, gts);
    }

    #[test]
    fn a_route_is_resolved_in_its_own_collection() {
        let store = OagwStore::new();
        let upstream_id = store
            .insert_upstream(upstream(TENANT, "api.vendor.com"))
            .expect("inserted")
            .id;
        let record = store
            .insert_route(route(upstream_id))
            .expect("inserted");

        let resolved =
            resolve_route(&store, TENANT, &format!("{ROUTE_ID_PREFIX}{}", record.id)).expect("resolved");
        assert_eq!(resolved.id, record.id);
        assert_eq!(resolved.tenant_id, TENANT);

        // The route identifier does not address an upstream and vice versa.
        let error = resolve_upstream(
            &store,
            TENANT,
            &format!("{ROUTE_ID_PREFIX}{}", record.id),
        )
        .expect_err("the prefixes differ");
        assert_eq!(error.status(), 400);

        let error = resolve_route(&store, TENANT, &Uuid::new_v4().to_string())
            .expect_err("unknown route");
        assert_eq!(error.status(), 404);
    }
}
