//! Write-side sharing modes and ancestor permission gates
//! (`cpt-cf-oagw-algo-sharing-mode-validate`).
//!
//! The management API is tenant-scoped: a record of another tenant is not
//! addressable at all (`cpt-cf-oagw-dod-tenant-scoping`). What this module adds
//! is the write-side half of the hierarchical configuration: a write whose
//! `alias` resolves to an ancestor upstream is a *bind*, and the blocks the
//! ancestor declared as `inherit` or `enforce` constrain what the descendant may
//! write. The read-time effective merge (upstream < route < tenant) belongs to
//! entry 2.4; nothing here merges at read time.
//!
//! # Ports
//!
//! The ancestor chain comes from [`TenantHierarchy`], a port the transport
//! resolves from `tenant-resolver`; the gate itself is synchronous and takes the
//! resolved chain, so the domain never awaits a resolver call.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::ManagementError;
use crate::domain::model::{PluginBinding, RateLimitConfig, Sharing, Upstream};
use crate::domain::repo::UpstreamRepository;

// @cpt-begin:cpt-cf-oagw-dod-sharing-and-permissions:p1:inst-full
/// Permission of an upstream create (`cpt-cf-oagw-interface-api`).
pub const PERM_UPSTREAM_CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";

/// Permission of an upstream replace.
pub const PERM_UPSTREAM_OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";

/// Permission of an upstream read or list.
pub const PERM_UPSTREAM_READ: &str = "gts.cf.core.oagw.upstream.v1~:read";

/// Permission of an upstream delete.
pub const PERM_UPSTREAM_DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";

/// Permission of a route create.
pub const PERM_ROUTE_CREATE: &str = "gts.cf.core.oagw.route.v1~:create";

/// Permission of a route replace.
pub const PERM_ROUTE_OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";

/// Permission of a route read or list.
pub const PERM_ROUTE_READ: &str = "gts.cf.core.oagw.route.v1~:read";

/// Permission of a route delete.
pub const PERM_ROUTE_DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";

/// Permission of a proxy request (`cpt-cf-oagw-interface-api`).
pub const PERM_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// Bind gate: a create whose `alias` matches an ancestor upstream (`inst-shar-03`).
pub const PERM_UPSTREAM_BIND: &str = "oagw:upstream:bind";

/// Auth-override gate (`inst-shar-07`).
pub const PERM_UPSTREAM_OVERRIDE_AUTH: &str = "oagw:upstream:override_auth";

/// Rate-limit-override gate (`inst-shar-08`).
pub const PERM_UPSTREAM_OVERRIDE_RATE: &str = "oagw:upstream:override_rate";

/// Plugin-append gate (`inst-shar-09`).
pub const PERM_UPSTREAM_ADD_PLUGINS: &str = "oagw:upstream:add_plugins";

/// The calling principal, resolved by the transport from the security context.
///
/// The scopes are the token's scope strings; the wildcard `*` means
/// unrestricted, which is what the toolkit issues to a platform operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// Tenant the operation is scoped to.
    pub tenant_id: Uuid,
    /// Principal identifier, for the audit line of entry 2.7.
    pub subject_id: Uuid,
    /// Token scope strings, `*` meaning unrestricted.
    pub scopes: Vec<String>,
}

impl Actor {
    /// An actor with the given token scopes.
    #[must_use]
    pub fn new(tenant_id: Uuid, subject_id: Uuid, scopes: Vec<String>) -> Self {
        Self {
            tenant_id,
            subject_id,
            scopes,
        }
    }

    /// An unrestricted actor of one tenant (the platform-operator posture).
    #[must_use]
    pub fn unrestricted(tenant_id: Uuid) -> Self {
        Self::new(tenant_id, Uuid::nil(), vec!["*".to_string()])
    }

    /// Whether `permission` is granted by the token scopes.
    #[must_use]
    pub fn is_granted(&self, permission: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == "*" || scope == permission)
    }

    /// Require `permission`, failing with `403` when it is not granted.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::forbidden`] naming the missing permission.
    pub fn require(&self, permission: &str) -> Result<(), ManagementError> {
        if self.is_granted(permission) {
            return Ok(());
        }
        Err(ManagementError::forbidden(format!(
            "the caller does not hold `{permission}`"
        )))
    }

    /// Require at least one of `permissions`, failing with `403` when none is.
    ///
    /// The gate of a request that addresses a family of resource types and so
    /// carries no single permission to derive, which is the plugin list: the
    /// type-specific gate of a definition is applied once its own type is known.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::forbidden`] naming every permission the
    /// caller would have needed.
    pub fn require_any(&self, permissions: &[String]) -> Result<(), ManagementError> {
        if permissions.iter().any(|permission| self.is_granted(permission)) {
            return Ok(());
        }
        Err(ManagementError::forbidden(format!(
            "the caller does not hold any of {}",
            permissions
                .iter()
                .map(|permission| format!("`{permission}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

// @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-01
/// Ancestor chain of the calling tenant, resolved by the transport.
///
/// `tenant-resolver` supplies the chain: the gear mounts a
/// [`crate::infra::hierarchy::ResolverHierarchy`] over the
/// `TenantResolverClient` it finds in the client hub and falls back to
/// [`FlatHierarchy`] when that dependency is not wired. The caller's security
/// context travels with the call, because the resolver passes it to its plugin
/// for the access-control decision, exactly as the sibling gears do.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Ancestor tenants of `tenant_id`, nearest ancestor first, self excluded.
    ///
    /// # Errors
    ///
    /// Returns the mapped `503` when the hierarchy source cannot be reached, so
    /// the write fails closed instead of silently pretending the tenant has no
    /// ancestors.
    async fn ancestors_of(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, ManagementError>;
}

/// Single-tenant posture: no ancestor chain, so no bind or sharing gate fires.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlatHierarchy;

#[async_trait]
impl TenantHierarchy for FlatHierarchy {
    async fn ancestors_of(
        &self,
        _ctx: &SecurityContext,
        _tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, ManagementError> {
        Ok(Vec::new())
    }
}

/// Hierarchy with a fixed chain per tenant, for tests and for a host that
/// resolves the chains once at startup.
#[derive(Debug, Default, Clone)]
pub struct StaticHierarchy {
    chains: BTreeMap<Uuid, Vec<Uuid>>,
}

impl StaticHierarchy {
    /// A hierarchy reporting the given chains.
    #[must_use]
    pub fn new(chains: BTreeMap<Uuid, Vec<Uuid>>) -> Self {
        Self { chains }
    }
}

#[async_trait]
impl TenantHierarchy for StaticHierarchy {
    async fn ancestors_of(
        &self,
        _ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, ManagementError> {
        Ok(self.chains.get(&tenant_id).cloned().unwrap_or_default())
    }
}

/// An ancestor upstream a descendant write binds to or inherits from.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAncestor {
    /// Tenant that owns the ancestor upstream.
    pub tenant_id: Uuid,
    /// The ancestor's stored definition.
    pub upstream: Arc<Upstream>,
}

/// The ancestor constraint set a write was accepted under (`inst-shar-12`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AncestorConstraints {
    /// Ancestor tenants of the calling tenant, nearest first (`inst-shar-01`).
    pub chain: Vec<Uuid>,
    /// Whether the alias resolved to an ancestor upstream, i.e. the write is a
    /// bind (`inst-shar-02`).
    pub bound: bool,
    /// The ancestor upstreams the write resolved, nearest first.
    pub ancestors: Vec<ResolvedAncestor>,
    /// Field names inherited in `enforce` mode, which the write may not override.
    pub enforced: Vec<&'static str>,
    /// Tags the union added from the ancestor chain (`inst-shar-10`).
    pub inherited_tags: Vec<String>,
}

impl AncestorConstraints {
    /// Whether the write is a bind to an ancestor upstream.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.bound
    }
}
// @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-01

/// Applies the ancestor bind and sharing-mode gates of an upstream write.
///
/// The gate is constructed per request with the already-resolved ancestor chain
/// and never mutates the store: it validates and returns the *effective* record
/// the store then writes. The chain is resolved by the caller — the management
/// service awaits [`TenantHierarchy::ancestors_of`] — so this half of the
/// algorithm stays synchronous.
pub struct SharingGate {
    /// Ancestor tenants of the calling tenant, nearest first (`inst-shar-01`).
    chain: Vec<Uuid>,
}

impl SharingGate {
    /// A gate over the given ancestor chain.
    #[must_use]
    pub fn new(chain: Vec<Uuid>) -> Self {
        Self { chain }
    }

    /// The ancestor chain the gate was built with (`inst-shar-01`).
    #[must_use]
    pub fn chain(&self) -> &[Uuid] {
        &self.chain
    }

    /// The ancestor upstreams of the chain that carry `alias`, nearest first.
    ///
    /// An ancestor record is reachable only through the chain walk; the
    /// management API itself never resolves a foreign tenant key
    /// (`cpt-cf-oagw-dod-tenant-scoping`).
    #[must_use]
    pub fn resolve_ancestors<U: UpstreamRepository + ?Sized>(
        &self,
        store: &U,
        alias: &str,
    ) -> Vec<ResolvedAncestor> {
        self.chain
            .iter()
            .filter_map(|ancestor| {
                store
                    .find_upstream_by_alias(*ancestor, alias)
                    .map(|upstream| ResolvedAncestor {
                        tenant_id: *ancestor,
                        upstream,
                    })
            })
            .collect()
    }

    /// Gate an upstream create (`cpt-cf-oagw-flow-upstream-create`).
    ///
    /// Returns the effective record to store — tags unioned, plugin chain
    /// appended — together with the ancestor constraint set.
    ///
    /// # Errors
    ///
    /// Returns the mapped `403` of a missing bind or override permission and the
    /// mapped `400` of an `enforce`-mode override or a weaker rate limit.
    pub fn check_upstream_create<U: UpstreamRepository + ?Sized>(
        &self,
        store: &U,
        actor: &Actor,
        proposed: Upstream,
    ) -> Result<(Upstream, AncestorConstraints), ManagementError> {
        let ancestors = self.resolve_ancestors(store, &proposed.alias);
        self.apply(actor, proposed, &ancestors, &[])
    }

    /// Gate an upstream replace (`cpt-cf-oagw-flow-upstream-replace`).
    ///
    /// The alias is immutable, so the ancestor resolution is the stored record's;
    /// the tags the record already carries are also inherited, so a replace can
    /// never drop one.
    ///
    /// # Errors
    ///
    /// Same outcomes as [`Self::check_upstream_create`].
    pub fn check_upstream_replace<U: UpstreamRepository + ?Sized>(
        &self,
        store: &U,
        actor: &Actor,
        existing: &Upstream,
        proposed: Upstream,
    ) -> Result<(Upstream, AncestorConstraints), ManagementError> {
        let ancestors = self.resolve_ancestors(store, &existing.alias);
        self.apply(actor, proposed, &ancestors, &existing.tags)
    }

    // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-02
    /// Apply the gates of one write against the resolved ancestors.
    fn apply(
        &self,
        actor: &Actor,
        mut proposed: Upstream,
        ancestors: &[ResolvedAncestor],
        previous_tags: &[String],
    ) -> Result<(Upstream, AncestorConstraints), ManagementError> {
        // The write is a bind when an ancestor upstream carries the same alias.
        let bound = !ancestors.is_empty();
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-02

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-03
        if bound {
            actor.require(PERM_UPSTREAM_BIND)?;
        }
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-03

        // The ancestor values this write may inherit, nearest first. A block the
        // ancestor declared `private` is invisible and contributes nothing.
        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-04
        let inherited_auth = nearest_visible(ancestors, |upstream| upstream.auth.as_ref());
        let inherited_rate = nearest_visible(ancestors, |upstream| upstream.rate_limit.as_ref());
        let inherited_cors = nearest_visible(ancestors, |upstream| upstream.cors.as_ref());
        let inherited_plugins = nearest_visible(ancestors, |upstream| upstream.plugins.as_ref());
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-04

        let mut enforced = Vec::new();

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-05
        // An `enforce` block blocks a descendant override of the same block.
        // `headers` declares no sharing scope, so it is never gated here.
        if proposed.auth.is_some() && is_enforced(inherited_auth.map(|(_, auth)| &auth.sharing)) {
            enforced.push("auth");
        }
        if proposed.cors.is_some() && is_enforced(inherited_cors.map(|(_, cors)| &cors.sharing)) {
            enforced.push("cors");
        }
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-05

        if !enforced.is_empty() {
            // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-06
            return Err(ManagementError::validation(format!(
                "an ancestor tenant enforces `{}`; a descendant may not override it",
                enforced.join("`, `")
            )));
            // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-06
        }

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-07
        // An auth override of an inherited declaration needs its own permission
        // and keeps `auth.config` on `cred://` references only.
        if let Some((_, ancestor_auth)) = inherited_auth {
            if let Some(proposed_auth) = proposed.auth.as_ref() {
                actor.require(PERM_UPSTREAM_OVERRIDE_AUTH)?;
                for (name, value) in &proposed_auth.config {
                    if !value.starts_with("cred://") {
                        return Err(ManagementError::validation(format!(
                            "auth.config.{name}: must reference credentials by `cred://`"
                        )));
                    }
                }
            }
            if ancestor_auth.sharing == Sharing::Enforce {
                enforced.push("auth");
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-07

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-08
        // A descendant rate limit is accepted only with the override permission,
        // and never weaker than an enforced ancestor value, because
        // `min(ancestor.enforced, descendant)` still applies.
        if let Some((_, ancestor_rate)) = inherited_rate
            && let Some(proposed_rate) = proposed.rate_limit.as_ref()
        {
            actor.require(PERM_UPSTREAM_OVERRIDE_RATE)?;
            if ancestor_rate.sharing == Sharing::Enforce
                && per_second(proposed_rate) > per_second(ancestor_rate)
            {
                return Err(ManagementError::validation(
                    "rate_limit: an ancestor tenant enforces a stronger limit, so a \
                     descendant may not weaken it"
                        .to_string(),
                ));
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-08

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-09
        // Descendant plugins append to the inherited chain; an enforced ancestor
        // plugin can never be removed.
        let inherited_items = inherited_plugins
            .map(|(_, plugins)| plugins.items.as_slice())
            .unwrap_or_default();
        let proposed_items = proposed.plugins.as_ref().map(|plugins| plugins.items.as_slice());
        let appends = proposed_items.is_some_and(|items| extends(items, inherited_items));
        if appends {
            actor.require(PERM_UPSTREAM_ADD_PLUGINS)?;
        }
        let mut effective_items = inherited_items.to_vec();
        if let Some(items) = proposed_items {
            if is_enforced(inherited_plugins.map(|(_, plugins)| &plugins.sharing))
                && !starts_with(items, inherited_items)
            {
                enforced.push("plugins");
                return Err(ManagementError::validation(
                    "plugins.items: an ancestor tenant enforces its plugin chain, so a \
                     descendant may not remove or reorder it"
                        .to_string(),
                ));
            }
            for item in items {
                if inherited_items
                    .iter()
                    .any(|inherited| inherited.reference == item.reference)
                {
                    continue;
                }
                effective_items.push(item.clone());
            }
        }
        renumber(&mut effective_items);
        if let Some(plugins) = proposed.plugins.as_mut() {
            plugins.items = effective_items;
        } else if !inherited_items.is_empty() {
            let sharing = inherited_plugins
                .map(|(_, plugins)| plugins.sharing)
                .unwrap_or_default();
            proposed.plugins = Some(crate::domain::model::PluginsConfig {
                sharing,
                items: effective_items,
            });
        }
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-09

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-10
        // Tags are an add-only union: the request's tags are tenant-local
        // additions and an inherited tag is never removed, including on a
        // binding create.
        let mut effective_tags = previous_tags.to_vec();
        effective_tags.extend(inherited_tags(ancestors));
        effective_tags.extend(proposed.tags.iter().cloned());
        effective_tags.sort();
        effective_tags.dedup();
        proposed.tags = effective_tags.clone();
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-10

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-11
        // The write-side constraints end here. The read-time effective merge —
        // upstream < route < tenant — is entry 2.4's job and is not performed in
        // this algorithm, which only records what the write was accepted under.
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-11

        let constraints = AncestorConstraints {
            chain: ancestors.iter().map(|item| item.tenant_id).collect(),
            bound,
            ancestors: ancestors.to_vec(),
            enforced,
            inherited_tags: inherited_tags(ancestors),
        };

        // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-12
        Ok((proposed, constraints))
        // @cpt-end:cpt-cf-oagw-algo-sharing-mode-validate:p1:inst-shar-12
    }
}
// @cpt-end:cpt-cf-oagw-dod-sharing-and-permissions:p1:inst-full

/// The nearest ancestor carrying a visible value of one block.
///
/// `visible` is the `sharing != private` rule (`inst-shar-04`): a `private`
/// ancestor block is invisible to the descendant and contributes no inheritable
/// value.
fn nearest_visible<'a, T>(
    ancestors: &'a [ResolvedAncestor],
    block: impl Fn(&'a Upstream) -> Option<&'a T>,
) -> Option<(&'a ResolvedAncestor, &'a T)>
where
    T: HasSharing,
{
    ancestors.iter().find_map(|resolved| {
        let value = block(&resolved.upstream)?;
        (value.sharing_ref() != Sharing::Private).then_some((resolved, value))
    })
}

/// The sharing scope of a configuration block.
trait HasSharing {
    fn sharing_ref(&self) -> Sharing;
}

macro_rules! has_sharing {
    ($($ty:ty),* $(,)?) => {
        $(impl HasSharing for $ty {
            fn sharing_ref(&self) -> Sharing {
                self.sharing
            }
        })*
    };
}

has_sharing!(
    crate::domain::model::AuthConfig,
    crate::domain::model::RateLimitConfig,
    crate::domain::model::CorsConfig,
    crate::domain::model::PluginsConfig,
);

/// Whether the visible ancestor value blocks a descendant override.
fn is_enforced(sharing: Option<&Sharing>) -> bool {
    sharing.is_some_and(|sharing| *sharing == Sharing::Enforce)
}

/// The tags the ancestor chain contributes (`inst-shar-10`).
fn inherited_tags(ancestors: &[ResolvedAncestor]) -> Vec<String> {
    let mut tags = Vec::new();
    for ancestor in ancestors {
        tags.extend(ancestor.upstream.tags.iter().cloned());
    }
    tags
}

/// Whether `items` holds anything beyond `inherited`, i.e. appends to the chain.
fn extends(items: &[PluginBinding], inherited: &[PluginBinding]) -> bool {
    items.len() > inherited.len()
        || items
            .iter()
            .any(|item| !inherited.iter().any(|known| known.reference == item.reference))
}

/// Whether `items` keeps `inherited` as its prefix.
fn starts_with(items: &[PluginBinding], inherited: &[PluginBinding]) -> bool {
    items.len() >= inherited.len()
        && items
            .iter()
            .zip(inherited.iter())
            .all(|(item, known)| item.reference == known.reference)
}

/// Renumber a plugin chain contiguously from `0`.
fn renumber(items: &mut [PluginBinding]) {
    for (position, item) in items.iter_mut().enumerate() {
        item.position = u32::try_from(position).unwrap_or(u32::MAX);
    }
}

/// Requests per second of a rate limit, so an enforced ancestor limit can be
/// compared with a descendant one across windows.
fn per_second(rate: &RateLimitConfig) -> f64 {
    let seconds = rate.sustained.window.seconds();
    if seconds == 0 {
        return 0.0;
    }
    rate.sustained.rate as f64 / seconds as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::domain::model::{
        AuthConfig, CorsConfig, Endpoint, HeadersConfig, PluginBinding, PluginsConfig, Protocol,
        RateWindow, Scheme, ServerConfig, SustainedRate, Timestamp,
    };

    fn empty_headers() -> HeadersConfig {
        HeadersConfig {
            request: crate::domain::model::RequestHeaders::default(),
            response: crate::domain::model::ResponseHeaders::default(),
        }
    }

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000001aa");
    const ANCESTOR: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000100");
    const SUBJECT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000002bb");

    #[derive(Default)]
    struct RecordingStore {
        upstreams: BTreeMap<(Uuid, String), Arc<Upstream>>,
    }

    impl RecordingStore {
        fn with(mut self, tenant: Uuid, upstream: Upstream) -> Self {
            self.upstreams
                .insert((tenant, upstream.alias.clone()), Arc::new(upstream));
            self
        }
    }

    impl UpstreamRepository for RecordingStore {
        fn insert_upstream(
            &self,
            _upstream: Upstream,
        ) -> Result<Arc<Upstream>, ManagementError> {
            unimplemented!("the gate only reads the store");
        }

        fn replace_upstream(
            &self,
            _existing: &Upstream,
            _replacement: Upstream,
        ) -> Result<Arc<Upstream>, ManagementError> {
            unimplemented!("the gate only reads the store");
        }

        fn delete_upstream(
            &self,
            _tenant_id: Uuid,
            _id: Uuid,
        ) -> Result<Vec<Arc<crate::domain::model::Route>>, ManagementError> {
            unimplemented!("the gate only reads the store");
        }

        fn find_upstream(&self, _tenant_id: Uuid, _id: Uuid) -> Option<Arc<Upstream>> {
            unimplemented!("the gate only reads the store");
        }

        fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
            self.upstreams.get(&(tenant_id, alias.to_string())).cloned()
        }

        fn list_upstreams(&self, _tenant_id: Uuid) -> Vec<Arc<Upstream>> {
            unimplemented!("the gate only reads the store");
        }
    }

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

    fn rate(rate: u64, window: RateWindow, sharing: Sharing) -> RateLimitConfig {
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

    fn plugins(sharing: Sharing, references: &[&str]) -> PluginsConfig {
        PluginsConfig {
            sharing,
            items: references
                .iter()
                .enumerate()
                .map(|(position, reference)| PluginBinding {
                    position: u32::try_from(position).unwrap_or(u32::MAX),
                    reference: (*reference).to_string(),
                    plugin_uuid: None,
                    config: None,
                })
                .collect(),
        }
    }

    fn auth(sharing: Sharing) -> AuthConfig {
        AuthConfig {
            kind: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.api_key.v1".to_string(),
            sharing,
            config: BTreeMap::from([(
                "key".to_string(),
                "cred://vendor/api-key".to_string(),
            )]),
        }
    }

    fn chain(ancestors: Vec<Uuid>) -> Vec<Uuid> {
        ancestors
    }

    /// An actor holding only the given permissions.
    fn actor(scopes: &[&str]) -> Actor {
        Actor::new(
            TENANT,
            SUBJECT,
            scopes.iter().map(|scope| (*scope).to_string()).collect(),
        )
    }

    #[test]
    fn a_create_without_ancestors_is_accepted_unrestricted() {
        let gate = SharingGate::new(chain(Vec::new()));
        let store = RecordingStore::default();
        let (effective, constraints) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), upstream(TENANT, "api.vendor.com"))
            .expect("accepted");
        assert!(!constraints.is_bound());
        assert!(constraints.chain.is_empty());
        assert_eq!(effective.tags, Vec::<String>::new());
    }

    #[test]
    fn an_ancestor_alias_requires_the_bind_permission() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let ancestor = upstream(ANCESTOR, "api.vendor.com");
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let error = gate
            .check_upstream_create(
                &store,
                &Actor::new(TENANT, SUBJECT, vec![PERM_UPSTREAM_CREATE.to_string()]),
                upstream(TENANT, "api.vendor.com"),
            )
            .expect_err("no bind permission");
        assert_eq!(error.status(), 403);
        assert!(error.detail().contains(PERM_UPSTREAM_BIND), "{}", error.detail());

        let (effective, constraints) = gate
            .check_upstream_create(
                &store,
                &Actor::new(
                    TENANT,
                    SUBJECT,
                    vec![PERM_UPSTREAM_CREATE.to_string(), PERM_UPSTREAM_BIND.to_string()],
                ),
                upstream(TENANT, "api.vendor.com"),
            )
            .expect("bind granted");
        assert!(constraints.is_bound());
        assert_eq!(constraints.chain, vec![ANCESTOR]);
        assert_eq!(effective.tenant_id, TENANT, "the record stays the caller's");
    }

    #[test]
    fn a_private_ancestor_block_contributes_no_inheritable_value() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.rate_limit = Some(rate(100, RateWindow::Second, Sharing::Private));
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.rate_limit = Some(rate(10, RateWindow::Second, Sharing::Inherit));
        // The alias still matches an ancestor upstream, so the bind gate applies;
        // the private block is invisible, so no rate-override permission is
        // asked for beyond it.
        let (effective, constraints) = gate
            .check_upstream_create(&store, &actor(&[PERM_UPSTREAM_CREATE, PERM_UPSTREAM_BIND]), proposed)
            .expect("accepted without an auth or rate permission");
        assert!(constraints.is_bound());
        assert!(constraints.enforced.is_empty());
        assert_eq!(
            effective.rate_limit.as_ref().expect("kept").sustained.rate,
            10,
            "the descendant value stands, no ancestor value merged"
        );
    }

    #[test]
    fn an_enforced_ancestor_block_blocks_the_override() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Enforce,
            enabled: true,
            allowed_origins: vec!["https://console.example.com".to_string()],
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        let error = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect_err("enforce blocks the override");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("cors"), "{}", error.detail());
    }

    #[test]
    fn an_auth_override_needs_its_permission_and_cred_references() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.auth = Some(auth(Sharing::Inherit));
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.auth = Some(auth(Sharing::Inherit));

        let error = gate
            .check_upstream_create(
                &store,
                &actor(&[PERM_UPSTREAM_CREATE, PERM_UPSTREAM_BIND]),
                proposed.clone(),
            )
            .expect_err("no auth override permission");
        assert_eq!(error.status(), 403);
        assert!(error.detail().contains(PERM_UPSTREAM_OVERRIDE_AUTH));

        let mut with_secret = proposed.clone();
        if let Some(config) = with_secret.auth.as_mut() {
            config.config.insert("key".to_string(), "sk-raw-secret".to_string());
        }
        let error = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), with_secret)
            .expect_err("no secret material");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("cred://"), "{}", error.detail());

        let (effective, _) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect("cred:// reference accepted");
        assert_eq!(
            effective.auth.as_ref().expect("kept").config["key"],
            "cred://vendor/api-key"
        );
    }

    #[test]
    fn a_descendant_rate_limit_needs_the_permission_and_may_not_weaken_an_enforced_one() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.rate_limit = Some(rate(100, RateWindow::Second, Sharing::Enforce));
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut weaker = upstream(TENANT, "api.vendor.com");
        weaker.rate_limit = Some(rate(200, RateWindow::Second, Sharing::Inherit));
        let error = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), weaker)
            .expect_err("weaker than the enforced limit");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("rate_limit"), "{}", error.detail());

        // Tighter is accepted, with the override permission.
        let mut tighter = upstream(TENANT, "api.vendor.com");
        tighter.rate_limit = Some(rate(50, RateWindow::Second, Sharing::Inherit));
        let error = gate
            .check_upstream_create(
                &store,
                &actor(&[PERM_UPSTREAM_CREATE, PERM_UPSTREAM_BIND]),
                tighter.clone(),
            )
            .expect_err("no override permission");
        assert_eq!(error.status(), 403);
        assert!(error.detail().contains(PERM_UPSTREAM_OVERRIDE_RATE));

        let (effective, constraints) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), tighter)
            .expect("tighter limit accepted");
        assert_eq!(effective.rate_limit.as_ref().expect("kept").sustained.rate, 50);
        assert_eq!(constraints.enforced, Vec::<&str>::new());
    }

    #[test]
    fn descendant_plugins_append_to_the_inherited_chain() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.plugins = Some(plugins(
            Sharing::Inherit,
            &["gts.cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1"],
        ));
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        // Appending without the permission is refused.
        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.plugins = Some(plugins(
            Sharing::Inherit,
            &[
                "gts.cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1",
                "gts.cf.core.oagw.plugin.v1~cf.core.oagw.header_rewrite.v1",
            ],
        ));
        let error = gate
            .check_upstream_create(
                &store,
                &actor(&[PERM_UPSTREAM_CREATE, PERM_UPSTREAM_BIND]),
                proposed.clone(),
            )
            .expect_err("no add_plugins permission");
        assert_eq!(error.status(), 403);
        assert!(error.detail().contains(PERM_UPSTREAM_ADD_PLUGINS));

        // With the permission the chain is the inherited one plus the append,
        // renumbered contiguously from 0.
        let (effective, _) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect("append accepted");
        let items = &effective.plugins.as_ref().expect("kept").items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].position, 0);
        assert_eq!(items[1].position, 1);
        assert_eq!(
            items[1].reference,
            "gts.cf.core.oagw.plugin.v1~cf.core.oagw.header_rewrite.v1"
        );
    }

    #[test]
    fn an_enforced_ancestor_plugin_chain_cannot_be_removed() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.plugins = Some(plugins(
            Sharing::Enforce,
            &["gts.cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1"],
        ));
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.plugins = Some(plugins(
            Sharing::Inherit,
            &["gts.cf.core.oagw.plugin.v1~cf.core.oagw.header_rewrite.v1"],
        ));
        let error = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect_err("enforced plugin removed");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("plugins.items"), "{}", error.detail());

        // Omitting the block entirely keeps the enforced chain.
        let proposed = upstream(TENANT, "api.vendor.com");
        let (effective, _) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect("kept");
        let items = &effective.plugins.as_ref().expect("inherited").items;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].reference,
            "gts.cf.core.oagw.plugin.v1~cf.core.oagw.required_headers.v1"
        );
    }

    #[test]
    fn tags_are_an_add_only_union() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.tags = vec!["platform".to_string(), "core".to_string()];
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.tags = vec!["tenant-local".to_string()];
        let (effective, constraints) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect("accepted");
        assert_eq!(
            effective.tags,
            vec![
                "core".to_string(),
                "platform".to_string(),
                "tenant-local".to_string()
            ]
        );
        assert_eq!(
            constraints.inherited_tags,
            vec!["platform".to_string(), "core".to_string()]
        );

        // A replace cannot drop an inherited tag either.
        let mut existing = upstream(TENANT, "api.vendor.com");
        existing.tags = vec!["core".to_string(), "platform".to_string()];
        let mut replacement = upstream(TENANT, "api.vendor.com");
        replacement.tags = Vec::new();
        let (effective, _) = gate
            .check_upstream_replace(&store, &Actor::unrestricted(TENANT), &existing, replacement)
            .expect("accepted");
        assert_eq!(
            effective.tags,
            vec!["core".to_string(), "platform".to_string()],
            "the inherited tags survive a replace that omits them"
        );
    }

    #[test]
    fn the_chain_is_taken_from_the_hierarchy_port() {
        let grandparent = uuid::uuid!("00000000-0000-0000-0000-000000000000");
        let gate = SharingGate::new(chain(vec![ANCESTOR, grandparent]));
        assert_eq!(gate.chain(), &[ANCESTOR, grandparent]);

        // The nearest ancestor carrying the alias wins the inheritance.
        let mut nearest = upstream(ANCESTOR, "api.vendor.com");
        nearest.tags = vec!["nearest".to_string()];
        let mut furthest = upstream(grandparent, "api.vendor.com");
        furthest.tags = vec!["furthest".to_string()];
        let store = RecordingStore::default()
            .with(ANCESTOR, nearest)
            .with(grandparent, furthest);

        let (_, constraints) = gate
            .check_upstream_create(
                &store,
                &Actor::unrestricted(TENANT),
                upstream(TENANT, "api.vendor.com"),
            )
            .expect("accepted");
        assert_eq!(constraints.chain, vec![ANCESTOR, grandparent]);
        assert_eq!(constraints.ancestors.len(), 2);
    }

    #[test]
    fn headers_declare_no_sharing_scope_so_an_override_is_never_gated() {
        let gate = SharingGate::new(chain(vec![ANCESTOR]));
        let mut ancestor = upstream(ANCESTOR, "api.vendor.com");
        ancestor.headers = Some(empty_headers());
        ancestor
            .headers
            .as_mut()
            .expect("headers")
            .request
            .set
            .insert("x-tenant".to_string(), "ancestor".to_string());
        let store = RecordingStore::default().with(ANCESTOR, ancestor);

        let mut proposed = upstream(TENANT, "api.vendor.com");
        proposed.headers = Some(empty_headers());
        proposed
            .headers
            .as_mut()
            .expect("headers")
            .request
            .set
            .insert("x-tenant".to_string(), "descendant".to_string());
        let (effective, constraints) = gate
            .check_upstream_create(&store, &Actor::unrestricted(TENANT), proposed)
            .expect("accepted");
        assert!(constraints.enforced.is_empty());
        assert_eq!(
            effective.headers.as_ref().expect("kept").request.set["x-tenant"],
            "descendant"
        );
    }
}
