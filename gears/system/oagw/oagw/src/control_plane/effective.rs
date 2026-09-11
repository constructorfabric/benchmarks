//! The per-field-family effective merge and the resolution entry point.
//!
//! [`merge_upstream_layer`] and [`merge_route_layer`] realize
//! `cpt-cf-oagw-algo-field-family-merge`: they take the routing target's row,
//! the ordered ancestor bindings with the families they contribute, and the
//! layer being resolved, and answer one [`EffectiveUpstreamConfig`] or one
//! [`EffectiveRouteConfig`].
//!
//! [`resolve_effective`] is the entry `cpt-cf-oagw-flow-resolve-effective-config`
//! names. It normalizes the alias, orders the chain the platform tenant
//! resolver supplies, walks it, shadow-resolves the candidates, and merges
//! both layers. It answers [`ResolveError::UnavailableChain`] when the chain
//! cannot be ordered — the caller fails closed with the platform 500 problem
//! shape — and `None` when no chain element holds the alias, which the
//! consumer answers 404.
//!
//! The merge applies from root to child: the layers arrive most distant first
//! with the routing target last, so a closer layer's own value replaces a more
//! distant one, and a value an ancestor marked `enforce` is decided by that
//! ancestor and no closer layer can replace it. [`merge_upstream_layer`]
//! builds the upstream chain's layers and [`merge_route_layer`] the route
//! chain's; both feed the same five per-family merges, which is why a route
//! row and an upstream row are decided by one table.

// @cpt-dod:cpt-cf-oagw-dod-field-family-merge:p1

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::control_plane::chain::{ChainCandidate, walk_candidates};
use crate::control_plane::shadow::{ShadowResolution, route_contributed, shadow_resolve};
use crate::domain::alias::Alias;
use crate::domain::effective::{
    AncestorBinding, ContributedFamilies, EffectiveAuth, EffectiveCors, EffectivePluginChain,
    EffectiveRateLimit, EffectiveRouteConfig, EffectiveTagSet, EffectiveUpstreamConfig, Family,
    FamilyModes, RouteSelector, TenantChain,
};
use crate::domain::route::{GrpcMatch, HttpMatch, Route};
use crate::domain::upstream::{
    AuthConfig, Burst, CorsConfig, PluginsConfig, RateLimitConfig, SharingMode, Sustained, Window,
};
use crate::store::OagwStore;

/// Why a resolution produced no configuration.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The platform tenant-resolver supplied no ordered ancestor chain, or the
    /// alias handed in cannot be normalized. Both fail the resolution closed:
    /// no configuration is produced and nothing is guessed.
    #[error("the resolution cannot run: the alias or the ancestor chain is unusable")]
    UnavailableChain,
}

/// The answer of one effective-configuration resolution.
///
/// The per-family sharing modes and the resolved ownership the consumer needs
/// for its own authorization check are carried by the per-family results, each
/// of which names the `mode` that produced it and the `owner` whose value is
/// effective.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveResolution {
    /// The upstream-layer result.
    pub upstream: EffectiveUpstreamConfig,
    /// The route-layer result, when the chain holds a matching route.
    pub route: Option<EffectiveRouteConfig>,
    /// The effective `enabled` state of the routing target: the conjunction of
    /// the target's own flag with every matched ancestor row's flag.
    pub enabled: bool,
}

/// Resolves the effective configuration one proxy request is subject to.
///
/// This is the routine the Data Plane calls with the normalized alias, the
/// method and the path — ADR 0006's `CP.resolve_proxy_target(alias, method,
/// path)`. It writes nothing and reads only through the store.
///
/// # Errors
///
/// Returns [`ResolveError::UnavailableChain`] when the chain cannot be
/// ordered or the alias cannot be normalized; the caller answers the platform
/// 500 problem shape and never a partial configuration.
pub fn resolve_effective(
    store: &OagwStore,
    calling_tenant: Uuid,
    ancestors: &[Uuid],
    alias: &str,
    selector: &RouteSelector,
) -> Result<Option<EffectiveResolution>, ResolveError> {
    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-request
    // The Data Plane supplies the alias and the calling tenant it resolved
    // from the SecurityContext; this feature registers no endpoint of its own.
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-request

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-normalize
    // The same normalization routine the write path uses, so a resolution can
    // never disagree with a stored alias about shape, case, or a trailing dot.
    let normalized = Alias::parse(alias).map_err(|_| ResolveError::UnavailableChain)?;
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-normalize

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain
    // The chain the caller obtained from the platform tenant-resolver, ordered
    // calling tenant first.
    let chain = TenantChain::from_resolver(calling_tenant, ancestors);
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-if
    if chain.is_none() {
        // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-return
        // An unordered or cyclic chain cannot decide who shadows whom, so the
        // resolution fails closed instead of ordering candidates against it.
        return Err(ResolveError::UnavailableChain);
        // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-return
    }
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-if

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-else
    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-walk
    let candidates = walk_candidates(store, calling_tenant, ancestors, &normalized)
        .map_err(|_| ResolveError::UnavailableChain)?;
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-walk

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-shadow
    let Some(shadow) = shadow_resolve(&candidates, &normalized) else {
        // No chain element holds the alias: the not-found outcome the consumer
        // answers 404. No configuration is produced.
        return Ok(None);
    };
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-shadow

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-merge-upstream
    let upstream = merge_upstream_layer(&shadow.target, &shadow.bindings);
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-merge-upstream

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-merge-route
    let route = merge_route_layer(store, &shadow, selector);
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-merge-route

    // @cpt-begin:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-return
    Ok(Some(EffectiveResolution {
        enabled: shadow.enabled,
        upstream,
        route,
    }))
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-return
    // @cpt-end:cpt-cf-oagw-flow-resolve-effective-config:p1:inst-res-chain-else
}

/// What one merge layer holds of its own, as opposed to what it contributes to
/// its descendants.
///
/// An ancestor layer holds nothing of its own: its families reach the merge
/// only through its `contributed` member, which carries nothing for a family
/// the row marked `private`.
#[derive(Default)]
struct OwnFamilies<'a> {
    auth: Option<&'a AuthConfig>,
    rate_limit: Option<&'a RateLimitConfig>,
    plugins: Option<&'a PluginsConfig>,
    cors: Option<&'a CorsConfig>,
    tags: &'a [String],
}

/// One layer of a merge: a row's contribution to its descendants and what it
/// holds of its own.
///
/// The layers arrive most distant first with the routing target last, which is
/// the root-to-child application order DECOMPOSITION §1.5 states and which
/// makes the target's own values the ones applied last.
struct MergeLayer<'a> {
    /// The tenant that owns the row of this layer.
    tenant_id: Uuid,
    /// What the row contributes to its descendants: nothing for the routing
    /// target, which is the local layer of the merge.
    contributed: ContributedFamilies,
    /// The modes the row itself declares, which are the fallback when no
    /// ancestor contributes a family.
    modes: FamilyModes,
    /// The row's own family values.
    own: OwnFamilies<'a>,
}

/// Merges the upstream layer: the routing target's row as the base, the
/// ancestor bindings applied from root to child.
#[must_use]
pub fn merge_upstream_layer(
    target: &ChainCandidate,
    bindings: &[AncestorBinding],
) -> EffectiveUpstreamConfig {
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-base
    // The routing target's row is the base every family starts from, and the
    // bindings are held most distant first so the merge applies root to child.
    let base = &target.row.upstream;
    let mut layers: Vec<MergeLayer<'_>> = bindings
        .iter()
        .map(|binding| MergeLayer {
            tenant_id: binding.tenant_id,
            contributed: binding.contributed.clone(),
            modes: modes_of_contributed(&binding.contributed),
            own: OwnFamilies::default(),
        })
        .collect();
    // The routing target is the local layer: it contributes nothing, because
    // its own values are the ones the merge applies last.
    layers.push(MergeLayer {
        tenant_id: target.tenant_id,
        contributed: ContributedFamilies {
            auth: None,
            rate_limit: None,
            plugins: None,
            cors: None,
            tags: None,
        },
        modes: target.modes,
        own: OwnFamilies {
            auth: base.auth.as_ref(),
            rate_limit: base.rate_limit.as_ref(),
            plugins: base.plugins.as_ref(),
            cors: base.cors.as_ref(),
            tags: &base.tags,
        },
    });
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-base

    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-loop
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-apply
    // Every family the layer carries is merged with its own strategy row, and
    // each result carries the sharing mode and the owner that produced it.
    let auth = merge_auth(&layers);
    let rate_limit = merge_rate_limit(&layers);
    let plugins = merge_plugins(&layers);
    let cors = merge_cors(&layers);
    let tags = merge_tags(&layers);
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-apply
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-loop

    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-upstream-else
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-upstream
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-return
    EffectiveUpstreamConfig {
        tenant_id: target.tenant_id,
        upstream_id: target.upstream_id,
        auth,
        rate_limit,
        plugins,
        cors,
        tags,
    }
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-return
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-upstream
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-upstream-else
}

/// Merges the route layer along the same chain: the closest matching route
/// wins, and the more distant matching routes contribute their families with
/// the same strategies.
///
/// A route carries no authentication family, so the result has no `auth`
/// member to skip. A route's modes are read from each family's own `sharing`
/// member, which the shipped route schema defaults to `private`.
#[must_use]
pub fn merge_route_layer(
    store: &OagwStore,
    shadow: &ShadowResolution,
    selector: &RouteSelector,
) -> Option<EffectiveRouteConfig> {
    // The chain elements that hold an alias-matched upstream row are the only
    // ones whose routes can match; each is read once, scoped to its own
    // tenant, which is what keeps a route row of a tenant outside the chain
    // out of the candidate set.
    let mut matched: Vec<(usize, Uuid, Route)> = Vec::new();
    for binding in &shadow.bindings {
        collect_routes(
            store,
            binding.tenant_id,
            binding.upstream_id,
            binding.depth,
            selector,
            &mut matched,
        );
    }
    collect_routes(
        store,
        shadow.target.tenant_id,
        shadow.target.upstream_id,
        shadow.target.depth,
        selector,
        &mut matched,
    );
    // Most distant first, so the closest matching route is the last layer and
    // takes priority.
    matched.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
    let (_, local_tenant, local_route) = matched.last()?;

    let mut layers: Vec<MergeLayer<'_>> = Vec::new();
    for (_, tenant_id, route) in &matched[..matched.len() - 1] {
        let contributed = route_contributed(route);
        let modes = modes_of_contributed(&contributed);
        layers.push(MergeLayer {
            tenant_id: *tenant_id,
            contributed,
            modes,
            own: OwnFamilies::default(),
        });
    }
    layers.push(MergeLayer {
        tenant_id: *local_tenant,
        contributed: ContributedFamilies {
            auth: None,
            rate_limit: None,
            plugins: None,
            cors: None,
            tags: None,
        },
        modes: modes_of_contributed(&route_contributed(local_route)),
        own: OwnFamilies {
            auth: None,
            rate_limit: local_route.rate_limit.as_ref(),
            plugins: local_route.plugins.as_ref(),
            cors: local_route.cors.as_ref(),
            tags: &local_route.tags,
        },
    });

    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-route-if
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-route
    // @cpt-begin:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-return
    // The auth family is skipped: a route carries no authentication family, so
    // there is no inherited auth configuration for a route to resolve.
    Some(EffectiveRouteConfig {
        tenant_id: *local_tenant,
        route_id: local_route.id,
        upstream_id: local_route.upstream_id,
        rate_limit: merge_rate_limit(&layers),
        plugins: merge_plugins(&layers),
        cors: merge_cors(&layers),
        tags: merge_tags(&layers),
    })
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-return
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-route
    // @cpt-end:cpt-cf-oagw-algo-field-family-merge:p1:inst-merge-route-if
}

/// The modes one row declares, read from the families it contributes.
///
/// A family the row does not contribute takes the schema default `private`,
/// which is what [`FamilyModes::new`] supplies for whatever it is not told
/// about.
fn modes_of_contributed(contributed: &ContributedFamilies) -> FamilyModes {
    FamilyModes::new(
        contributed.auth.as_ref().map(|item| item.mode),
        contributed.rate_limit.as_ref().map(|item| item.mode),
        contributed.plugins.as_ref().map(|item| item.mode),
        contributed.cors.as_ref().map(|item| item.mode),
    )
}

/// Reads one chain element's routes for one upstream and keeps the ones the
/// selector matches.
fn collect_routes(
    store: &OagwStore,
    tenant_id: Uuid,
    upstream_id: Uuid,
    depth: usize,
    selector: &RouteSelector,
    matched: &mut Vec<(usize, Uuid, Route)>,
) {
    for row in store.routes_of_upstream(tenant_id, upstream_id) {
        if matches_selector(&row.route, selector) {
            matched.push((depth, tenant_id, row.route));
        }
    }
}

/// Whether one route matches the selector the resolution carries.
fn matches_selector(route: &Route, selector: &RouteSelector) -> bool {
    match selector {
        RouteSelector::Http { method, path } => route
            .match_config
            .http
            .as_ref()
            .is_some_and(|http: &HttpMatch| {
                (http.methods.is_empty() || http.methods.iter().any(|declared| declared == method))
                    && path_matches(&http.path, path)
            }),
        RouteSelector::Grpc { service, rpc } => route
            .match_config
            .grpc
            .as_ref()
            .is_some_and(|grpc: &GrpcMatch| &grpc.service == service && &grpc.method == rpc),
    }
}

/// Whether the configured match path addresses the request path.
///
/// A configured path matches itself and every path it prefixes at a segment
/// boundary, which is how a gateway route table is read: `/v1` addresses
/// `/v1` and `/v1/chat`, and never `/v1chat`.
fn path_matches(configured: &str, request: &str) -> bool {
    if configured == request {
        return true;
    }
    request
        .strip_prefix(configured)
        .is_some_and(|tail| tail.starts_with('/'))
}

/// The authentication family of one merged layer.
///
/// The closest layer that carries a value decides the owner, and an `enforce`
/// ancestor decides it for every closer layer. The routing target's own
/// binding is the closest layer, so it wins over an `inherit` base without
/// consuming any permission: `private` blocks the visibility of the ancestor's
/// value, not the descendant's own configuration.
fn merge_auth(layers: &[MergeLayer<'_>]) -> Option<EffectiveAuth> {
    let local = layers.last()?;
    let mut base: Option<(Uuid, SharingMode, AuthConfig)> = None;
    for layer in layers {
        let Some(contribution) = &layer.contributed.auth else {
            continue;
        };
        match contribution.mode {
            SharingMode::Enforce => {
                return Some(EffectiveAuth {
                    owner: layer.tenant_id,
                    mode: SharingMode::Enforce,
                    auth: contribution.value.clone(),
                });
            }
            SharingMode::Inherit => {
                if base.is_none() {
                    base = Some((layer.tenant_id, SharingMode::Inherit, contribution.value.clone()));
                }
            }
            SharingMode::Private => {}
        }
    }
    let Some(own) = local.own.auth else {
        // The routing target carries no `auth` object of its own, so the
        // inherited one is the effective one.
        return base.map(|(owner, mode, auth)| EffectiveAuth { owner, mode, auth });
    };
    Some(EffectiveAuth {
        owner: local.tenant_id,
        mode: local.modes.mode_of(Family::Auth),
        auth: own.clone(),
    })
}

/// The rate-limit family of one merged layer.
///
/// The minimum of the visible sustained rates applies, normalized per second
/// and reported in the winner's window, with the minimum of the visible burst
/// capacities under the same gate; the remaining members are carried unchanged
/// from the closest visible object, which is the routing target's own whenever
/// it carries one.
fn merge_rate_limit(layers: &[MergeLayer<'_>]) -> Option<EffectiveRateLimit> {
    let local = layers.last()?;
    let mut visible: Vec<VisibleLimit<'_>> = Vec::new();
    for layer in layers {
        let Some(contribution) = &layer.contributed.rate_limit else {
            continue;
        };
        visible.push(VisibleLimit {
            owner: layer.tenant_id,
            mode: contribution.mode,
            limit: &contribution.value,
        });
    }
    // An ancestor that marks the family `private` contributes nothing, so the
    // routing target's own limit is then the only participant — and a routing
    // target with no `rate_limit` at all resolves to no limit rather than to
    // the ancestor's.
    if let Some(own) = local.own.rate_limit {
        visible.push(VisibleLimit {
            owner: local.tenant_id,
            mode: local.modes.mode_of(Family::RateLimit),
            limit: own,
        });
    }
    merged_rate_limit(&visible)
}

/// One visible rate limit in a merge, with the tenant that declares it and the
/// mode that made it visible.
struct VisibleLimit<'a> {
    owner: Uuid,
    mode: SharingMode,
    limit: &'a RateLimitConfig,
}

/// The minimum over the visible limits.
fn merged_rate_limit(visible: &[VisibleLimit<'_>]) -> Option<EffectiveRateLimit> {
    let mut winner: Option<(Uuid, &Sustained)> = None;
    for limit in visible {
        let Some(sustained) = limit.limit.sustained.as_ref() else {
            continue;
        };
        let closer = match winner {
            None => true,
            Some((_, held)) => per_common_scale(sustained) < per_common_scale(held),
        };
        if closer {
            winner = Some((limit.owner, sustained));
        }
    }
    let Some((owner, sustained)) = winner else {
        // No visible limit carries a sustained rate, so there is no limit to
        // report.
        return None;
    };
    let capacity = visible
        .iter()
        .filter_map(|limit| {
            limit
                .limit
                .burst
                .as_ref()
                .map(|burst: &Burst| burst.capacity)
        })
        .min();
    // The members no strategy merges are carried unchanged from the closest
    // visible object, which is the routing target's own whenever it carries a
    // `rate_limit` at all.
    let carrier = visible.last()?.limit;
    Some(EffectiveRateLimit {
        owner,
        mode: strictest(visible.iter().map(|limit| limit.mode)),
        rate_limit: RateLimitConfig {
            sharing: carrier.sharing,
            algorithm: carrier.algorithm,
            sustained: Some(sustained.clone()),
            burst: capacity.map(|capacity| Burst { capacity }),
            scope: carrier.scope,
            strategy: carrier.strategy,
            cost: carrier.cost,
        },
    })
}

/// The plugin family of one merged layer.
///
/// The ancestors' items come first and the routing target's own last, so an
/// `enforce` ancestor's items are never removable by a replacement that omits
/// them.
fn merge_plugins(layers: &[MergeLayer<'_>]) -> Option<EffectivePluginChain> {
    let local = layers.last()?;
    let mut items: Vec<String> = Vec::new();
    let mut owners: Vec<Uuid> = Vec::new();
    let mut modes: Vec<SharingMode> = Vec::new();
    for layer in layers {
        let Some(contribution) = &layer.contributed.plugins else {
            continue;
        };
        items.extend(contribution.value.items.iter().cloned());
        owners.push(layer.tenant_id);
        modes.push(contribution.mode);
    }
    let own_items: &[String] = local
        .own
        .plugins
        .map(|plugins| plugins.items.as_slice())
        .unwrap_or(&[]);
    items.extend(own_items.iter().cloned());
    if items.is_empty() && local.own.plugins.is_none() {
        return None;
    }
    if !own_items.is_empty() {
        modes.push(local.modes.mode_of(Family::Plugins));
        // The routing target's own items are in the chain too, so its tenant is
        // a contributor of the result.
        owners.push(local.tenant_id);
    }
    // The nearest items decide the owner: the routing target's own when it
    // contributes items, otherwise the closest contributing ancestor.
    let owner = if own_items.is_empty() {
        owners.last().copied().unwrap_or(local.tenant_id)
    } else {
        local.tenant_id
    };
    Some(EffectivePluginChain {
        owner,
        mode: strictest(modes),
        items,
        // The layers arrive most distant first, which is the order the
        // contributors are recorded in.
        contributors: owners,
    })
}

/// The CORS family of one merged layer.
///
/// `enforce` decides the whole object; `inherit` unions `allowed_origins`
/// only, and never the header rules; `private` leaves the routing target's own
/// object alone.
fn merge_cors(layers: &[MergeLayer<'_>]) -> Option<EffectiveCors> {
    let local = layers.last()?;
    let mut origins: Vec<String> = Vec::new();
    let mut closest: Option<(Uuid, CorsConfig)> = None;
    let mut modes: Vec<SharingMode> = Vec::new();
    for layer in layers {
        let Some(contribution) = &layer.contributed.cors else {
            continue;
        };
        match contribution.mode {
            SharingMode::Enforce => {
                return Some(EffectiveCors {
                    owner: layer.tenant_id,
                    mode: SharingMode::Enforce,
                    cors: contribution.value.clone(),
                });
            }
            SharingMode::Inherit => {
                for origin in &contribution.value.allowed_origins {
                    if !origins.contains(origin) {
                        origins.push(origin.clone());
                    }
                }
                modes.push(SharingMode::Inherit);
                closest = Some((layer.tenant_id, contribution.value.clone()));
            }
            SharingMode::Private => {}
        }
    }
    let Some(own) = local.own.cors else {
        // The routing target carries no CORS object of its own, so the
        // inherited origins are the effective ones and the closest
        // contributing ancestor owns them.
        let (owner, inherited) = closest?;
        return Some(EffectiveCors {
            owner,
            mode: strictest(modes),
            cors: CorsConfig {
                sharing: inherited.sharing,
                enabled: inherited.enabled,
                allowed_origins: origins,
                allowed_methods: inherited.allowed_methods,
                expose_headers: inherited.expose_headers,
                allow_credentials: inherited.allow_credentials,
            },
        });
    };
    // The routing target's own origins join the union in the same root-to-child
    // order the other families apply, so the ancestors' origins come first.
    let mut allowed = origins;
    for origin in &own.allowed_origins {
        if !allowed.contains(origin) {
            allowed.push(origin.clone());
        }
    }
    modes.push(local.modes.mode_of(Family::Cors));
    Some(EffectiveCors {
        owner: local.tenant_id,
        mode: strictest(modes),
        cors: CorsConfig {
            sharing: own.sharing,
            enabled: own.enabled,
            allowed_origins: allowed,
            allowed_methods: own.allowed_methods.clone(),
            expose_headers: own.expose_headers.clone(),
            allow_credentials: own.allow_credentials,
        },
    })
}

/// The tag family of one merged layer: the add-only union, so a descendant
/// adds and can never remove an inherited tag.
fn merge_tags(layers: &[MergeLayer<'_>]) -> EffectiveTagSet {
    let mut tags: Vec<String> = Vec::new();
    let mut contributors: Vec<Uuid> = Vec::new();
    for layer in layers {
        let mut added = false;
        for tag in layer.own.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
                added = true;
            }
        }
        for tag in layer.contributed.tags.iter().flatten() {
            if !tags.contains(tag) {
                tags.push(tag.clone());
                added = true;
            }
        }
        if added {
            contributors.push(layer.tenant_id);
        }
    }
    EffectiveTagSet { tags, contributors }
}

/// The strictest sharing mode among the layers whose value reached a result.
///
/// `enforce` is strictest, `inherit` is the base a permitted descendant may
/// replace, and `private` contributes nothing at all, so a result that carries
/// a value from an `enforce` layer reports `enforce` even when a closer layer
/// put its own value beside it.
pub(crate) fn strictest(modes: impl IntoIterator<Item = SharingMode>) -> SharingMode {
    let mut strictest = SharingMode::Private;
    for mode in modes {
        strictest = match (strictest, mode) {
            (SharingMode::Enforce, _) | (_, SharingMode::Enforce) => SharingMode::Enforce,
            (SharingMode::Inherit, _) | (_, SharingMode::Inherit) => SharingMode::Inherit,
            _ => SharingMode::Private,
        };
    }
    strictest
}

/// Brings one sustained rate to the common scale the comparison needs.
///
/// `min` is stated over values that carry a window of `second`, `minute`,
/// `hour`, or `day`, so `100/second` against `5000/minute` is not decidable
/// without a common unit. The scale is requests per day: multiplying every
/// rate up to the widest declared window compares the rates without the
/// division and the rounding a narrower common scale would need.
fn per_common_scale(sustained: &Sustained) -> u64 {
    const SECONDS_PER_MINUTE: u64 = 60;
    const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
    const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
    match sustained.window {
        Some(Window::Minute) => sustained
            .rate
            .saturating_mul(SECONDS_PER_DAY / SECONDS_PER_MINUTE),
        Some(Window::Hour) => sustained
            .rate
            .saturating_mul(SECONDS_PER_DAY / SECONDS_PER_HOUR),
        Some(Window::Second) => sustained.rate.saturating_mul(SECONDS_PER_DAY),
        Some(Window::Day) | None => sustained.rate,
    }
}
