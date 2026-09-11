//! The effective configuration merge.
//!
//! `cpt-cf-oagw-algo-config-merge` folds the declarations the walk produced —
//! the selected upstream, the matched route and the ancestor chain the walk
//! consulted, including the records the selection shadowed — into the one
//! [`EffectiveUpstream`] the selection, validation, transformation and call
//! stages read.
//!
//! The precedence is upstream < route < tenant, and the sharing modes the
//! write side recorded decide which overrides survive:
//!
//! * `enforce` pins the ancestor value, so a descendant cannot lift it;
//! * `inherit` accepts the descendant override, and unions CORS origins;
//! * `private` contributes only at its owning level, so an ancestor's private
//!   declaration is not inherited at all.
//!
//! The merge produces no credential material: the auth declaration is carried
//! exactly as stored, as `cred://` references with their sharing mode, and its
//! resolution is the entry-2.5 auth hook's business.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginBinding, RateLimitConfig, RateWindow, Route,
    Sharing, Upstream,
};
use crate::infra::storage::ConfigSnapshot;

/// A declaration one layer contributes, with the sharing mode each field was
/// written with.
#[derive(Debug, Clone)]
struct Layer {
    owner: Uuid,
    headers: Option<HeadersConfig>,
    cors: Option<CorsConfig>,
    rate_limit: Option<RateLimitConfig>,
    plugins: Option<Vec<PluginBinding>>,
    auth: Option<AuthConfig>,
    /// Sharing mode of the CORS declaration.
    cors_sharing: Sharing,
    /// Sharing mode of the rate limit.
    rate_limit_sharing: Sharing,
    /// Sharing mode of the plugin pipeline.
    plugins_sharing: Sharing,
    /// Sharing mode of the auth declaration.
    auth_sharing: Sharing,
}

/// The header manipulation carries no sharing mode of its own in the schema, so
/// it always inherits.
const HEADER_SHARING: Sharing = Sharing::Inherit;

impl Layer {
    /// The layer an upstream record contributes.
    fn of_upstream(upstream: &Upstream) -> Self {
        Self {
            owner: upstream.tenant_id,
            headers: upstream.headers.clone(),
            cors: upstream.cors.clone(),
            rate_limit: upstream.rate_limit.clone(),
            plugins: upstream.plugins.as_ref().map(|plugins| plugins.items.clone()),
            auth: upstream.auth.clone(),
            cors_sharing: upstream.cors.as_ref().map_or(HEADER_SHARING, |c| c.sharing),
            rate_limit_sharing: upstream
                .rate_limit
                .as_ref()
                .map_or(HEADER_SHARING, |limit| limit.sharing),
            plugins_sharing: upstream
                .plugins
                .as_ref()
                .map_or(HEADER_SHARING, |plugins| plugins.sharing),
            auth_sharing: upstream
                .auth
                .as_ref()
                .map_or(HEADER_SHARING, |auth| auth.sharing),
        }
    }

    /// The layer a route record contributes.
    fn of_route(route: &Route) -> Self {
        Self {
            owner: route.tenant_id,
            headers: None,
            cors: route.cors.clone(),
            rate_limit: route.rate_limit.clone(),
            plugins: route.plugins.as_ref().map(|plugins| plugins.items.clone()),
            auth: None,
            cors_sharing: route.cors.as_ref().map_or(HEADER_SHARING, |c| c.sharing),
            rate_limit_sharing: route
                .rate_limit
                .as_ref()
                .map_or(HEADER_SHARING, |limit| limit.sharing),
            plugins_sharing: route
                .plugins
                .as_ref()
                .map_or(HEADER_SHARING, |plugins| plugins.sharing),
            auth_sharing: HEADER_SHARING,
        }
    }

    /// Whether a field declared with `sharing` at this layer inherits into a
    /// request resolved at `owner`.
    ///
    /// A `private` declaration contributes only at its owning level: an
    /// ancestor's private CORS, rate-limit, plugin or auth rule is not
    /// inherited by a descendant request at all.
    fn inherits(self_sharing: Sharing, layer_owner: Uuid, owner: Uuid) -> bool {
        self_sharing != Sharing::Private || layer_owner == owner
    }
}

/// An ancestor bound a field with `enforce`, so no descendant can lift it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcedField {
    /// The ancestor pinned the header rules.
    Headers,
    /// The ancestor pinned the CORS configuration.
    Cors,
    /// The ancestor pinned the rate-limit bound.
    RateLimit,
    /// The ancestor pinned the plugin pipeline.
    Plugins,
    /// The ancestor pinned the auth declaration.
    Auth,
}

impl EnforcedField {
    /// The name the field carries in the schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Headers => "headers",
            Self::Cors => "cors",
            Self::RateLimit => "rate_limit",
            Self::Plugins => "plugins",
            Self::Auth => "auth",
        }
    }
}

/// One enforced ancestor constraint collected across the walked chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcedConstraint {
    /// Tenant that declared the constraint.
    pub owner: Uuid,
    /// The pinned field.
    pub field: EnforcedField,
}

/// The effective configuration of one proxy request.
///
/// Every member is a routing or policy fact. No member carries a resolved
/// secret: the auth declaration is the stored `cred://` reference set.
#[derive(Debug, Clone, Default)]
pub struct EffectiveUpstream {
    /// Merged header manipulation, upstream < route < tenant.
    pub headers: HeadersConfig,
    /// Merged CORS configuration, `None` when no layer enables it.
    pub cors: Option<CorsConfig>,
    /// The strictest rate limit across the layers, `None` when none declares
    /// one. Entry 2.5 executes the bucket this bound describes.
    pub rate_limit: Option<RateLimitConfig>,
    /// The plugin bindings in reference order: ancestor chain, then upstream,
    /// then route.
    pub plugins: Vec<PluginBinding>,
    /// The auth declaration as stored, `cred://` references only.
    pub auth: Option<AuthConfig>,
    /// The enforced ancestor constraints, which shadowing cannot lift.
    pub enforced: Vec<EnforcedConstraint>,
    /// The add-only union of the layers' tags, for the discovery record.
    pub tags: Vec<String>,
}

impl EffectiveUpstream {
    /// Whether CORS handling is enabled for the request.
    #[must_use]
    pub const fn cors_enabled(&self) -> bool {
        match &self.cors {
            Some(cors) => cors.enabled,
            None => false,
        }
    }

    /// Whether `origin` is an allowed origin.
    ///
    /// `*` admits any origin, per the CORS configuration's own semantics.
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        match &self.cors {
            Some(cors) if cors.enabled => cors
                .allowed_origins
                .iter()
                .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin)),
            _ => false,
        }
    }

    /// Whether `method` is an allowed CORS method.
    #[must_use]
    pub fn method_allowed(&self, method: &str) -> bool {
        match &self.cors {
            Some(cors) if cors.enabled => cors
                .allowed_methods
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(method)),
            _ => false,
        }
    }

    /// The headers the browser may read, from `expose_headers`.
    #[must_use]
    pub fn exposed_headers(&self) -> &[String] {
        match &self.cors {
            Some(cors) => &cors.expose_headers,
            None => &[],
        }
    }

    /// Whether the CORS configuration allows credentials.
    #[must_use]
    pub const fn allow_credentials(&self) -> bool {
        match &self.cors {
            Some(cors) => cors.allow_credentials,
            None => false,
        }
    }
}

/// The requests-per-day equivalent of a rate limit, the comparison key that
/// makes the strictest bound win across window units.
fn rate_per_day(limit: &RateLimitConfig) -> u128 {
    let seconds = match limit.sustained.window {
        RateWindow::Second => 1_u128,
        RateWindow::Minute => 60,
        RateWindow::Hour => 60 * 60,
        RateWindow::Day => 24 * 60 * 60,
    };
    u128::from(limit.sustained.rate) * (24 * 60 * 60) / seconds
}

/// The strictest of two rate limits.
fn stricter(left: &RateLimitConfig, right: &RateLimitConfig) -> RateLimitConfig {
    if rate_per_day(right) < rate_per_day(left) {
        right.clone()
    } else {
        left.clone()
    }
}

/// Merge the effective configuration for one request.
///
/// `walk_tenants` is the walked chain, caller first, and `shadowed` holds the
/// ancestor upstream records the selection shadowed — their `enforce`
/// declarations still bind, which is what makes shadowing unable to lift an
/// enforced bound.
#[must_use]
// @cpt-begin:cpt-cf-oagw-dod-config-merge:p1:inst-full
pub fn merge(
    snapshot: &ConfigSnapshot,
    upstream: &Upstream,
    route: &Route,
    walk_tenants: &[Uuid],
    shadowed: &[Arc<Upstream>],
    tags: &[String],
) -> EffectiveUpstream {
    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-01
    // The upstream configuration is the base layer; every later layer only
    // overrides what its sharing mode allows it to override.
    let owner = upstream.tenant_id;
    let base = Layer::of_upstream(upstream);
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-01

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-02
    // The route's overrides apply, then the tenant-level ones: the ancestor
    // declarations of the walked chain, root first, so the nearest tenant wins
    // an `inherit` contest and an `enforce` ancestor pins the field.
    let route_layer = Layer::of_route(route);
    let mut ancestor_layers: Vec<Layer> = Vec::new();
    for tenant in walk_tenants.iter().rev() {
        for record in snapshot
            .upstreams_of(*tenant)
            .iter()
            .filter(|record| record.alias == upstream.alias && record.id != upstream.id)
        {
            ancestor_layers.push(Layer::of_upstream(record));
        }
    }
    for record in shadowed {
        if record.id != upstream.id {
            ancestor_layers.push(Layer::of_upstream(record));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-02

    let mut effective = EffectiveUpstream::default();
    let mut enforced: Vec<EnforcedConstraint> = Vec::new();

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-03
    // The sharing mode recorded on write decides each field: `enforce` keeps
    // the ancestor value, `inherit` accepts the descendant override and
    // `private` contributes only at its owning level.
    for layer in &ancestor_layers {
        if Layer::inherits(layer.cors_sharing, layer.owner, owner)
            && layer.cors_sharing == Sharing::Enforce
            && layer.cors.is_some()
        {
            enforced.push(EnforcedConstraint {
                owner: layer.owner,
                field: EnforcedField::Cors,
            });
        }
        if Layer::inherits(layer.rate_limit_sharing, layer.owner, owner)
            && layer.rate_limit_sharing == Sharing::Enforce
            && layer.rate_limit.is_some()
        {
            enforced.push(EnforcedConstraint {
                owner: layer.owner,
                field: EnforcedField::RateLimit,
            });
        }
        if Layer::inherits(layer.plugins_sharing, layer.owner, owner)
            && layer.plugins_sharing == Sharing::Enforce
            && layer.plugins.is_some()
        {
            enforced.push(EnforcedConstraint {
                owner: layer.owner,
                field: EnforcedField::Plugins,
            });
        }
        if Layer::inherits(layer.auth_sharing, layer.owner, owner)
            && layer.auth_sharing == Sharing::Enforce
            && layer.auth.is_some()
        {
            enforced.push(EnforcedConstraint {
                owner: layer.owner,
                field: EnforcedField::Auth,
            });
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-03

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-05
    // CORS unions the origin sets under `inherit` and keeps the ancestor set
    // under `enforce`; the nearest enabled declaration decides the rest of the
    // configuration.
    effective.cors = merge_cors(&ancestor_layers, &base, &route_layer, owner);
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-05

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-04
    // The strictest bound wins across every enforced ancestor, the route and
    // the upstream, so a descendant cannot widen an ancestor's limit.
    effective.rate_limit = merge_rate_limit(&ancestor_layers, &base, &route_layer, owner);
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-04

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-06
    // The binding list is a concatenation, not a contest: the ancestor chain's
    // bindings first, then the upstream's, then the route's. What executes
    // against the list is entry 2.5's behaviour.
    let mut bindings: Vec<PluginBinding> = Vec::new();
    for layer in ancestor_layers.iter().filter(|l| {
        Layer::inherits(l.plugins_sharing, l.owner, owner)
    }) {
        bindings.extend(layer.plugins.iter().flatten().cloned());
    }
    bindings.extend(base.plugins.iter().flatten().cloned());
    bindings.extend(route_layer.plugins.iter().flatten().cloned());
    effective.plugins = bindings;
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-06

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-07
    effective.enforced = enforced;
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-07

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-08
    // The auth declaration travels exactly as stored: a `cred://` reference
    // with its sharing mode. No credential is resolved here, and none is placed
    // on the effective configuration or the request context.
    effective.auth = merge_auth(&ancestor_layers, &base, owner);
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-08

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-09
    // Tags are an add-only union: they label the discovery record and play no
    // part in matching or forwarding.
    let mut union: Vec<String> = tags.to_vec();
    for layer in &ancestor_layers {
        for tag in snapshot_tags(snapshot, layer.owner, &upstream.alias) {
            if !union.contains(&tag) {
                union.push(tag);
            }
        }
    }
    for tag in &upstream.tags {
        if !union.contains(tag) {
            union.push(tag.clone());
        }
    }
    for tag in &route.tags {
        if !union.contains(tag) {
            union.push(tag.clone());
        }
    }
    effective.tags = union;
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-09

    effective.headers = merge_headers(&ancestor_layers, &base, &route_layer);

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-10
    // The merged value is a configuration, not a credential: a `Debug` render
    // of it names references and bounds only.
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-10

    // @cpt-begin:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-11
    effective
    // @cpt-end:cpt-cf-oagw-algo-config-merge:p1:inst-pe-cm-11
}
// @cpt-end:cpt-cf-oagw-dod-config-merge:p1:inst-full

/// The tags of the ancestor record the chain holds for `alias`.
fn snapshot_tags(snapshot: &ConfigSnapshot, tenant: Uuid, alias: &str) -> Vec<String> {
    snapshot
        .upstreams_of(tenant)
        .into_iter()
        .filter(|record| record.alias == alias)
        .flat_map(|record| record.tags.clone())
        .collect()
}

/// Merge the header manipulation of the layers.
fn merge_headers(ancestors: &[Layer], base: &Layer, route: &Layer) -> HeadersConfig {
    // The header manipulation declares no sharing mode, so every ancestor's
    // rules are inherited and the descendant overrides them: the nearest
    // declaration that names a rule keeps it.
    let mut merged = HeadersConfig::default();
    for layer in ancestors {
        if let Some(headers) = &layer.headers {
            merged = headers.clone();
        }
    }
    if let Some(headers) = &base.headers {
        merged = headers.clone();
    }
    if let Some(headers) = &route.headers {
        merged = headers.clone();
    }
    merged
}

/// Merge the CORS declarations of the layers.
fn merge_cors(
    ancestors: &[Layer],
    base: &Layer,
    route: &Layer,
    owner: Uuid,
) -> Option<CorsConfig> {
    let mut merged: Option<CorsConfig> = None;
    let mut pinned = false;
    for layer in ancestors
        .iter()
        .filter(|l| Layer::inherits(l.cors_sharing, l.owner, owner))
    {
        if pinned {
            break;
        }
        if let Some(cors) = &layer.cors {
            merged = Some(match &merged {
                Some(previous) if layer.cors_sharing == Sharing::Inherit => {
                    // `inherit` unions the origin sets the two levels allow.
                    let mut union = previous.allowed_origins.clone();
                    for origin in &cors.allowed_origins {
                        if !union.contains(origin) {
                            union.push(origin.clone());
                        }
                    }
                    let mut inherited = previous.clone();
                    inherited.allowed_origins = union;
                    inherited
                }
                _ => cors.clone(),
            });
            pinned = layer.cors_sharing == Sharing::Enforce;
        }
    }
    if pinned {
        return merged;
    }
    if let Some(cors) = &base.cors {
        merged = Some(match merged {
            Some(previous) => {
                let mut union = previous.allowed_origins.clone();
                for origin in &cors.allowed_origins {
                    if !union.contains(origin) {
                        union.push(origin.clone());
                    }
                }
                let mut inherited = previous;
                inherited.allowed_origins = union;
                inherited
            }
            None => cors.clone(),
        });
    }
    if let Some(cors) = &route.cors {
        merged = Some(match merged {
            Some(previous) => {
                let mut union = previous.allowed_origins.clone();
                for origin in &cors.allowed_origins {
                    if !union.contains(origin) {
                        union.push(origin.clone());
                    }
                }
                let mut inherited = previous;
                inherited.allowed_origins = union;
                inherited
            }
            None => cors.clone(),
        });
    }
    merged
}

/// Merge the rate limits of the layers into the strictest bound.
fn merge_rate_limit(
    ancestors: &[Layer],
    base: &Layer,
    route: &Layer,
    owner: Uuid,
) -> Option<RateLimitConfig> {
    let mut strictest: Option<RateLimitConfig> = None;
    for layer in ancestors
        .iter()
        .filter(|l| Layer::inherits(l.rate_limit_sharing, l.owner, owner))
    {
        if let Some(limit) = &layer.rate_limit {
            strictest = Some(match strictest {
                Some(current) => stricter(&current, limit),
                None => limit.clone(),
            });
        }
    }
    if let Some(limit) = &base.rate_limit {
        strictest = Some(match strictest {
            Some(current) => stricter(&current, limit),
            None => limit.clone(),
        });
    }
    if let Some(limit) = &route.rate_limit {
        strictest = Some(match strictest {
            Some(current) => stricter(&current, limit),
            None => limit.clone(),
        });
    }
    strictest
}

/// Merge the auth declarations of the layers, keeping the stored value.
fn merge_auth(ancestors: &[Layer], base: &Layer, owner: Uuid) -> Option<AuthConfig> {
    for layer in ancestors
        .iter()
        .filter(|l| Layer::inherits(l.auth_sharing, l.owner, owner))
    {
        if layer.auth_sharing == Sharing::Enforce
            && let Some(auth) = &layer.auth {
                return Some(auth.clone());
            }
    }
    base.auth.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HeadersConfig, HttpMatch, MatchRule, MatchType, PluginBinding, PluginsConfig,
        Protocol, RequestHeaders, ResponseHeaders, Scheme, ServerConfig, SustainedRate, Timestamp,
    };
    use std::collections::BTreeMap;

    fn endpoint() -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: "api.vendor.com".to_owned(),
            port: 443,
        }
    }

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            tags: vec!["edge".to_owned()],
            server: ServerConfig {
                endpoints: vec![endpoint()],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::now(),
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            enabled: true,
            matches: MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::SuffixMode::Append,
                }),
                grpc: None,
            },
            match_type: MatchType::Http,
            priority: 0,
            tags: vec!["route-tag".to_owned()],
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::now(),
        }
    }

    fn limit(rate: u64, window: RateWindow, sharing: Sharing) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: None,
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            cost: 1,
        }
    }

    fn snapshot(upstreams: Vec<Upstream>) -> ConfigSnapshot {
        let mut store = ConfigSnapshot::default();
        for upstream in upstreams {
            store
                .upstreams
                .insert((upstream.tenant_id, upstream.id), Arc::new(upstream));
        }
        store
    }

    #[test]
    fn the_upstream_is_the_base_layer() {
        let tenant = Uuid::new_v4();
        let mut record = upstream(tenant, "api.vendor.com");
        record.headers = Some(HeadersConfig {
            request: RequestHeaders {
                set: BTreeMap::from([("x-a".to_owned(), "1".to_owned())]),
                ..RequestHeaders::default()
            },
            response: ResponseHeaders::default(),
        });
        let route_record = route(tenant, record.id);
        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert_eq!(
            merged.headers.request.set.get("x-a").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn the_route_overrides_the_upstream() {
        let tenant = Uuid::new_v4();
        let mut record = upstream(tenant, "api.vendor.com");
        record.headers = Some(HeadersConfig {
            request: RequestHeaders {
                set: BTreeMap::from([("x-a".to_owned(), "upstream".to_owned())]),
                ..RequestHeaders::default()
            },
            response: ResponseHeaders::default(),
        });
        let mut route_record = route(tenant, record.id);
        route_record.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.dev".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert!(merged.cors_enabled());
        assert!(merged.origin_allowed("https://app.dev"));
        assert!(!merged.origin_allowed("https://other.dev"));
        // The upstream header rule survives, because the route declares none.
        assert_eq!(
            merged.headers.request.set.get("x-a").map(String::as_str),
            Some("upstream")
        );
    }

    #[test]
    fn an_enforced_ancestor_value_cannot_be_lifted_by_shadowing() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ancestor = upstream(parent, "api.vendor.com");
        ancestor.rate_limit = Some(limit(10, RateWindow::Minute, Sharing::Enforce));
        let mut own = upstream(child, "api.vendor.com");
        own.rate_limit = Some(limit(1000, RateWindow::Minute, Sharing::Inherit));
        let route_record = route(child, own.id);

        let merged = merge(
            &snapshot(vec![ancestor.clone()]),
            &own,
            &route_record,
            &[child, parent],
            &[Arc::new(ancestor)],
            &[],
        );

        let bound = merged.rate_limit.expect("a bound is enforced");
        assert_eq!(bound.sustained.rate, 10);
        assert!(merged
            .enforced
            .iter()
            .any(|constraint| constraint.field == EnforcedField::RateLimit));
    }

    #[test]
    fn an_inherited_ancestor_origin_set_is_unioned() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ancestor = upstream(parent, "api.vendor.com");
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://parent.dev".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let own = upstream(child, "api.vendor.com");
        let mut route_record = route(child, own.id);
        route_record.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://child.dev".to_owned()],
            allowed_methods: vec!["POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });

        let merged = merge(
            &snapshot(vec![ancestor]),
            &own,
            &route_record,
            &[child, parent],
            &[],
            &[],
        );

        let cors = merged.cors.expect("CORS is enabled");
        assert!(cors.allowed_origins.contains(&"https://parent.dev".to_owned()));
        assert!(cors.allowed_origins.contains(&"https://child.dev".to_owned()));
    }

    #[test]
    fn an_enforced_ancestor_origin_set_is_kept() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ancestor = upstream(parent, "api.vendor.com");
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Enforce,
            enabled: true,
            allowed_origins: vec!["https://parent.dev".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let own = upstream(child, "api.vendor.com");
        let mut route_record = route(child, own.id);
        route_record.cors = Some(CorsConfig {
            sharing: Sharing::Enforce,
            enabled: true,
            allowed_origins: vec!["https://child.dev".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });

        let merged = merge(
            &snapshot(vec![ancestor]),
            &own,
            &route_record,
            &[child, parent],
            &[],
            &[],
        );

        let cors = merged.cors.expect("CORS is enabled");
        assert_eq!(cors.allowed_origins, vec!["https://parent.dev".to_owned()]);
    }

    #[test]
    fn a_private_ancestor_declaration_is_not_inherited() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ancestor = upstream(parent, "api.vendor.com");
        ancestor.rate_limit = Some(limit(5, RateWindow::Minute, Sharing::Private));
        let own = upstream(child, "api.vendor.com");
        let route_record = route(child, own.id);

        let merged = merge(
            &snapshot(vec![ancestor]),
            &own,
            &route_record,
            &[child, parent],
            &[],
            &[],
        );

        assert!(merged.rate_limit.is_none(), "a private limit is not inherited");
        assert!(merged.enforced.is_empty());
    }

    #[test]
    fn the_strictest_limit_wins_across_window_units() {
        let per_second = limit(1, RateWindow::Second, Sharing::Inherit);
        let per_minute = limit(100, RateWindow::Minute, Sharing::Inherit);
        let tenant = Uuid::new_v4();
        let mut record = upstream(tenant, "api.vendor.com");
        record.rate_limit = Some(per_minute);
        let mut route_record = route(tenant, record.id);
        route_record.rate_limit = Some(per_second);

        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert_eq!(merged.rate_limit.unwrap().sustained.window, RateWindow::Second);
    }

    #[test]
    fn the_plugin_bindings_are_concatenated_upstream_before_route() {
        let tenant = Uuid::new_v4();
        let binding = |reference: &str| PluginBinding {
            position: 0,
            reference: reference.to_owned(),
            plugin_uuid: None,
            config: None,
        };
        let mut record = upstream(tenant, "api.vendor.com");
        record.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("gts.cf.core.oagw.auth_plugin.v1~upstream")],
        });
        let mut route_record = route(tenant, record.id);
        route_record.plugins = Some(PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("gts.cf.core.oagw.guard_plugin.v1~route")],
        });

        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert_eq!(merged.plugins.len(), 2);
        assert_eq!(merged.plugins[0].reference, "gts.cf.core.oagw.auth_plugin.v1~upstream");
        assert_eq!(merged.plugins[1].reference, "gts.cf.core.oagw.guard_plugin.v1~route");
    }

    #[test]
    fn the_auth_declaration_is_carried_as_stored() {
        let tenant = Uuid::new_v4();
        let mut record = upstream(tenant, "api.vendor.com");
        record.auth = Some(AuthConfig {
            kind: "gts.cf.core.oagw.oauth2_client_credentials_auth_plugin.v1".to_owned(),
            sharing: Sharing::Inherit,
            config: BTreeMap::from([
                (
                    "token_url".to_owned(),
                    "cred://tenant/token_url".to_owned(),
                ),
                ("audience".to_owned(), "payments".to_owned()),
            ]),
        });
        let route_record = route(tenant, record.id);

        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        let auth = merged.auth.as_ref().expect("the declaration is carried");
        assert!(auth.config["token_url"].starts_with("cred://"));
        // No resolved secret lands on the merged configuration.
        let rendered = format!("{merged:?}").to_lowercase();
        assert!(!rendered.contains("bearer"));
    }

    #[test]
    fn tags_are_an_add_only_union() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut ancestor = upstream(parent, "api.vendor.com");
        ancestor.tags = vec!["edge".to_owned(), "pci".to_owned()];
        let own = upstream(child, "api.vendor.com");
        let route_record = route(child, own.id);

        let merged = merge(
            &snapshot(vec![ancestor]),
            &own,
            &route_record,
            &[child, parent],
            &[],
            &["edge".to_owned()],
        );

        assert!(merged.tags.contains(&"pci".to_owned()));
        assert_eq!(
            merged.tags.iter().filter(|tag| *tag == "edge").count(),
            1,
            "a tag is added once"
        );
        assert!(merged.tags.contains(&"route-tag".to_owned()));
    }

    #[test]
    fn an_upstream_without_any_declaration_yields_an_empty_configuration() {
        let tenant = Uuid::new_v4();
        let record = upstream(tenant, "api.vendor.com");
        let route_record = route(tenant, record.id);
        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert!(merged.cors.is_none());
        assert!(merged.rate_limit.is_none());
        assert!(merged.plugins.is_empty());
        assert!(merged.enforced.is_empty());
        assert!(merged.auth.is_none());
        assert!(!merged.cors_enabled());
    }

    #[test]
    fn the_enforced_field_names_are_the_schema_members() {
        assert_eq!(EnforcedField::Headers.as_str(), "headers");
        assert_eq!(EnforcedField::Cors.as_str(), "cors");
        assert_eq!(EnforcedField::RateLimit.as_str(), "rate_limit");
        assert_eq!(EnforcedField::Plugins.as_str(), "plugins");
        assert_eq!(EnforcedField::Auth.as_str(), "auth");
    }

    #[test]
    fn a_wildcard_origin_admits_any_origin() {
        let tenant = Uuid::new_v4();
        let mut record = upstream(tenant, "api.vendor.com");
        record.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let route_record = route(tenant, record.id);
        let merged = merge(&snapshot(vec![]), &record, &route_record, &[tenant], &[], &[]);
        assert!(merged.origin_allowed("https://anything.dev"));
        assert!(merged.method_allowed("get"));
        assert!(!merged.method_allowed("DELETE"));
        assert!(merged.exposed_headers().is_empty());
        assert!(!merged.allow_credentials());
    }
}
